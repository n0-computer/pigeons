use std::{
    env,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
    str::FromStr,
};

use anyhow::{anyhow, bail};
use iroh::RelayUrl;
use tokio::fs;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use crate::service::linux::LinuxService;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use crate::service::macos::MacosService;

// Much of the windows module is invoked by the Windows Service Control Manager
// rather than our own code paths, so the compiler sees it as dead code.
#[cfg(target_os = "windows")]
#[allow(dead_code, reason = "invoked by the Windows Service Control Manager")]
mod windows;

#[cfg(target_os = "windows")]
pub(crate) use crate::service::windows::WindowsService;

#[derive(Debug, Clone)]
pub struct ServiceParams {
    pub ssh_port: u16,
    pub relay_url: Vec<String>,
    pub binary_path: PathBuf,
}

/// Discover the absolute path of the currently-running pigeons binary and
/// validate that it lives in a location suitable for a system daemon.
///
/// On Unix, the binary must be in a standard system path. On Windows, the
/// service installer copies the binary itself, so any path is accepted.
pub fn resolve_binary_path() -> anyhow::Result<PathBuf> {
    let exe = env::current_exe()?;
    let resolved = exe.canonicalize()?;

    #[cfg(unix)]
    {
        const SENSIBLE_PREFIXES: &[&str] = &[
            "/usr/local/bin",
            "/usr/bin",
            "/opt/homebrew/bin",
            "/opt/",
            "/usr/local/sbin",
            "/usr/sbin",
            "/var/usrlocal/bin/", // `/usr/local/bin` is a symlink to this in immutable Fedora distros.
        ];

        let path_str = resolved
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("binary path is not valid UTF-8: {resolved:?}"))?;

        if !SENSIBLE_PREFIXES
            .iter()
            .any(|pfx| path_str.starts_with(pfx))
        {
            anyhow::bail!(
                "pigeons binary is at {path_str}, which doesn't look like a permanent install location.\n\
                 Install pigeons to one of the standard paths ({}) before running service install.",
                SENSIBLE_PREFIXES.join(", ")
            );
        }
    }

    tracing::info!("resolved pigeons binary path: {}", resolved.display());
    Ok(resolved)
}

pub trait Service {
    fn install(service_params: ServiceParams) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn info() -> impl Future<Output = anyhow::Result<()>> + Send;
    fn uninstall() -> impl Future<Output = anyhow::Result<()>> + Send;
    fn restart() -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Checks that relay URLs are valid and can be embedded in a service definition.
///
/// The Linux and macOS install scripts put the URLs in double quotes into a
/// `bash -c` command line inside a systemd unit or a launchd plist. Only
/// characters that need no escaping in any of these layers are allowed.
///
/// # Errors
///
/// Returns an error for the first URL that does not parse or that contains a
/// character outside the allowed set.
pub fn validate_relay_urls(urls: &[String]) -> anyhow::Result<()> {
    for url in urls {
        RelayUrl::from_str(url).map_err(|err| anyhow!("invalid relay URL '{url}': {err}"))?;
        if let Some(c) = url.chars().find(|c| !is_service_safe_url_char(*c)) {
            bail!("relay URL '{url}' contains '{c}', which a service definition cannot hold");
        }
    }
    Ok(())
}

/// Returns true for URL characters that need no escaping in double quotes, systemd or XML.
fn is_service_safe_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "-._~:/?#[]@!*+,=".contains(c)
}

/// Formats relay URLs as `--relay-url` arguments for a `bash -c` command line.
///
/// Each URL is put in double quotes, so the shell takes characters such as `?`
/// and `*` literally. The URLs must have passed [`validate_relay_urls`], which
/// rejects the characters that double quotes do not protect.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn service_relay_args(urls: &[String]) -> String {
    urls.iter()
        .map(|url| format!(" --relay-url \"{url}\""))
        .collect()
}

pub async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
    validate_relay_urls(&service_params.relay_url)?;
    tracing::info!(
        "installing service for os={}, ssh_port={}",
        env::consts::OS,
        service_params.ssh_port
    );
    match env::consts::OS {
        #[cfg(target_os = "linux")]
        "linux" => LinuxService::install(service_params).await,
        #[cfg(target_os = "macos")]
        "macos" => MacosService::install(service_params).await,
        #[cfg(target_os = "windows")]
        "windows" => WindowsService::install(service_params).await,
        _ => anyhow::bail!("service mode is only supported on linux, macos, and windows"),
    }
}

