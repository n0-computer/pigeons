use std::{
    env, panic,
    path::PathBuf,
    str::{self, FromStr},
    time::Duration,
};

use anyhow::{Context, Result};
use iroh::{
    Endpoint, EndpointId, RelayMap, RelayUrl, SecretKey,
    endpoint::{QuicTransportConfig, RelayMode, presets},
    protocol::Router,
};
use iroh_services::ApiSecret;
use tokio::{io, net::TcpListener, sync::watch};
use tracing::warn;

use crate::{
    config::Config,
    protocol::PigeonsProtocol,
    session::run_client,
    ssh::{self, dot_ssh_secret_key},
};

const DEFAULT_RELAY_URLS: [&str; 4] = [
    "https://ee92s2vncvnx9k0vd.euc1.relay.iroh-svc.com/",
    "https://ee92s2vncvnx9k0vd.use1.relay.iroh-svc.com/",
    "https://ee92s2vncvnx9k0vd.usw1.relay.iroh-svc.com/",
    "https://ee92s2vncvnx9k0vd.aps1.relay.iroh-svc.com/",
];

#[derive(Debug)]
pub struct RoostConfig {
    pub ssh_port: u16,
}

impl Default for RoostConfig {
    fn default() -> Self {
        Self { ssh_port: 22 }
    }
}

#[derive(Debug)]
pub struct TunnelBuilder {
    /// optional roost role configuration to expose a local ssh server
    /// through the tunnel
    pub roost: Option<RoostConfig>,
    /// ED25519 key to use to secure tunnel communications, the endpoint ID that
    /// identifies the tunnel is the public half of this keypair
    pub secret_key: SecretKey,
    /// the set of iroh relay urls to use. Empty set will use pigeons' default
    /// relay servers
    pub relay_urls: Vec<RelayUrl>,
    /// iroh services client for telemetry aggregation
    pub isvc_api_secret: Option<ApiSecret>,
    /// The pigeons config loaded from the toml file.
    pub config: Config,
}

impl TunnelBuilder {
    fn new(secret_key: SecretKey, config: Config) -> Result<Self> {
        let isvc_api_secret = iroh_services_api_secret(&config)?;
        Ok(Self {
            roost: None,
            secret_key,
            relay_urls: vec![],
            isvc_api_secret,
            config,
        })
    }

    pub fn api_secret(mut self, secret: Option<ApiSecret>) -> Self {
        self.isvc_api_secret = secret;
        self
    }

