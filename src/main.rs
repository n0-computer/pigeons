use std::{
    io::{self, IsTerminal},
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
};

use clap::{ArgAction, Args, Parser, Subcommand};
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use iroh_pigeons::{
    Config, RoostConfig, ServiceParams, Tunnel, add_tunnel_host, home_ssh_dir, install_service,
    list_tunnel_hosts, persistent_endpoint_id, publish_telemetry_choice_for_service,
    remove_tunnel_host, resolve_binary_path, restart_service, service_endpoint_id, service_log,
    uninstall_service,
};
use tokio::{
    fs,
    io::{self as async_io, AsyncBufRead, AsyncBufReadExt, BufReader},
    signal,
};

const RELAY_URL_HELP: &str = "use this relay server, replacing the defaults (repeatable)";

/// Kept to what the iroh-services client actually reports: endpoint counters.
const TELEMETRY_PITCH: &str = "\
pigeons can send anonymous metrics to help us develop iroh, the peer-to-peer
network it flies over. They are counts of: relay usage,
hole-punching success, bytes moved. No hostnames, no usernames, no SSH
traffic, and nothing about the machines you connect to.";

/// Derive a route name from an endpoint ID for when `--name` is omitted.
///
/// Truncation counts characters rather than bytes: the ID is unvalidated user
/// input at this point, and slicing it by byte index panics whenever the cut
/// lands inside a multi-byte character.
fn default_route_name(id: &str) -> String {
    let prefix: String = id.chars().take(8).collect();
    format!("pigeon-{prefix}")
}

#[derive(Parser, Debug)]
#[command(
    name = "pigeons",
    about = "carrier pigeons for your SSH connections. no IP addresses, no problem."
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Set up a roost. Accepts incoming pigeons and delivers them to your local sshd
    Roost(RoostArgs),
    /// Send a pigeon to a remote roost, opening a local tunnel for SSH
    Fly(FlyArgs),
    /// Train a pigeon route (add an SSH config entry for a remote roost)
    Add(AddArgs),
    /// See what pigeon routes are configured
    List,
    /// Forget a pigeon route (remove an SSH config entry)
    Remove(RemoveArgs),
    /// Coop management: install or uninstall pigeons as a system service
    Service {
        #[command(subcommand)]
        op: ServiceCmd,
    },
    /// Print the version number
    Version,
    /// Print the paths used for config and other files
    Paths,
    /// Print the endpoint ID for a persistent identity
    EndpointId(EndpointIdArgs),
}

#[derive(Subcommand, Clone, Debug)]
pub enum ServiceCmd {
    /// Build a permanent coop (install as system service)
    Install {
        #[arg(long, default_value = "22")]
        ssh_port: u16,

        #[arg(long, value_name = "URL", help = RELAY_URL_HELP, action = ArgAction::Append)]
        relay_url: Vec<String>,
    },
    /// Tear down the coop (uninstall system service)
    Uninstall,
    /// Restart the running service
    Restart,
    /// Show service status
    Status,
    /// Show service logs
    Log,
}

#[derive(Args, Clone, Debug)]
pub struct RoostArgs {
    /// Which port your local sshd is nesting on
    #[arg(long, default_value = "22")]
    pub ssh_port: u16,

    /// Use a throwaway identity instead of persisting keys
    #[arg(short, long, default_value_t = false)]
    pub ephemeral: bool,

    #[arg(long, value_name = "URL", help = RELAY_URL_HELP, action = ArgAction::Append)]
    pub relay_url: Vec<String>,
}

#[derive(Args, Clone, Debug)]
pub struct FlyArgs {
    /// The public key of the remote roost to fly to
    #[arg()]
    pub public_key: String,

    /// Bridge stdin/stdout instead of binding a local port (for use as SSH ProxyCommand)
    #[arg(long, default_value_t = false)]
    pub stdio: bool,

    /// Directory containing the persistent identity used by this client
    #[arg(long, value_name = "DIR")]
    pub key_dir: Option<PathBuf>,

    /// Direct socket address for the remote endpoint (repeatable)
    #[arg(long, value_name = "ADDR", action = ArgAction::Append)]
    pub direct_address: Vec<SocketAddr>,

    #[arg(long, value_name = "URL", help = RELAY_URL_HELP, action = ArgAction::Append)]
    pub relay_url: Vec<String>,
}

#[derive(Args, Clone, Debug)]
pub struct EndpointIdArgs {
    /// Directory containing the persistent identity
    #[arg(long, value_name = "DIR")]
    pub key_dir: Option<PathBuf>,
}

#[derive(Args, Clone, Debug)]
pub struct AddArgs {
    /// The endpoint ID of the remote roost
    #[arg(long)]
    pub id: String,

    /// A friendly name for this pigeon route (used as SSH Host name)
    #[arg(long)]
    pub name: Option<String>,
}

