use std::{
    env, io,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Whether to send metrics to iroh-services. `None` means we have not asked.
    pub telemetry_enabled: Option<bool>,
}

impl Config {
    /// Loads the per-user config, taking any key it leaves unset from the
    /// machine-wide one. The service runs as root, where the per-user config is
    /// root's own and almost always absent.
    pub async fn load() -> Result<Self> {
        let user = Self::load_user().await?;
        let Ok(system_path) = Self::system_config_path() else {
            return Ok(user);
        };
        let system = Self::load_from(&system_path).await?;

        Ok(Self {
            telemetry_enabled: user.telemetry_enabled.or(system.telemetry_enabled),
        })
    }

    /// Loads only the per-user config. The first-run question is about this
    /// user's setting, so a machine-wide answer must not stand in for it.
    pub async fn load_user() -> Result<Self> {
        Self::load_from(&Self::config_path()?).await
    }

    /// Read and parse the config at `path`. Loading is read-only: a config that
    /// has not been written yet simply yields the defaults.
    async fn load_from(path: &Path) -> Result<Self> {
        let config_bytes = match fs::read(path).await {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed reading config at {}", path.display()));
            }
        };

        toml::from_slice(&config_bytes)
            .with_context(|| format!("failed parsing config at {}", path.display()))
    }

    /// Loads the config, falling back to the default config on error.
    pub async fn load_or_default() -> Self {
        match Self::load().await {
            Ok(config) => config,
            Err(err) => {
                tracing::error!("failed to load config, using default: {err:#?}");
                Self::default()
            }
        }
    }

    pub async fn store(&self) -> Result<()> {
        self.store_to(&Self::config_path()?).await
    }

    async fn store_to(&self, config_file_path: &Path) -> Result<()> {
        fs::create_dir_all(config_file_path.parent().expect("joined path")).await?;

        let mut file = File::options()
            .write(true)
            .truncate(true)
            .create(true)
            .open(config_file_path)
            .await?;
        file.write_all(toml::to_string(self)?.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Writes the config to `path`, readable by everyone: unprivileged runs
    /// read the machine-wide config, and root's umask does not get a vote.
    async fn store_world_readable(&self, path: &Path) -> Result<()> {
        self.store_to(path).await?;

        #[cfg(unix)]
        {
            use std::{fs::Permissions, os::unix::fs::PermissionsExt};

            let dir = path.parent().expect("joined path");
            fs::set_permissions(dir, Permissions::from_mode(0o755)).await?;
            fs::set_permissions(path, Permissions::from_mode(0o644)).await?;
        }

        Ok(())
    }

    pub fn config_path() -> Result<PathBuf> {
        let config_dir = dirs::config_dir()
            .context("can't figure out config dir on this system")?
            .join("pigeons");
        Ok(config_dir.join("config.toml"))
    }

    /// Path of the machine-wide config, alongside the endpoint ID the roost
    /// publishes there.
    pub fn system_config_path() -> Result<PathBuf> {
        let dir = match env::consts::OS {
            "linux" | "macos" => Path::new("/etc/pigeons"),
            "windows" => Path::new("C:\\ProgramData\\pigeons"),
            other => anyhow::bail!("no machine-wide config location on {other}"),
        };
        Ok(dir.join("config.toml"))
    }

    /// Path of the per-user config under `home`, for reading the config of a
    /// user other than the one running. Assumes the default layout `dirs`
    /// reports, which is all we can know about another account.
    fn config_path_in(home: &Path) -> PathBuf {
        let relative = match env::consts::OS {
            "macos" => "Library/Application Support/pigeons/config.toml",
            "windows" => "AppData/Roaming/pigeons/config.toml",
            _ => ".config/pigeons/config.toml",
        };
        home.join(relative)
    }
}

/// Copies the installing user's telemetry choice into the machine-wide config
/// and reports what the service will do with it. Call this from an elevated
/// `pigeons service install`; the service reads root's config, not theirs.
///
/// # Errors
///
/// Returns an error when the machine-wide config cannot be written.
pub async fn publish_telemetry_choice_for_service() -> Result<bool> {
    let system_path = Config::system_config_path()?;
    let choice = installing_user_telemetry_choice().await;
    publish_telemetry_choice_to(&system_path, choice).await
}

async fn publish_telemetry_choice_to(system_path: &Path, choice: Option<bool>) -> Result<bool> {
    if let Some(enabled) = choice {
        let mut system = Config::load_from(system_path).await.unwrap_or_default();
        system.telemetry_enabled = Some(enabled);
        system.store_world_readable(system_path).await?;
    }

    // With nothing to propagate, an answer already on the machine still stands.
    Ok(Config::load_from(system_path)
        .await
        .unwrap_or_default()
        .telemetry_enabled
        .unwrap_or(false))
}

/// The telemetry choice of the user who started an elevated command.
///
/// `SUDO_USER` is the only pointer an elevated unix process has back to them.
/// Windows elevation keeps the same account, so its own config is the right one.
async fn installing_user_telemetry_choice() -> Option<bool> {
    let path = match env::var("SUDO_USER") {
        Ok(user) => config_path_for_user(&user)?,
        Err(_) => Config::config_path().ok()?,
    };

    Config::load_from(&path).await.ok()?.telemetry_enabled
}

fn config_path_for_user(user: &str) -> Option<PathBuf> {
    let home = homedir::home(user).ok()??;
    Some(Config::config_path_in(&home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config() {
        let config = toml::from_str::<Config>("").unwrap();
        assert_eq!(config.telemetry_enabled, None);
    }

    #[tokio::test]
    async fn publishing_a_choice_writes_the_machine_wide_config() {
        for enabled in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let system = dir.path().join("pigeons").join("config.toml");

            let effective = publish_telemetry_choice_to(&system, Some(enabled))
                .await
                .unwrap();

            assert_eq!(effective, enabled);
            let stored = Config::load_from(&system).await.unwrap();
            assert_eq!(stored.telemetry_enabled, Some(enabled));
        }
    }

    /// Installing from a root shell leaves no `SUDO_USER` to trace back to.
    #[tokio::test]
    async fn publishing_nothing_keeps_the_existing_machine_wide_answer() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("config.toml");
        fs::write(&system, "telemetry_enabled = true")
            .await
            .unwrap();

        let effective = publish_telemetry_choice_to(&system, None).await.unwrap();

        assert!(effective);
    }

    #[tokio::test]
    async fn publishing_nothing_onto_a_bare_machine_leaves_the_service_opted_out() {
        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("config.toml");

        let effective = publish_telemetry_choice_to(&system, None).await.unwrap();

        assert!(!effective);
        assert!(!system.exists(), "nothing to record, nothing to write");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn machine_wide_config_is_world_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let system = dir.path().join("pigeons").join("config.toml");

        publish_telemetry_choice_to(&system, Some(true))
            .await
            .unwrap();

        let mode = fs::metadata(&system).await.unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        let dir_mode = fs::metadata(system.parent().unwrap())
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o755);
    }

    #[test]
    fn config_path_for_user_lands_in_that_users_home() {
        let me = whoami::username().expect("running as some user");

        let path = config_path_for_user(&me).expect("current user has a home directory");

        let home = homedir::my_home().unwrap().unwrap();
        assert!(path.starts_with(&home), "{path:?} is not under {home:?}");
        assert!(path.ends_with("pigeons/config.toml"), "{path:?}");
    }

    #[test]
    fn config_path_in_a_home_matches_the_platform_layout() {
        let path = Config::config_path_in(Path::new("/home/pigeon"));

        let expected = match env::consts::OS {
            "macos" => "/home/pigeon/Library/Application Support/pigeons/config.toml",
            "windows" => "/home/pigeon/AppData/Roaming/pigeons/config.toml",
            _ => "/home/pigeon/.config/pigeons/config.toml",
        };
        assert_eq!(path, Path::new(expected));
    }

    #[tokio::test]
    async fn stored_telemetry_choice_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");

        for enabled in [true, false] {
            let config = Config {
                telemetry_enabled: Some(enabled),
            };
            config.store_to(&path).await.unwrap();

            let loaded = Config::load_from(&path).await.unwrap();
            assert_eq!(loaded.telemetry_enabled, Some(enabled));
        }
    }

    /// Not having written a config yet is the normal case, not an error.
    #[tokio::test]
    async fn load_from_missing_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();

        let config = Config::load_from(&dir.path().join("config.toml"))
            .await
            .unwrap();

        assert_eq!(config.telemetry_enabled, None);
    }

    /// Regression: `load` used to open the file with `create(true)` and no write
    /// access, which fails unconditionally, so settings on disk were silently
    /// discarded in favour of the defaults.
    #[tokio::test]
    async fn load_from_reads_settings_off_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "telemetry_enabled = true").await.unwrap();

        let config = Config::load_from(&path).await.unwrap();

        assert_eq!(
            config.telemetry_enabled,
            Some(true),
            "settings on disk must take precedence over the defaults"
        );
    }
}
