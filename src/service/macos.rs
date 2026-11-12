use std::process::{Command, Stdio};

use tempfile::TempPath;

use crate::{Service, ServiceParams, service::service_relay_args};

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub(crate) struct MacosService;

#[cfg(target_os = "macos")]
impl Service for MacosService {
    async fn install(service_params: ServiceParams) -> anyhow::Result<()> {
        let path = Self::init_install_script(service_params)?;
        tracing::debug!("running install script: {}", path.display());

        let status = Command::new("sh")
            .arg(&path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;

        if !status.success() {
            anyhow::bail!("install script failed with exit code: {}", status);
        }

        Ok(())
    }

    async fn info() -> anyhow::Result<()> {
        anyhow::bail!("service info is not yet supported")
    }

    async fn uninstall() -> anyhow::Result<()> {
        let path = Self::init_uninstall_script()?;
        tracing::debug!("running uninstall script: {}", path.display());

        let status = Command::new("sh")
            .arg(&path)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;

        if !status.success() {
            anyhow::bail!("uninstall script failed with exit code: {}", status);
        }

        Ok(())
    }

    async fn restart() -> anyhow::Result<()> {
        let status = Command::new("launchctl")
            .args(["kickstart", "-k", "system/computer.pigeons.daemon"])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
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

    fn init_install_script(service_params: ServiceParams) -> anyhow::Result<TempPath> {
        use std::io::Write as _;

        let relay_args = service_relay_args(&service_params.relay_url);

        let mut temp_sh = tempfile::Builder::new()
            .prefix("pigeons_install-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(
            Self::INSTALL_SH_BYTES
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
        // The file is deleted when the returned path is dropped, after the
        // script has run.
        Ok(temp_sh.into_temp_path())
    }

    fn init_uninstall_script() -> anyhow::Result<TempPath> {
        use std::io::Write as _;

        let mut temp_sh = tempfile::Builder::new()
            .prefix("pigeons_uninstall-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(Self::UNINSTALL_SH_BYTES.as_bytes())?;
        // The file is deleted when the returned path is dropped, after the
        // script has run.
        Ok(temp_sh.into_temp_path())
    }
}