#[derive(Args, Clone, Debug)]
pub struct RemoveArgs {
    /// The name of the pigeon route to remove
    #[arg()]
    pub name: String,
}

/// Whether this invocation is one we can interrupt with the telemetry question.
/// Only a person setting pigeons up at a terminal should ever see it.
fn should_ask_about_telemetry(cmd: &Cmd) -> bool {
    let setup_command = match cmd {
        // `fly --stdio` is ssh's ProxyCommand: a prompt would corrupt the tunnel.
        Cmd::Fly(args) => !args.stdio,
        Cmd::Roost(_) | Cmd::Add(_) => true,
        Cmd::Service {
            op: ServiceCmd::Install { .. },
        } => true,
        _ => false,
    };

    // An elevated run would write the answer into root's config, and the
    // unelevated half of `service install` has already asked by then.
    setup_command
        && !self_runas::is_elevated()
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
}

/// Reads a yes or no answer from `input`, re-asking until it gets one. An empty
/// line takes `default`; end of input returns `None`, which is not a no.
async fn read_yes_no<R: AsyncBufRead + Unpin>(
    input: &mut R,
    default: bool,
) -> io::Result<Option<bool>> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(Some(default)),
            "y" | "yes" => return Ok(Some(true)),
            "n" | "no" => return Ok(Some(false)),
            _ => eprint!("please answer 'y' or 'n': "),
        }
    }
}

/// Asks about telemetry the first time someone sets pigeons up. Both answers
/// are recorded, which is what makes it a one-time question. Failures are
/// swallowed: a preference we cannot record must not stop the command itself.
async fn ask_about_telemetry_once(cmd: &Cmd) {
    if !should_ask_about_telemetry(cmd) {
        return;
    }
    let Ok(config_path) = Config::config_path() else {
        return;
    };
    // Deliberately not `load_or_default`: a config we could not parse is one we
    // must not overwrite.
    let Ok(mut config) = Config::load_user().await else {
        return;
    };
    if config.telemetry_enabled.is_some() {
        return;
    }

    eprintln!("\n{TELEMETRY_PITCH}\n");
    eprint!("Send anonymous metrics? [y/N] ");
    let enabled = match read_yes_no(&mut BufReader::new(async_io::stdin()), false).await {
        Ok(Some(enabled)) => enabled,
        // No answer: leave the config alone so the next run asks again.
        Ok(None) => {
            eprintln!();
            return;
        }
        Err(err) => {
            tracing::warn!("failed reading telemetry answer: {err:#}");
            return;
        }
    };

    config.telemetry_enabled = Some(enabled);
    if let Err(err) = config.store().await {
        eprintln!("could not save your answer: {err:#}\n");
        return;
    }
    let path = config_path.display();
    if enabled {
        eprintln!("\nThanks! Set telemetry_enabled = false in {path} to turn metrics off.\n");
    } else {
        eprintln!("\nNo metrics will be sent. Set telemetry_enabled = true in {path} to opt in.\n");
    }
}

