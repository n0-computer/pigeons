use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use tokio::net::TcpStream;
use tracing::{debug, error, info};

use crate::tunnel::bridge_streams;

#[derive(Debug)]
pub(crate) struct PigeonsProtocol {
    ssh_port: u16,
}

impl PigeonsProtocol {
    pub(crate) const ALPN: &[u8] = b"/pigeons/0";

    /// create a new pigeons "home" that will forward incoming connections from
    /// a bound endpoint to the given local ssh server port
    pub(crate) fn new(ssh_port: u16) -> Self {
        Self { ssh_port }
    }
}

impl ProtocolHandler for PigeonsProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint_id = connection.remote_id();

        let (mut iroh_send, mut iroh_recv) = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(e) => {
                error!("pigeon dropped its message: {e}");
                return Ok(());
            }
        };
        info!("pigeon arrived from {endpoint_id}");

        let ssh_stream = match TcpStream::connect(format!("127.0.0.1:{}", self.ssh_port)).await {
            Ok(ssh_stream) => ssh_stream,
            Err(e) => {
                error!("pigeon couldn't reach sshd: {e}");
                return Ok(());
            }
        };
        info!("delivering to local sshd on port {}", self.ssh_port);
        ssh_stream.set_nodelay(true).ok();

        let (mut local_read, mut local_write) = ssh_stream.into_split();
        if let Err(err) = bridge_streams(
            &mut local_read,
            &mut local_write,
            &mut iroh_send,
            &mut iroh_recv,
        )
        .await
        {
            debug!("pigeon from {endpoint_id}: stream ended with error: {err}");
        }
        info!("pigeon from {endpoint_id} returned home");

        // The client closes the connection once it has read everything.
        // Returning earlier drops the connection, which closes it and discards
        // data the client has not received yet. A client that disappears is
        // caught by the connection's idle timeout.
        connection.closed().await;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use iroh::{
        Endpoint, RelayMode,
        endpoint::{RecvStream, SendStream, presets},
        protocol::Router,
    };
    use tokio::{
        io::{self, AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::sleep,
    };

    use super::*;
    use crate::tunnel::fly_session;

    const BANNER: &[u8] = b"SSH-2.0-test\r\n";

    /// How long a test waits for a session to end before it fails.
    const SESSION_TIMEOUT: Duration = Duration::from_secs(5);

    /// Checks that the roost delivers all of sshd's reply when sshd closes first.
    ///
    /// The roost used to drop the connection as soon as sshd closed its socket.
    /// That closed the QUIC connection and discarded the stream data the client
    /// had not acknowledged yet.
    #[tokio::test]
    async fn roost_delivers_all_data_when_sshd_closes_first() {
        const PAYLOAD_LEN: usize = 4 * 1024 * 1024;

        // A stand-in for sshd: reads the client banner, sends a large reply and
        // closes right away.
        let ssh_port = spawn_fake_sshd(|mut socket| async move {
            let mut banner = [0u8; BANNER.len()];
            socket.read_exact(&mut banner).await.unwrap();
            socket.write_all(&vec![7u8; PAYLOAD_LEN]).await.unwrap();
        })
        .await;

        let (roost, client) = start_roost_and_client(ssh_port).await;
        let (conn, _send, mut recv) = open_session(&client, &roost).await;

        let received = recv.read_to_end(PAYLOAD_LEN + 1).await.unwrap();
        assert_eq!(received.len(), PAYLOAD_LEN);

        shut_down(conn, client, roost).await;
    }

    /// Checks that a reset sshd connection reaches the client as an error.
    ///
    /// Finishing the stream instead would make a truncated reply look complete.
    #[tokio::test]
    async fn roost_reports_a_reset_sshd_connection_as_error() {
        let ssh_port = spawn_resetting_sshd().await;
        let (roost, client) = start_roost_and_client(ssh_port).await;
        let (conn, _send, mut recv) = open_session(&client, &roost).await;

        let result = recv.read_to_end(1024 * 1024).await;

        assert!(
            result.is_err(),
            "a reset looked like a clean end: {result:?}"
        );
        shut_down(conn, client, roost).await;
    }

    /// Checks that a client session ends at once when the roost resets the stream.
    ///
    /// The client used to wait for the roost to acknowledge its reset stream.
    /// noq never wakes that wait, so the session hung until a timeout.
    #[tokio::test]
    async fn fly_session_ends_promptly_after_a_reset() {
        let ssh_port = spawn_resetting_sshd().await;
        let (roost, client) = start_roost_and_client(ssh_port).await;
        let conn = connect(&client, &roost).await;
        // The other half stands in for the local ssh client. It stays open, so
        // only the reset can end the session.
        let (local, mut ssh_client) = io::duplex(64 * 1024);
        ssh_client.write_all(BANNER).await.unwrap();
        let (mut local_read, mut local_write) = io::split(local);

        let result = within_timeout(fly_session(&conn, &mut local_read, &mut local_write)).await;

        assert!(result.is_err(), "a reset looked like a clean end");
        shut_down(conn, client, roost).await;
    }

    /// Checks that a session ends when sshd closes while the client still uploads.
    ///
    /// The client waits until the roost has acknowledged all its data, but the
    /// roost no longer reads it. This works because noq accepts only as much data
    /// as the roost's flow control allows, and the roost acknowledges that data
    /// without reading it.
    #[tokio::test]
    async fn fly_session_ends_when_sshd_closes_during_an_upload() {
        let ssh_port = spawn_fake_sshd(|mut socket| async move {
            let mut banner = [0u8; BANNER.len()];
            socket.read_exact(&mut banner).await.unwrap();
            socket.write_all(b"bye").await.unwrap();
            socket.shutdown().await.unwrap();
            // Keep reading, so that the kernel does not reset the connection.
            io::copy(&mut socket, &mut io::sink()).await.ok();
        })
        .await;
        let (roost, client) = start_roost_and_client(ssh_port).await;
        let conn = connect(&client, &roost).await;
        let mut local_read = BANNER.chain(io::repeat(0));
        let mut local_write = io::sink();

        within_timeout(fly_session(&conn, &mut local_read, &mut local_write))
            .await
            .unwrap();

        shut_down(conn, client, roost).await;
    }

    /// Runs `fut` and panics if it does not complete within [`SESSION_TIMEOUT`].
    ///
    /// Unlike `tokio::time::timeout`, this does not poll `fut` once more when the
    /// time is up. That last poll would complete a future that missed its wakeup,
    /// and the test would not see the hang.
    async fn within_timeout<T>(fut: impl Future<Output = T>) -> T {
        tokio::select! {
            biased;
            () = sleep(SESSION_TIMEOUT) => panic!("did not complete within {SESSION_TIMEOUT:?}"),
            output = fut => output,
        }
    }

    /// Binds an endpoint that only talks over loopback, without relays or address lookup.
    async fn local_endpoint() -> Endpoint {
        Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind_addr("127.0.0.1:0")
            .expect("valid bind addr")
            .bind()
            .await
            .expect("bind endpoint")
    }

    /// Starts a TCP listener that handles one connection with `handler`.
    async fn spawn_fake_sshd<F, Fut>(handler: F) -> u16
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            handler(socket).await;
        });
        port
    }

    /// Starts a fake sshd that sends part of a reply and then resets the connection.
    async fn spawn_resetting_sshd() -> u16 {
        spawn_fake_sshd(|mut socket| async move {
            let mut banner = [0u8; BANNER.len()];
            socket.read_exact(&mut banner).await.unwrap();
            socket.write_all(&[7u8; 1024]).await.unwrap();
            // With a zero linger time, closing sends a reset instead of a FIN.
            socket.set_zero_linger().unwrap();
        })
        .await
    }

    /// Starts a roost that forwards to `ssh_port`, and a client endpoint.
    async fn start_roost_and_client(ssh_port: u16) -> (Router, Endpoint) {
        let roost = Router::builder(local_endpoint().await)
            .accept(PigeonsProtocol::ALPN, PigeonsProtocol::new(ssh_port))
            .spawn();
        (roost, local_endpoint().await)
    }

    /// Connects the client endpoint to the roost.
    async fn connect(client: &Endpoint, roost: &Router) -> Connection {
        client
            .connect(roost.endpoint().addr(), PigeonsProtocol::ALPN)
            .await
            .unwrap()
    }

    /// Connects to the roost and sends the client banner, as ssh does.
    ///
    /// The caller must keep the send stream. Dropping it finishes the stream,
    /// which ends the session.
    async fn open_session(
        client: &Endpoint,
        roost: &Router,
    ) -> (Connection, SendStream, RecvStream) {
        let conn = connect(client, roost).await;
        let (mut send, recv) = conn.open_bi().await.unwrap();
        // The roost only sees the stream once the client sends something on it.
        send.write_all(BANNER).await.unwrap();
        (conn, send, recv)
    }

    /// Closes the client connection and shuts down both endpoints.
    async fn shut_down(conn: Connection, client: Endpoint, roost: Router) {
        conn.close(0u32.into(), b"done");
        client.close().await;
        roost.shutdown().await.unwrap();
    }
}