    pub async fn build(self) -> Result<Tunnel> {
        tracing::debug!("building tunnel, roost={}", self.roost.is_some());
        let relay_map = if self.relay_urls.is_empty() {
            tracing::debug!("using {} default relay URLs", DEFAULT_RELAY_URLS.len());
            RelayMap::try_from_iter(DEFAULT_RELAY_URLS)
                .context("pigeons contains an invalid default relay URL")?
        } else {
            tracing::debug!("using {} custom relay URLs", self.relay_urls.len());
            self.relay_urls.iter().cloned().collect()
        };

        let builder = Endpoint::builder(presets::N0)
            .secret_key(self.secret_key.clone())
            .relay_mode(RelayMode::Custom(relay_map))
            .transport_config(tunnel_transport_config()?);

        let endpoint = builder.bind().await?;
        tracing::info!("endpoint bound, id={}", endpoint.id());

        let isvc_client = match self.isvc_api_secret {
            Some(secret) => {
                let client = iroh_services::Client::builder(&endpoint)
                    .api_secret(secret)?
                    .build()
                    .await?;
                Some(client)
            }
            None => None,
        };

        let mut router = Router::builder(endpoint.clone());

        if let Some(home) = &self.roost {
            tracing::debug!("roost mode: checking for sshd on port {}", home.ssh_port);
            ssh::ensure_local_ssh_server_exists(home.ssh_port).await?;
            let handler = PigeonsProtocol::new(home.ssh_port);
            router = router.accept(PigeonsProtocol::ALPN, handler);
            tracing::info!(
                "roost accepting connections on ALPN {:?}",
                str::from_utf8(PigeonsProtocol::ALPN)
            );
        }

        let router = router.spawn();
        tracing::debug!("router spawned");

        Ok(Tunnel {
            router,
            isvc_client,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Tunnel {
    router: Router,
    #[allow(dead_code, reason = "held to keep the telemetry client alive")]
    isvc_client: Option<iroh_services::Client>,
}

impl Tunnel {
    pub async fn builder_ephemeral() -> Result<TunnelBuilder> {
        let config = Config::load_or_default().await;
        TunnelBuilder::new(SecretKey::generate(), config)
    }

    pub async fn builder_from_ssh_dir(ssh_dir: PathBuf) -> Result<TunnelBuilder> {
        let secret_key = dot_ssh_secret_key(ssh_dir).await?;
        let config = Config::load_or_default().await;
        let builder = TunnelBuilder::new(secret_key, config)?;
        Ok(builder)
    }

    /// Runs a local listener; every accepted connection gets its own
    /// resumable session, so each running SSH connection survives network
    /// drops independently.
    pub async fn fly(&self, remote: EndpointId, cancel: watch::Receiver<bool>) -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        tracing::info!("fly: listening on {}", listener.local_addr()?);
        let endpoint = self.endpoint();
        loop {
            let (tcp, _addr) = listener.accept().await?;
            tcp.set_nodelay(true).ok();
            let cancel = cancel.clone();
            let endpoint = endpoint.clone();
            tokio::spawn(async move {
                let (read, write) = tcp.into_split();
                if let Err(err) = run_client(
                    &endpoint,
                    remote,
                    PigeonsProtocol::ALPN,
                    read,
                    write,
                    cancel,
                )
                .await
                {
                    tracing::debug!(%err, "fly session ended");
                }
            });
        }
    }

    /// Bridge stdin/stdout directly to a remote roost via iroh.
    /// Designed for use as an SSH ProxyCommand:
    ///   ProxyCommand pigeons fly --stdio <endpoint_id>
    pub async fn fly_stdio(&self, remote: EndpointId, cancel: watch::Receiver<bool>) -> Result<()> {
        run_client(
            self.endpoint(),
            remote,
            PigeonsProtocol::ALPN,
            io::stdin(),
            io::stdout(),
            cancel,
        )
        .await
    }

    pub async fn close(&self) -> Result<()> {
        self.router.shutdown().await.context("shutting down router")
    }

    pub async fn close_after(
        self,
        fut: impl Future<Output = Result<()>> + Send + 'static,
    ) -> Result<()> {
        let ret = tokio::spawn(fut).await;
        if let Err(e) = self.close().await {
            eprintln!("{e:#?}");
        }
        match ret {
            Ok(result) => result,
            Err(e) => match e.try_into_panic() {
                Ok(panic) => panic::resume_unwind(panic),
                Err(e) => Err(e.into()),
            },
        }
    }

    pub fn endpoint(&self) -> &Endpoint {
        self.router.endpoint()
    }
}

/// Idle timeout and keepalive for the tunnel: a dead connection is
/// detected within a minute, and 5-second keepalives stop NATs from
/// expiring an idle tunnel while its sessions are being held.
fn tunnel_transport_config() -> Result<QuicTransportConfig> {
    Ok(QuicTransportConfig::builder()
        .max_idle_timeout(Some(Duration::from_secs(30).try_into()?))
        .keep_alive_interval(Duration::from_secs(5))
        .build())
}

fn iroh_services_api_secret(config: &Config) -> Result<Option<ApiSecret>> {
    if !config.telemetry_enabled.unwrap_or(false) {
        return Ok(None);
    }

    if let Ok(env_secret) = env::var(iroh_services::API_SECRET_ENV_VAR_NAME) {
        match ApiSecret::from_str(&env_secret) {
            Ok(secret) => return Ok(Some(secret)),
            Err(_) => {
                warn!(
                    "{} is defined but not valid",
                    iroh_services::API_SECRET_ENV_VAR_NAME
                );
            }
        }
    }
    // fall back to the embedded API secret, if one exists
    if let Some(build_secret) = option_env!("BUILD_IROH_SERVICES_API_SECRET") {
        match ApiSecret::from_str(build_secret) {
            Ok(secret) => return Ok(Some(secret)),
            Err(_) => {
                warn!("build-embedded API secret is not valid");
            }
        }
    }
    Ok(None)
}
