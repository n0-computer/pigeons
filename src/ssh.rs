use std::{
    fmt,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use ed25519_dalek::SECRET_KEY_LENGTH;
use homedir::my_home;
use iroh::{PublicKey, SecretKey};
use tokio::{fs, net::TcpStream};

pub fn home_ssh_dir() -> anyhow::Result<PathBuf> {
    let distro_home = my_home()?.ok_or_else(|| anyhow::anyhow!("home directory not found"))?;
    Ok(distro_home.join(".ssh"))
}

pub async fn dot_ssh_secret_key(ssh_dir: PathBuf) -> anyhow::Result<SecretKey> {
    let pub_key = ssh_dir.join("pigeons_ed25519.pub");
    let priv_key = ssh_dir.join("pigeons_ed25519");

    if !ssh_dir.exists() {
        tracing::info!("creating ssh directory: {}", ssh_dir.display());
        fs::create_dir_all(&ssh_dir).await?;
    }

    if pub_key.exists() && priv_key.exists() {
        tracing::debug!("loading existing keys from {}", ssh_dir.display());
        let secret_key = fs::read(&priv_key)
            .await
            .with_context(|| format!("failed to read secret key from {}", priv_key.display()))?;
        Ok(decode_secret_key(&secret_key)
            .with_context(|| format!("failed to load secret key from {}", priv_key.display()))?)
    } else {
        tracing::info!("generating new keys in {}", ssh_dir.display());
        let secret_key = SecretKey::generate();
        let public_key = secret_key.public();

        fs::write(&pub_key, z32::encode(public_key.as_bytes())).await?;
        write_secret_key(&priv_key, &z32::encode(&secret_key.to_bytes())).await?;

        Ok(secret_key)
    }
}

/// Load or create the persistent identity stored in `ssh_dir` and return its endpoint ID.
pub async fn persistent_endpoint_id(ssh_dir: PathBuf) -> anyhow::Result<PublicKey> {
    Ok(dot_ssh_secret_key(ssh_dir).await?.public())
}

/// Write the secret key, readable only by its owner on unix.
///
/// This key is the roost's identity, so anyone able to read it can impersonate
/// the roost. The mode is set as the file is created rather than afterwards, so
/// there is no window in which the key sits on disk world-readable.
async fn write_secret_key(path: &Path, encoded: &str) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use tokio::io::AsyncWriteExt as _;

        // `mode` is an inherent method on tokio's unix OpenOptions.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .await
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(encoded.as_bytes())
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
        // Dropping a tokio `File` does not flush it: writes are dispatched to a
        // blocking pool and can still be in flight. Without this the key can be
        // read back empty or truncated, and would be lost outright if the
        // process exited here.
        file.sync_all()
            .await
            .with_context(|| format!("failed to flush {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        fs::write(path, encoded)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Decode a z32-encoded secret key.
///
/// The length is checked rather than assumed: a truncated or otherwise corrupt
/// key file is a plausible on-disk state, and copying it into a fixed-size
/// buffer would abort the process instead of reporting the bad file.
fn decode_secret_key(encoded: &[u8]) -> anyhow::Result<SecretKey> {
    let decoded = z32::decode(encoded).context("secret key is not valid z32")?;
    let sk_bytes: [u8; SECRET_KEY_LENGTH] = decoded.as_slice().try_into().map_err(|_| {
        anyhow::anyhow!(
            "secret key is {} bytes, expected {SECRET_KEY_LENGTH}",
            decoded.len()
        )
    })?;
    Ok(SecretKey::from_bytes(&sk_bytes))
}

fn ssh_config_path() -> anyhow::Result<PathBuf> {
    let home = my_home()?.ok_or_else(|| anyhow::anyhow!("home directory not found"))?;
    Ok(home.join(".ssh").join("config"))
}

#[derive(Debug, Clone)]
pub struct SshConfigPigeonEntry {
    pub name: String,
    pub endpoint_id: String,
}

impl fmt::Display for SshConfigPigeonEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} -> {}", self.name, self.endpoint_id)
    }
}

