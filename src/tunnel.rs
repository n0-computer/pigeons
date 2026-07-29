use std::{path::PathBuf, str::FromStr};

use anyhow::{Context, Result, anyhow};
use iroh::{
    Endpoint, EndpointId, RelayUrl, SecretKey,
    endpoint::{RelayMode, presets},
    protocol::Router,
};
use iroh_services::ApiSecret;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tracing::{debug, error, info, warn};

use crate::{
    config::Config,
    protocol::PigeonsProtocol,
    ssh::{self, dot_ssh_secret_key},
};

#[derive(Debug)]
pub struct SshConfig {
    pub ssh_port: u16,
}

impl Default for SshConfig {
    fn default() -> Self {
        Self { ssh_port: 22 }
    }
}

#[derive(Debug)]
pub struct TunnelBuilder {
    /// optional ssh server role configuration to expose a local ssh server
    /// through the tunnel
    pub ssh: Option<SshConfig>,
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
        Ok(TunnelBuilder {
            ssh: None,
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
        debug!(roost = self.ssh.is_some(), "building tunnel");
        let mut builder = Endpoint::builder(presets::N0).secret_key(self.secret_key.clone());

        if !self.relay_urls.is_empty() {
            debug!(
                relay_url_count = self.relay_urls.len(),
                "using custom relay URLs"
            );
            let relay_map = self.relay_urls.iter().cloned().collect();
            builder = builder.relay_mode(RelayMode::Custom(relay_map));
        }

        let endpoint = builder.bind().await?;
        info!(id = %endpoint.id(), "endpoint bound");

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

        if let Some(home) = &self.ssh {
            debug!(port = home.ssh_port, "roost mode: checking for sshd");
            ssh::ensure_local_ssh_server_exists(home.ssh_port).await?;
            let handler = PigeonsProtocol::new(home.ssh_port);
            router = router.accept(PigeonsProtocol::ALPN, handler);
            info!(alpn = ?std::str::from_utf8(PigeonsProtocol::ALPN), "roost accepting connections");
        }

        let router = router.spawn();
        debug!("router spawned");

        Ok(Tunnel {
            router,
            isvc_client,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Tunnel {
    router: Router,
    #[allow(dead_code)]
    isvc_client: Option<iroh_services::Client>,
}

impl Tunnel {
    pub async fn builder_ephemeral() -> Result<TunnelBuilder> {
        let config = Config::load_or_default().await;
        TunnelBuilder::new(SecretKey::generate(), config)
    }

    pub async fn builder_from_ssh_dir(ssh_dir: PathBuf) -> Result<TunnelBuilder> {
        let secret_key = dot_ssh_secret_key(ssh_dir)?;
        let config = Config::load_or_default().await;
        let builder = TunnelBuilder::new(secret_key, config)?;
        Ok(builder)
    }

    pub async fn fly(&self, remote: EndpointId) -> Result<()> {
        let bind_addr = format!("127.0.0.1:{}", 0);
        let listener = TcpListener::bind(&bind_addr).await?;
        info!(addr = %listener.local_addr()?, "fly: listening");
        prepare_pigeon(self.endpoint().clone(), listener, remote).await
    }

    /// Bridge stdin/stdout directly to a remote roost via iroh.
    /// Designed for use as an SSH ProxyCommand:
    ///   ProxyCommand pigeons fly --stdio <endpoint_id>
    pub async fn fly_stdio(&self, remote: EndpointId) -> Result<()> {
        debug!(remote = %remote, "fly_stdio: connecting");
        let conn = self
            .endpoint()
            .connect(remote, PigeonsProtocol::ALPN)
            .await?;
        debug!("fly_stdio: connected, opening bidirectional stream");
        let (mut iroh_send, mut iroh_recv) = conn.open_bi().await?;
        debug!("fly_stdio: bridging stdin/stdout");

        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();

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
                Ok(panic) => std::panic::resume_unwind(panic),
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
    remote: EndpointId,
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((tcp_stream, peer_addr)) => {
                info!(peer_addr = %peer_addr, "pigeon departing");
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    if let Err(e) = bridge_connection(tcp_stream, &endpoint, remote).await {
                        error!(err = %e, "pigeon lost in transit");
                    }
                });
            }
            Err(err) => {
                error!(err = %err, "failed to accept connection");
                return Err(anyhow!(err));
            }
        }
    }
}

/// takes a TCP stream & adds it
async fn bridge_connection(
    tcp_stream: tokio::net::TcpStream,
    endpoint: &Endpoint,
    remote_id: EndpointId,
) -> anyhow::Result<()> {
    tcp_stream.set_nodelay(true)?;
    debug!(remote_id = %remote_id, "bridge_connection: connecting");
    let conn = endpoint.connect(remote_id, PigeonsProtocol::ALPN).await?;
    debug!("bridge_connection: connected, opening bi stream");
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
pub(crate) async fn copy_flush<R, W>(reader: &mut R, writer: &mut W) -> std::io::Result<u64>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
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
    if !config.telemetry_enabled() {
        return Ok(None);
    }

    if let Ok(env_secret) = std::env::var(iroh_services::API_SECRET_ENV_VAR_NAME) {
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