/// Carries the installing user's telemetry choice over to the service, which
/// reads root's config rather than theirs. A failure here leaves the service
/// opted out, which is not worth failing a successful install over.
async fn report_service_telemetry() {
    let enabled = match publish_telemetry_choice_for_service().await {
        Ok(enabled) => enabled,
        Err(err) => {
            println!("Could not record a telemetry setting for the service: {err:#}");
            false
        }
    };

    let path = Config::system_config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| "the machine-wide config".to_string());
    if enabled {
        println!(
            "Anonymous metrics: on for the service. Set telemetry_enabled = false in {path} to turn them off."
        );
    } else {
        println!(
            "Anonymous metrics: off for the service. Set telemetry_enabled = true in {path} to opt in."
        );
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    ask_about_telemetry_once(&cli.cmd).await;

    match cli.cmd {
        Cmd::Roost(args) => {
            let ssh_dir = home_ssh_dir()?;
            let mut builder = if args.ephemeral {
                Tunnel::builder_ephemeral().await?
            } else {
                Tunnel::builder_from_ssh_dir(ssh_dir).await?
            };
            builder.roost = Some(RoostConfig {
                ssh_port: args.ssh_port,
            });
            for url in &args.relay_url {
                builder.relay_urls.push(
                    RelayUrl::from_str(url)
                        .map_err(|e| anyhow::anyhow!("invalid relay URL '{url}': {e}"))?,
                );
            }
            let tunnel = builder.build().await?;
            tunnel
                .clone()
                .close_after(async move {
                    let id = tunnel.endpoint().id();

                    // If running as root (service mode), publish the endpoint ID
                    // so unprivileged users can read it via 'pigeons status'
                    if self_runas::is_elevated() {
                        let dir = Path::new("/etc/pigeons");
                        fs::create_dir_all(dir).await?;
                        fs::write(dir.join("endpoint_id"), id.to_string().as_bytes()).await?;
                        // world-readable
                        #[cfg(unix)]
                        {
                            use std::{fs::Permissions, os::unix::fs::PermissionsExt};
                            fs::set_permissions(
                                dir.join("endpoint_id"),
                                Permissions::from_mode(0o644),
                            )
                            .await?;
                        }
                    }

                    println!("roost is running! id: {}", id);
                    signal::ctrl_c().await?;
                    Ok(())
                })
                .await
        }
        Cmd::Fly(args) => {
            let mut builder = match args.key_dir {
                Some(key_dir) => Tunnel::builder_from_ssh_dir(key_dir).await?,
                None => Tunnel::builder_ephemeral().await?,
            };
            let relay_urls = parse_relay_urls(&args.relay_url)?;
            builder.relay_urls = relay_urls.clone();
            let tunnel = builder.build().await?;
            tunnel
                .clone()
                .close_after(async move {
                    let remote_id = EndpointId::from_str(&args.public_key)?;
                    let remote = endpoint_addr(remote_id, relay_urls, args.direct_address);

                    if args.stdio {
                        tunnel.fly_stdio(remote).await?;
                    } else {
                        let fut = tunnel.fly(remote);
                        tokio::select! {
                            res = fut => {
                                if let Err(err) = res {
                                    eprintln!("error: {err}");
                                };
                            }
                            _ = signal::ctrl_c() => {
                                println!("shutting down...");
                            }
                        };
                    }

                    Ok(())
                })
                .await
        }
        Cmd::EndpointId(args) => {
            let key_dir = args.key_dir.unwrap_or(home_ssh_dir()?);
            println!("{}", persistent_endpoint_id(key_dir).await?);
            Ok(())
        }
        Cmd::Add(args) => {
            let name = args.name.unwrap_or_else(|| default_route_name(&args.id));
            let name = name.trim();
            if name.is_empty() {
                anyhow::bail!("host name cannot be empty");
            }
            if name.chars().any(|c| c.is_whitespace()) {
                anyhow::bail!("host name '{name}' cannot contain whitespace");
            }
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
            {
                anyhow::bail!(
                    "host name '{name}' contains invalid characters (use letters, digits, hyphens, dots, or underscores)"
                );
            }
            let endpoint_id = EndpointId::from_str(&args.id)?;
            add_tunnel_host(name, &endpoint_id).await?;

            println!("Pigeon route '{name}' added to ~/.ssh/config");
            println!();
            println!("  Fly with: ssh <user>@{name}");
            Ok(())
        }
        Cmd::List => {
            let entries = list_tunnel_hosts().await?;
            if entries.is_empty() {
                println!("No pigeon routes configured.");
            } else {
                println!("Pigeon routes:");
                println!();
                for entry in &entries {
                    println!("  {:<20} {}", entry.name, entry.endpoint_id);
                }
            }
            Ok(())
        }
        Cmd::Remove(args) => {
            remove_tunnel_host(&args.name).await?;
            println!("Pigeon route '{}' removed.", args.name);
            Ok(())
        }
        Cmd::Version => {
            println!("pigeons v{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Cmd::Paths => {
            println!("config: {:?}", Config::config_path()?);
            if let Ok(system_config) = Config::system_config_path() {
                println!("system config: {system_config:?}");
            }
            let ssh_dir = home_ssh_dir()?;
            let pub_key = ssh_dir.join("pigeons_ed25519.pub");
            let priv_key = ssh_dir.join("pigeons_ed25519");
            println!("ssh public key: {pub_key:?}");
            println!("ssh private key: {priv_key:?}");
            Ok(())
        }
        Cmd::Service { op } => {
            match op {
                ServiceCmd::Install {
                    ssh_port,
                    relay_url,
                } => {
                    // Resolve and validate the binary path *before* elevating,
                    // so the user sees any error in their own terminal.
                    let binary_path = resolve_binary_path()?;

                    if !self_runas::is_elevated() {
                        self_runas::admin()?;
                        return Ok(());
                    }

                    install_service(ServiceParams {
                        ssh_port,
                        relay_url,
                        binary_path,
                    })
                    .await?;
                    println!("Pigeons service installed.");
                    report_service_telemetry().await;
                    Ok(())
                }
                ServiceCmd::Uninstall => {
                    if !self_runas::is_elevated() {
                        self_runas::admin()?;
                        return Ok(());
                    }

                    uninstall_service().await?;
                    println!("Pigeons service uninstalled.");
                    Ok(())
                }
                ServiceCmd::Restart => {
                    if !self_runas::is_elevated() {
                        self_runas::admin()?;
                        return Ok(());
                    }

                    restart_service().await?;
                    println!("Pigeons service restarted.");
                    Ok(())
                }
                ServiceCmd::Status => {
                    match service_endpoint_id().await {
                        Some(id) => {
                            println!("Service:       running");
                            println!();
                            println!("  Roost ID: {id}");
                            println!();
                            println!("  Connect with:");
                            println!("    pigeons add --id {id} --name my-roost");
                        }
                        None => {
                            println!("Service:       not installed");
                        }
                    }
                    Ok(())
                }
                ServiceCmd::Log => {
                    service_log()?;
                    Ok(())
                }
            }
        }
    }
}

fn parse_relay_urls(urls: &[String]) -> anyhow::Result<Vec<RelayUrl>> {
    urls.iter()
        .map(|url| {
            RelayUrl::from_str(url).map_err(|e| anyhow::anyhow!("invalid relay URL '{url}': {e}"))
        })
        .collect()
}

fn endpoint_addr(
    id: EndpointId,
    relay_urls: Vec<RelayUrl>,
    direct_addresses: Vec<SocketAddr>,
) -> EndpointAddr {
    relay_urls.into_iter().fold(
        direct_addresses
            .into_iter()
            .fold(EndpointAddr::new(id), |addr, direct| {
                addr.with_ip_addr(direct)
            }),
        |addr, relay| addr.with_relay_url(relay),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_route_name_uses_id_prefix() {
        assert_eq!(
            default_route_name("bb8e1a5661a6dfa9ae2dd978922f30f5"),
            "pigeon-bb8e1a56"
        );
    }

    #[test]
    fn default_route_name_handles_short_ids() {
        assert_eq!(default_route_name("abc"), "pigeon-abc");
        assert_eq!(default_route_name(""), "pigeon-");
    }

    async fn answer(input: &str, default: bool) -> Option<bool> {
        read_yes_no(&mut BufReader::new(input.as_bytes()), default)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn read_yes_no_accepts_both_spellings_in_any_case() {
        assert_eq!(answer("y\n", false).await, Some(true));
        assert_eq!(answer("YES\n", false).await, Some(true));
        assert_eq!(answer("  n  \n", true).await, Some(false));
        assert_eq!(answer("No\n", true).await, Some(false));
    }

    #[tokio::test]
    async fn read_yes_no_takes_the_default_on_a_bare_newline() {
        assert_eq!(answer("\n", true).await, Some(true));
        assert_eq!(answer("\n", false).await, Some(false));
    }

    #[tokio::test]
    async fn read_yes_no_reprompts_until_the_answer_parses() {
        assert_eq!(answer("maybe\nsure\nn\n", true).await, Some(false));
    }

    #[tokio::test]
    async fn read_yes_no_returns_none_at_end_of_input() {
        assert_eq!(answer("", true).await, None);
        assert_eq!(answer("maybe\n", true).await, None);
    }

    #[test]
    fn telemetry_question_skips_ssh_proxy_command() {
        let stdio = Cmd::Fly(FlyArgs {
            public_key: "abc".to_string(),
            stdio: true,
            key_dir: None,
            direct_address: vec![],
            relay_url: vec![],
        });
        assert!(!should_ask_about_telemetry(&stdio));
    }

    /// Reporting commands are not a setup step.
    #[test]
    fn telemetry_question_skips_read_only_commands() {
        assert!(!should_ask_about_telemetry(&Cmd::List));
        assert!(!should_ask_about_telemetry(&Cmd::Version));
        assert!(!should_ask_about_telemetry(&Cmd::Paths));
        assert!(!should_ask_about_telemetry(&Cmd::EndpointId(
            EndpointIdArgs { key_dir: None }
        )));
        assert!(!should_ask_about_telemetry(&Cmd::Service {
            op: ServiceCmd::Status
        }));
    }

    #[tokio::test]
    async fn endpoint_addr_includes_explicit_relay_and_direct_routes() {
        let id = persistent_endpoint_id(tempfile::tempdir().unwrap().path().to_path_buf())
            .await
            .unwrap();
        let relay: RelayUrl = "https://relay.example.com".parse().unwrap();
        let direct: SocketAddr = "192.0.2.1:4242".parse().unwrap();

        let addr = endpoint_addr(id, vec![relay], vec![direct]);

        assert!(addr.addrs.iter().any(|address| address.is_relay()));
        assert!(addr.addrs.iter().any(|address| address.is_ip()));
    }

    /// Regression: the ID is not validated until after the name is derived, so
    /// truncating it by byte index panicked on any multi-byte input.
    #[test]
    fn default_route_name_does_not_split_multibyte_characters() {
        // 9 characters in, 8 characters out — and crucially, no panic.
        assert_eq!(
            default_route_name("日本語テストデータ"),
            "pigeon-日本語テストデー"
        );
    }
}