/// Add or update a pigeon host entry in ~/.ssh/config using ProxyCommand
pub async fn add_tunnel_host(name: &str, endpoint_id: &PublicKey) -> anyhow::Result<()> {
    tracing::debug!("adding tunnel host name={name} endpoint={endpoint_id}");
    let config_path = ssh_config_path()?;

    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let existing = if config_path.exists() {
        fs::read_to_string(&config_path)
            .await
            .with_context(|| format!("failed to read {}", config_path.display()))?
    } else {
        String::new()
    };

    // Drop any existing pigeon entry for this name so we replace rather than
    // duplicate it. A name already taken by a host that isn't ours is an error:
    // appending anyway would leave two `Host` blocks, and ssh honours the first,
    // so the new route would be silently ignored.
    let cleaned = remove_host_block(&existing, name)?;

    let block = format!(
        "Host {name}\n\
         \x20   ProxyCommand pigeons fly --stdio {endpoint_id}\n\
         \x20   UserKnownHostsFile /dev/null\n\
         \x20   StrictHostKeyChecking no\n"
    );

    let mut new_content = cleaned;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    if !new_content.is_empty() {
        new_content.push('\n');
    }
    new_content.push_str(&block);

    atomic_write(&config_path, &new_content).await
}

/// Remove a pigeon host entry from ~/.ssh/config
pub async fn remove_tunnel_host(name: &str) -> anyhow::Result<()> {
    tracing::debug!("removing tunnel host name={name}");
    let config_path = ssh_config_path()?;

    if !config_path.exists() {
        bail!("no ssh config found at {}", config_path.display());
    }

    let existing = fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let cleaned = remove_host_block(&existing, name)?;

    if cleaned == existing {
        bail!("no host '{name}' found in ssh config");
    }

    atomic_write(&config_path, &cleaned).await
}

/// List all pigeons-managed entries in ~/.ssh/config by finding Host blocks
/// whose ProxyCommand starts with "pigeons fly"
pub async fn list_tunnel_hosts() -> anyhow::Result<Vec<SshConfigPigeonEntry>> {
    let config_path = ssh_config_path()?;

    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let content = fs::read_to_string(&config_path)
        .await
        .with_context(|| format!("failed to read {}", config_path.display()))?;

    let mut entries = Vec::new();
    let mut current_host: Option<String> = None;
    let mut current_endpoint: Option<String> = None;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("Host ") {
            // Flush previous block if it was a pigeon entry
            if let (Some(host), Some(endpoint)) = (current_host.take(), current_endpoint.take()) {
                entries.push(SshConfigPigeonEntry {
                    name: host,
                    endpoint_id: endpoint,
                });
            }
            current_host = Some(rest.trim().to_string());
            current_endpoint = None;
        } else if let Some(proxy_cmd) = trimmed.strip_prefix("ProxyCommand ")
            && let Some(endpoint_id) = parse_pigeons_proxy_command(proxy_cmd.trim())
        {
            current_endpoint = Some(endpoint_id);
        }
    }

    // Flush last block
    if let (Some(host), Some(endpoint)) = (current_host, current_endpoint) {
        entries.push(SshConfigPigeonEntry {
            name: host,
            endpoint_id: endpoint,
        });
    }

    Ok(entries)
}

/// Parse a ProxyCommand value like "pigeons fly --stdio <endpoint_id>"
/// and return the endpoint_id if it matches
fn parse_pigeons_proxy_command(cmd: &str) -> Option<String> {
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    // expect: ["pigeons", "fly", "--stdio", "<endpoint_id>"]
    if parts.len() >= 4 && parts[0] == "pigeons" && parts[1] == "fly" && parts[2] == "--stdio" {
        Some(parts[3].to_string())
    } else {
        None
    }
}

