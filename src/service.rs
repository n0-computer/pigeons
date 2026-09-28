use std::{
    env,
    future::Future,
    path::{Path, PathBuf},
    process::Command,
};

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

pub async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
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
