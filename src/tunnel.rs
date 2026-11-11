use std::{
    env, panic,
    path::PathBuf,
    str::{self, FromStr},
};

use anyhow::{Context, Result, anyhow};
use iroh::{
    Endpoint, EndpointId, RelayMap, RelayUrl, SecretKey,
    address_lookup::{DnsAddressLookup, PkarrResolver},
    endpoint::{Connection, RecvStream, RelayMode, SendStream, presets},
    protocol::Router,
};
use iroh_services::ApiSecret;
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tracing::{debug, warn};

use crate::{
    config::Config,
    protocol::PigeonsProtocol,
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

        let builder = if self.roost.is_some() {
            // Clients find a roost by its endpoint ID, so a roost publishes its
            // addresses.
            Endpoint::builder(presets::N0)
        } else {
            // A flying pigeon only dials out, and its ID is thrown away on exit.
            // It resolves the roost's addresses but publishes none of its own.
            Endpoint::builder(presets::Minimal)
                .address_lookup(PkarrResolver::n0_dns())
                .address_lookup(DnsAddressLookup::n0_dns())
        }
        .secret_key(self.secret_key.clone())
        .relay_mode(RelayMode::Custom(relay_map));

        let endpoint = builder.bind().await?;
        tracing::info!("endpoint bound, id={}", endpoint.id());

        let isvc_client = match self.isvc_api_secret {
            Some(secret) => {
                let client = iroh_services::Client::builder(&endpoint)
                    .api_secret(secret)
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

    /// Forwards connections to a local port to the roost `remote`.
    ///
    /// Binds a free port on 127.0.0.1 and prints it, so you can connect with ssh.
    pub async fn fly(&self, remote: EndpointId) -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        tracing::info!("fly: listening on 127.0.0.1:{port}");
        println!("pigeon is ready on 127.0.0.1:{port}");
        println!();
        println!("  Fly with: ssh -p {port} <user>@127.0.0.1");
        prepare_pigeon(self.endpoint().clone(), listener, remote).await
    }

    /// Bridge stdin/stdout directly to a remote roost via iroh.
    /// Designed for use as an SSH ProxyCommand:
    ///   ProxyCommand pigeons fly --stdio <endpoint_id>
    pub async fn fly_stdio(&self, remote: EndpointId) -> Result<()> {
        tracing::debug!("fly_stdio: connecting to {remote}");
        let conn = self
            .endpoint()
            .connect(remote, PigeonsProtocol::ALPN)
            .await?;
        tracing::debug!("fly_stdio: connected, bridging stdin/stdout");
        fly_session(&conn, &mut io::stdin(), &mut io::stdout()).await
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
    remote: EndpointId,
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((tcp_stream, peer_addr)) => {
                tracing::info!("pigeon departing from {peer_addr}");
                let endpoint = endpoint.clone();
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
    remote_id: EndpointId,
) -> anyhow::Result<()> {
    tcp_stream.set_nodelay(true)?;
    tracing::debug!("bridge_connection: connecting to {remote_id}");
    let conn = endpoint.connect(remote_id, PigeonsProtocol::ALPN).await?;
    tracing::debug!("bridge_connection: connected, bridging the TCP stream");
    let (mut tcp_read, mut tcp_write) = tcp_stream.into_split();
    fly_session(&conn, &mut tcp_read, &mut tcp_write).await
}

/// Runs one ssh session from a local byte stream to the roost over `conn`.
///
/// Opens a stream pair and bridges it with the local stream. After the bridge
/// ends cleanly, waits until the roost has acknowledged all data, then closes
/// `conn`. Closing earlier would discard data still in flight. A dead roost
/// ends the wait through the connection's idle timeout.
pub(crate) async fn fly_session<R, W>(
    conn: &Connection,
    local_read: &mut R,
    local_write: &mut W,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut iroh_send, mut iroh_recv) = conn.open_bi().await?;
    let result = bridge_streams(local_read, local_write, &mut iroh_send, &mut iroh_recv).await;
    // After a reset there is nothing left to deliver. Also, noq does not wake
    // `stopped()` when the peer acknowledges a reset, so the wait would hang.
    if result.is_ok()
        && let Err(err) = iroh_send.stopped().await
    {
        debug!("send stream ended before delivery was confirmed: {err}");
    }
    conn.close(0u32.into(), b"done");
    Ok(result?)
}

/// Copies data between a local byte stream and an iroh stream pair.
///
/// Returns when either direction reaches end of stream or fails. On a clean
/// end, `iroh_send` is finished so the peer reads all data and then a clean end
/// of stream. On failure, `iroh_send` is reset so the peer sees an error rather
/// than a stream that looks complete but is truncated.
///
/// The caller must keep the connection open until the peer has the data.
/// Dropping it closes the connection and discards data still in flight.
pub(crate) async fn bridge_streams<R, W>(
    local_read: &mut R,
    local_write: &mut W,
    iroh_send: &mut SendStream,
    iroh_recv: &mut RecvStream,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let result = tokio::select! {
        result = copy_flush(local_read, iroh_send) => result,
        result = copy_flush(iroh_recv, local_write) => result,
    };
    // Both calls fail only if the stream was already finished or reset, which
    // is harmless here.
    match result {
        Ok(_) => iroh_send.finish().ok(),
        Err(_) => iroh_send.reset(0u32.into()).ok(),
    };
    result.map(|_| ())
}

/// Copy data from reader to writer, flushing after every write.
/// Unlike `tokio::io::copy` (which buffers 8KB before flushing),
/// this ensures interactive data like SSH keystrokes are forwarded
/// immediately.
async fn copy_flush<R, W>(reader: &mut R, writer: &mut W) -> io::Result<u64>
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
