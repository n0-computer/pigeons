use std::{
    env, panic,
    path::PathBuf,
    str::{self, FromStr},
};

use anyhow::{Context, Result, anyhow};
use iroh::{
    Endpoint, EndpointAddr, RelayUrl, SecretKey,
    endpoint::{RelayMode, presets},
    protocol::Router,
};
use iroh_services::ApiSecret;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::warn;

use crate::{
    config::Config,
    protocol::PigeonsProtocol,
    ssh::{self, dot_ssh_secret_key},
};

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
    /// the set of iroh relay urls to use. Empty set will default to public
    /// relay servers run by number 0
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
        let mut builder = Endpoint::builder(presets::N0).secret_key(self.secret_key.clone());

        if !self.relay_urls.is_empty() {
            tracing::debug!("using {} custom relay URLs", self.relay_urls.len());
            let relay_map = self.relay_urls.iter().cloned().collect();
            builder = builder.relay_mode(RelayMode::Custom(relay_map));
        }

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

    pub async fn fly(&self, remote: impl Into<EndpointAddr>) -> Result<()> {
        let bind_addr = format!("127.0.0.1:{}", 0);
        let listener = TcpListener::bind(&bind_addr).await?;
        tracing::info!("fly: listening on {}", listener.local_addr()?);
        prepare_pigeon(self.endpoint().clone(), listener, remote.into()).await
    }

    /// Bridge stdin/stdout directly to a remote roost via iroh.
    /// Designed for use as an SSH ProxyCommand:
    ///   ProxyCommand pigeons fly --stdio <endpoint_id>
    pub async fn fly_stdio(&self, remote: impl Into<EndpointAddr>) -> Result<()> {
        let remote = remote.into();
        tracing::debug!("fly_stdio: connecting to {}", remote.id);
        let conn = self
            .endpoint()
            .connect(remote, PigeonsProtocol::ALPN)
            .await?;
        tracing::debug!("fly_stdio: connected, opening bidirectional stream");
        let (mut iroh_send, mut iroh_recv) = conn.open_bi().await?;
        tracing::debug!("fly_stdio: bridging stdin/stdout");

        let mut stdin = io::stdin();
        let mut stdout = io::stdout();

        let stdin_to_iroh = copy_flush(&mut stdin, &mut iroh_send);
        let iroh_to_stdout = copy_flush(&mut iroh_recv, &mut stdout);

        tokio::select! {
            result = stdin_to_iroh => { result?; }
            result = iroh_to_stdout => { result?; }
        }

        Ok(())
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

async fn prepare_pigeon(
    endpoint: Endpoint,
    listener: TcpListener,
    remote: EndpointAddr,
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((tcp_stream, peer_addr)) => {
                tracing::info!("pigeon departing from {peer_addr}");
                let endpoint = endpoint.clone();
                let remote = remote.clone();
                tokio::spawn(async move {
                    if let Err(e) = bridge_connection(tcp_stream, &endpoint, remote).await {
                        tracing::error!("pigeon lost in transit: {e}");
                    }
                });
            }
            Err(err) => {
                tracing::error!("failed to accept connection: {err}");
                return Err(anyhow!(err));
            }
        }
    }
}

/// takes a TCP stream & adds it
async fn bridge_connection(
    tcp_stream: TcpStream,
    endpoint: &Endpoint,
    remote: EndpointAddr,
) -> anyhow::Result<()> {
    tcp_stream.set_nodelay(true)?;
    tracing::debug!("bridge_connection: connecting to {}", remote.id);
    let conn = endpoint.connect(remote, PigeonsProtocol::ALPN).await?;
    tracing::debug!("bridge_connection: connected, opening bi stream");
    let (mut iroh_send, mut iroh_recv) = conn.open_bi().await?;
    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();

    let tcp_to_iroh = copy_flush(&mut tcp_read, &mut iroh_send);
    let iroh_to_tcp = copy_flush(&mut iroh_recv, &mut tcp_write);

    tokio::select! {
        result = tcp_to_iroh => {
            let _ = result;
        }
        result = iroh_to_tcp => {
            let _ = result;
        }
    }

    Ok(())
}

/// Copy data from reader to writer, flushing after every write.
/// Unlike `tokio::io::copy` (which buffers 8KB before flushing),
/// this ensures interactive data like SSH keystrokes are forwarded
/// immediately.
pub(crate) async fn copy_flush<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = [0u8; 8 * 1024];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(total);
        }
        writer.write_all(&buf[..n]).await?;
        writer.flush().await?;
        total += n as u64;
    }
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
