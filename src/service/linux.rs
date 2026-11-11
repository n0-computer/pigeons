use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

use crate::{Service, ServiceParams, service::service_relay_args};

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub(crate) struct LinuxService;

#[cfg(target_os = "linux")]
impl Service for LinuxService {
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
        let status = Command::new("systemctl")
            .args(["restart", "pigeons.service"])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()?;

        if !status.success() {
            anyhow::bail!("systemctl restart failed with exit code: {}", status);
        }

        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl LinuxService {
    const INSTALL_SH_BYTES: &str = include_str!("../../service/install_linux.sh");
    const UNINSTALL_SH_BYTES: &str = include_str!("../../service/uninstall_linux.sh");

    fn init_install_script(service_params: ServiceParams) -> anyhow::Result<PathBuf> {
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
        let sh_path = temp_sh.path().to_path_buf();
        temp_sh.keep()?;

        Ok(sh_path)
    }

    fn init_uninstall_script() -> anyhow::Result<PathBuf> {
        use std::io::Write as _;

        let mut temp_sh = tempfile::Builder::new()
            .prefix("pigeons_uninstall-")
            .suffix(".sh")
            .tempfile_in("/tmp")?;
        temp_sh.write_all(Self::UNINSTALL_SH_BYTES.as_bytes())?;
        let sh_path = temp_sh.path().to_path_buf();
        temp_sh.keep()?;

        Ok(sh_path)
    }
}