pub async fn uninstall() -> anyhow::Result<()> {
    match env::consts::OS {
        #[cfg(target_os = "linux")]
        "linux" => LinuxService::uninstall().await,
        #[cfg(target_os = "macos")]
        "macos" => MacosService::uninstall().await,
        #[cfg(target_os = "windows")]
        "windows" => WindowsService::uninstall().await,
        _ => anyhow::bail!("service mode is only supported on linux, macos, and windows"),
    }
}

pub async fn restart() -> anyhow::Result<()> {
    match env::consts::OS {
        #[cfg(target_os = "linux")]
        "linux" => LinuxService::restart().await,
        #[cfg(target_os = "macos")]
        "macos" => MacosService::restart().await,
        #[cfg(target_os = "windows")]
        "windows" => WindowsService::restart().await,
        _ => anyhow::bail!("service mode is only supported on linux, macos, and windows"),
    }
}

/// The run state of the pigeons service, as reported by the service manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceRunState {
    /// The service is running.
    Running,
    /// The service is installed but not running.
    NotRunning,
    /// The service manager does not know the service.
    NotInstalled,
    /// The service manager could not be asked, or its answer was not understood.
    Unknown,
}

/// Asks the platform's service manager whether the pigeons service is running.
pub fn service_run_state() -> ServiceRunState {
    let output = match env::consts::OS {
        "linux" => Command::new("systemctl")
            .args([
                "show",
                "--property=LoadState,ActiveState",
                "pigeons.service",
            ])
            .output(),
        "macos" => Command::new("launchctl")
            .args(["print", "system/computer.pigeons.daemon"])
            .output(),
        "windows" => Command::new("sc").args(["query", "pigeons"]).output(),
        _ => return ServiceRunState::Unknown,
    };
    match output {
        Ok(output) => parse_run_state(
            env::consts::OS,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        ),
        Err(err) => {
            tracing::debug!("failed to query the service manager: {err}");
            ServiceRunState::Unknown
        }
    }
}

/// Reads the run state from the output of the service manager query for `os`.
fn parse_run_state(os: &str, stdout: &str, stderr: &str) -> ServiceRunState {
    match os {
        "linux" => {
            // `systemctl show` prints one `Name=value` line per property.
            let property = |name: &str| {
                stdout
                    .lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            };
            match (property("LoadState"), property("ActiveState")) {
                (_, Some("active" | "reloading")) => ServiceRunState::Running,
                (Some("not-found"), _) => ServiceRunState::NotInstalled,
                (Some(_), Some(_)) => ServiceRunState::NotRunning,
                _ => ServiceRunState::Unknown,
            }
        }
        "macos" if stdout.contains("state = running") => ServiceRunState::Running,
        "macos" if stdout.contains("state = ") => ServiceRunState::NotRunning,
        "macos" if stderr.contains("Could not find service") => ServiceRunState::NotInstalled,
        "windows" if stdout.contains("RUNNING") => ServiceRunState::Running,
        "windows" if stdout.contains("STATE") => ServiceRunState::NotRunning,
        // Error 1060: the service does not exist.
        "windows" if stdout.contains("1060") => ServiceRunState::NotInstalled,
        _ => ServiceRunState::Unknown,
    }
}

/// Try to read the endpoint ID of the installed pigeons service.
/// The roost writes the endpoint ID string to /etc/pigeons/endpoint_id
/// on startup when running as root (service mode).
/// Returns Some(endpoint_id) if found, None otherwise.
pub async fn service_endpoint_id() -> Option<iroh::EndpointId> {
    let content = match env::consts::OS {
        "linux" => "/etc/pigeons/endpoint_id",
        "macos" => "/etc/pigeons/endpoint_id",
        "windows" => "C:\\ProgramData\\pigeons\\endpoint_id",
        _ => {
            tracing::warn!(
                "service-level endpoint id is only supported on linux, macos, and windows"
            );
            return None;
        }
    };
    let content = fs::read_to_string(content).await.ok()?;
    content.trim().parse().ok()
}

