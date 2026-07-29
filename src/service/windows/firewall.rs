use std::{path::Path, process::Command};

#[cfg(target_os = "windows")]
use anyhow::{Context, Result, bail};
use tracing::{info, warn};

pub fn add_firewall_rules(executable_path: &Path) -> Result<()> {
    let exe_path = executable_path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("executable path contains invalid UTF-8"))?;

    info!(path = exe_path, "Adding Windows Firewall rules");

    let ps_script = format!(
        r#"
$ErrorActionPreference = 'Stop'

# Remove old rules if they exist (ignore errors)
Remove-NetFirewallRule -DisplayName 'pigeons Service Outbound' -ErrorAction SilentlyContinue
Remove-NetFirewallRule -DisplayName 'pigeons Service Inbound' -ErrorAction SilentlyContinue

# Add outbound UDP rule for relay connections and STUN
New-NetFirewallRule `
    -DisplayName 'pigeons Service Outbound' `
    -Description 'Allow outbound UDP for pigeons QUIC, relay, and holepunching' `
    -Direction Outbound `
    -Action Allow `
    -Protocol UDP `
    -Program '{}' `
    -Profile Any `
    -Enabled True | Out-Null

Write-Host 'Added outbound rule'

# Add inbound UDP rule for accepting holepunched connections
New-NetFirewallRule `
    -DisplayName 'pigeons Service Inbound' `
    -Description 'Allow inbound UDP for pigeons holepunching and direct connections' `
    -Direction Inbound `
    -Action Allow `
    -Protocol UDP `
    -Program '{}' `
    -Profile Any `
    -Enabled True | Out-Null

Write-Host 'Added inbound rule'

# Also add outbound TCP rule for HTTPS relay connections
New-NetFirewallRule `
    -DisplayName 'pigeons Service HTTPS' `
    -Description 'Allow outbound HTTPS for pigeons relay server connections' `
    -Direction Outbound `
    -Action Allow `
    -Protocol TCP `
    -Program '{}' `
    -RemotePort 443 `
    -Profile Any `
    -Enabled True | Out-Null

Write-Host 'Added HTTPS rule'
"#,
        exe_path, exe_path, exe_path
    );

    let output = Command::new("powershell")
        .args(&[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &ps_script,
        ])
        .output()
        .context("Failed to execute PowerShell to add firewall rules")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        bail!(
            "PowerShell failed to add firewall rules.\nStdout: {}\nStderr: {}",
            stdout,
            stderr
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    info!(output = %stdout, "Firewall rules added successfully");

    Ok(())
}

pub fn remove_firewall_rules() -> Result<()> {
    info!("Removing Windows Firewall rules for pigeons");

    let ps_script = r#"
$ErrorActionPreference = 'Stop'

Remove-NetFirewallRule -DisplayName 'pigeons Service Outbound' -ErrorAction Stop
Write-Host 'Removed outbound rule'

Remove-NetFirewallRule -DisplayName 'pigeons Service Inbound' -ErrorAction Stop
Write-Host 'Removed inbound rule'

Remove-NetFirewallRule -DisplayName 'pigeons Service HTTPS' -ErrorAction Stop
Write-Host 'Removed HTTPS rule'
"#;

    let output = Command::new("powershell")
        .args(&[
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            ps_script,
        ])
        .output()
        .context("Failed to execute PowerShell to remove firewall rules")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Don't fail on cleanup - just log the error
        warn!(stdout = %stdout, stderr = %stderr, "Failed to remove some firewall rules (may not exist)");
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        info!(output = %stdout, "Firewall rules removed successfully");
    }

    Ok(())
}