/// Remove a Host block by name from ssh config content.
/// A Host block starts with "Host <name>" and ends at the next "Host " line
/// or end of file. Returns an error if the block exists but does not contain
/// a ProxyCommand that invokes pigeons.
fn remove_host_block(content: &str, name: &str) -> anyhow::Result<String> {
    let mut result = String::new();
    let mut skipping = false;
    let mut found = false;
    let mut has_pigeons_proxy = false;

    for line in content.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix("Host ") {
            if rest.trim() == name {
                skipping = true;
                found = true;
                has_pigeons_proxy = false;
                continue;
            } else {
                if found && !has_pigeons_proxy {
                    bail!("host '{name}' is not configured to use pigeons");
                }
                skipping = false;
            }
        }

        if skipping {
            if trimmed.starts_with("ProxyCommand") && trimmed.contains("pigeons") {
                has_pigeons_proxy = true;
            }
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    // Check after the last block (no trailing Host line to trigger the check)
    if found && !has_pigeons_proxy {
        bail!("host '{name}' is not configured to use pigeons");
    }

    // Trim trailing blank lines
    while result.ends_with("\n\n") {
        result.pop();
    }

    Ok(result)
}

async fn atomic_write(path: &PathBuf, content: &str) -> anyhow::Result<()> {
    let dir = path.parent().unwrap();
    let temp_path = dir.join(".config.pigeons.tmp");
    fs::write(&temp_path, content)
        .await
        .with_context(|| format!("failed to write {}", temp_path.display()))?;
    fs::rename(&temp_path, path)
        .await
        .with_context(|| format!("failed to rename temp file to {}", path.display()))?;
    Ok(())
}

pub(crate) async fn ensure_local_ssh_server_exists(ssh_port: u16) -> anyhow::Result<()> {
    tracing::debug!("probing sshd on port {ssh_port}");
    match TcpStream::connect(format!("127.0.0.1:{}", ssh_port)).await {
        Ok(_) => {
            tracing::debug!("sshd found on port {ssh_port}");
            Ok(())
        }
        Err(_) => Err(anyhow::anyhow!(format!(
            "no sshd detected on port {ssh_port}. Make sure sshd is running before sending pigeons to this roost",
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_secret_key_round_trips() {
        let key = SecretKey::generate();
        let encoded = z32::encode(&key.to_bytes());

        let decoded = decode_secret_key(encoded.as_bytes()).unwrap();

        assert_eq!(decoded.to_bytes(), key.to_bytes());
    }

    /// Regression: a short key used to be copied into a fixed-size buffer,
    /// which aborted the process instead of reporting the corrupt file.
    #[test]
    fn decode_secret_key_rejects_truncated_key() {
        let key = SecretKey::generate();
        let encoded = z32::encode(&key.to_bytes());
        let truncated = &encoded.as_bytes()[..encoded.len() - 8];

        let err = decode_secret_key(truncated).unwrap_err().to_string();

        assert!(err.contains("expected 32"), "unexpected error: {err}");
    }

    #[test]
    fn decode_secret_key_rejects_garbage() {
        assert!(decode_secret_key(b"not z32 at all!!").is_err());
    }

    #[tokio::test]
    async fn generated_key_is_reloaded_not_regenerated() {
        let dir = tempfile::tempdir().unwrap();

        let generated = dot_ssh_secret_key(dir.path().to_path_buf()).await.unwrap();
        let reloaded = dot_ssh_secret_key(dir.path().to_path_buf()).await.unwrap();

        assert_eq!(generated.to_bytes(), reloaded.to_bytes());
    }

    /// The key is the roost's identity, so anyone who can read it can
    /// impersonate the roost. It must not be group- or world-readable.
    #[cfg(unix)]
    #[tokio::test]
    async fn generated_key_is_owner_readable_only() {
        use std::{fs::metadata, os::unix::fs::PermissionsExt as _};

        let dir = tempfile::tempdir().unwrap();
        dot_ssh_secret_key(dir.path().to_path_buf()).await.unwrap();

        let mode = metadata(dir.path().join("pigeons_ed25519"))
            .unwrap()
            .permissions()
            .mode();

        assert_eq!(mode & 0o777, 0o600, "got mode {:o}", mode & 0o777);
    }

    const PIGEON_BLOCK: &str = "Host my-server\n    ProxyCommand pigeons fly --stdio ABC\n";

    #[test]
    fn remove_host_block_leaves_content_alone_when_host_is_absent() {
        let content = "Host other\n    HostName example.com\n";

        assert_eq!(remove_host_block(content, "my-server").unwrap(), content);
    }

    #[test]
    fn remove_host_block_removes_a_pigeon_entry() {
        assert_eq!(remove_host_block(PIGEON_BLOCK, "my-server").unwrap(), "");
    }

    /// Adding a route reuses this to replace an existing entry. A name already
    /// taken by a host that isn't ours has to be rejected: appending regardless
    /// would leave two `Host` blocks, and ssh honours the first, so the new
    /// route would never take effect.
    #[test]
    fn remove_host_block_rejects_a_host_that_is_not_ours() {
        let content = "Host my-server\n    HostName example.com\n";

        let err = remove_host_block(content, "my-server")
            .unwrap_err()
            .to_string();

        assert!(err.contains("not configured to use pigeons"), "got: {err}");
    }

    #[test]
    fn remove_host_block_keeps_unrelated_hosts() {
        let keep = "Host keep-me\n    HostName example.com\n";

        assert_eq!(
            remove_host_block(&format!("{keep}{PIGEON_BLOCK}"), "my-server").unwrap(),
            keep
        );
        assert_eq!(
            remove_host_block(&format!("{PIGEON_BLOCK}{keep}"), "my-server").unwrap(),
            keep
        );
    }
}