/// Print service logs to stdout.
pub fn service_log() -> anyhow::Result<()> {
    match env::consts::OS {
        "macos" => {
            let path = Path::new("/var/log/pigeons.log");
            if !path.exists() {
                anyhow::bail!(
                    "no log file found at /var/log/pigeons.log — is the service installed?"
                );
            }
            let status = Command::new("cat").arg(path).status()?;
            if !status.success() {
                anyhow::bail!("failed to read log file (try running with sudo)");
            }
            Ok(())
        }
        "linux" => {
            let status = Command::new("journalctl")
                .args(["-u", "pigeons.service", "--no-pager"])
                .status()?;
            if !status.success() {
                anyhow::bail!("failed to read service journal");
            }
            Ok(())
        }
        other => anyhow::bail!("service log is not supported on {other}"),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn validate(url: &str) -> anyhow::Result<()> {
        validate_relay_urls(&[url.to_string()])
    }

    #[test]
    fn parse_run_state_reads_systemctl() {
        let state = |load, active| {
            parse_run_state(
                "linux",
                &format!("LoadState={load}\nActiveState={active}\n"),
                "",
            )
        };

        assert_eq!(state("loaded", "active"), ServiceRunState::Running);
        assert_eq!(state("loaded", "inactive"), ServiceRunState::NotRunning);
        assert_eq!(state("loaded", "failed"), ServiceRunState::NotRunning);
        assert_eq!(
            state("not-found", "inactive"),
            ServiceRunState::NotInstalled
        );
        assert_eq!(
            parse_run_state("linux", "", "Failed to connect to bus"),
            ServiceRunState::Unknown
        );
    }

    #[test]
    fn parse_run_state_reads_launchctl() {
        let running = "system/computer.pigeons.daemon = {\n\tstate = running\n}";
        let stopped = "system/computer.pigeons.daemon = {\n\tstate = not running\n}";
        let missing = "Could not find service \"computer.pigeons.daemon\" in domain for system";

        assert_eq!(
            parse_run_state("macos", running, ""),
            ServiceRunState::Running
        );
        assert_eq!(
            parse_run_state("macos", stopped, ""),
            ServiceRunState::NotRunning
        );
        assert_eq!(
            parse_run_state("macos", "", missing),
            ServiceRunState::NotInstalled
        );
        assert_eq!(
            parse_run_state("macos", "", "permission denied"),
            ServiceRunState::Unknown
        );
    }

    #[test]
    fn parse_run_state_reads_sc_query() {
        let running = "SERVICE_NAME: pigeons\n        STATE              : 4  RUNNING\n";
        let stopped = "SERVICE_NAME: pigeons\n        STATE              : 1  STOPPED\n";
        let missing = "[SC] EnumQueryServicesStatus:OpenService FAILED 1060:\n";

        assert_eq!(
            parse_run_state("windows", running, ""),
            ServiceRunState::Running
        );
        assert_eq!(
            parse_run_state("windows", stopped, ""),
            ServiceRunState::NotRunning
        );
        assert_eq!(
            parse_run_state("windows", missing, ""),
            ServiceRunState::NotInstalled
        );
    }

    #[test]
    fn validate_relay_urls_accepts_relay_urls() {
        validate("https://ee92s2vncvnx9k0vd.euc1.relay.iroh-svc.com/").unwrap();
        validate("https://relay.example.com:8443/path?x=1").unwrap();
    }

    #[test]
    fn validate_relay_urls_rejects_invalid_urls() {
        let err = validate("not a url").unwrap_err().to_string();

        assert!(err.contains("invalid relay URL"), "got: {err}");
    }

    /// Checks that characters which would break a service definition are rejected.
    ///
    /// A quote or an ampersand would break the unit or the plist. A semicolon
    /// would end the `bash -c` command and start another one.
    #[test]
    fn validate_relay_urls_rejects_characters_the_service_cannot_hold() {
        for url in [
            "https://relay.example.com/?a='b",
            "https://relay.example.com/?a=1&b=2",
            "https://relay.example.com/%41",
            "https://relay.example.com/$HOME",
            "https://relay.example.com/a;reboot",
        ] {
            let err = validate(url).unwrap_err().to_string();
            assert!(err.contains("service definition"), "{url}: {err}");
        }
    }

    /// Checks that relay URLs reach the roost unchanged through `bash -c`.
    ///
    /// The working directory holds a file that the URL matches as a glob pattern.
    /// Unquoted, bash would replace the URL with the file name.
    #[cfg(unix)]
    #[test]
    fn service_relay_args_survive_the_shell() {
        let urls = vec!["https://relay.example.com/*".to_string()];
        validate_relay_urls(&urls).unwrap();
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("https:/relay.example.com")).unwrap();
        fs::write(dir.path().join("https:/relay.example.com/file"), "").unwrap();

        let output = Command::new("bash")
            .arg("-c")
            .arg(format!("printf '%s\\n'{}", service_relay_args(&urls)))
            .current_dir(dir.path())
            .output()
            .unwrap();

        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "--relay-url\nhttps://relay.example.com/*\n"
        );
    }
}
