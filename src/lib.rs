mod config;
mod protocol;
mod service;
mod ssh;
mod tunnel;

pub use config::{Config, publish_telemetry_choice_for_service};
pub use service::{
    Service, ServiceParams, install as install_service, resolve_binary_path,
    restart as restart_service, service_endpoint_id, service_log, uninstall as uninstall_service,
};
pub use ssh::{
    SshConfigPigeonEntry, add_tunnel_host, dot_ssh_secret_key, home_ssh_dir, list_tunnel_hosts,
    persistent_endpoint_id, remove_tunnel_host,
};
pub use tunnel::{RoostConfig, Tunnel, TunnelBuilder};
