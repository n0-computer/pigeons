use tracing::debug;

use crate::{Service, ServiceParams};

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct MacosService;

#[cfg(target_os = "macos")]
impl Service for MacosService {
    async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
        let path = MacosService::init_install_script(service_params)?;
        debug!(path = %path.display(), "running install script");

        let status = std::process::Command::new("sh")
            .arg(&path)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;

        if !status.success() {
            anyhow::bail!("install script failed with exit code: {}", status);
        }

        Ok(())
    }

    async fn info() -> anyhow::Result<()> {
        todo!("service info is not yet supported")
    }

    async fn uninstall() -> anyhow::Result<()> {
        let path = MacosService::init_uninstall_script()?;
        debug!(path = %path.display(), "running uninstall script");

        let status = std::process::Command::new("sh")
            .arg(&path)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;

        if !status.success() {
            anyhow::bail!("uninstall script failed with exit code: {}", status);
        }

        Ok(())
    }

    async fn restart() -> anyhow::Result<()> {
        let status = std::process::Command::new("launchctl")
            .args(["kickstart", "-k", "system/computer.pigeons.daemon"])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()?;

        if !status.success() {
            anyhow::bail!("launchctl kickstart failed with exit code: {}", status);
        }

        Ok(())
    }
}

#[cfg(target_os = "macos")]
impl MacosService {
    const INSTALL_SH_BYTES: &str = include_str!("../../service/install_macos.sh");
    const UNINSTALL_SH_BYTES: &str = include_str!("../../service/uninstall_macos.sh");

    fn init_install_script(service_params: ServiceParams) -> anyhow::Result<std::path::PathBuf> {
        use std::io::Write as _;

        let mut relay_args = String::new();
        for url in &service_params.relay_url {
            relay_args.push_str(&format!(" --relay-url {url}"));
        }

        let mut temp_sh = tempfile::Builder::new()
            .prefix("pigeons_install-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(
            MacosService::INSTALL_SH_BYTES
                .replace("[SSHPORT]", &service_params.ssh_port.to_string())
                .replace("[RELAYARGS]", &relay_args)
                .replace(
                    "[BINARYPATH]",
                    service_params
                        .binary_path
                        .to_str()
                        .ok_or_else(|| anyhow::anyhow!("binary path is not valid UTF-8"))?,
                )
                .as_bytes(),
        )?;
        let sh_path = temp_sh.path().to_path_buf();
        temp_sh.keep()?;

        Ok(sh_path)
    }

    fn init_uninstall_script() -> anyhow::Result<std::path::PathBuf> {
        use std::io::Write as _;

        let mut temp_sh = tempfile::Builder::new()
            .prefix("pigeons_uninstall-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(MacosService::UNINSTALL_SH_BYTES.as_bytes())?;
        let sh_path = temp_sh.path().to_path_buf();
        temp_sh.keep()?;

        Ok(sh_path)
    }
}
