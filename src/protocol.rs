use iroh::{
    endpoint::Connection,
    protocol::{AcceptError, ProtocolHandler},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::TcpStream,
};

use crate::tunnel::copy_both;

#[derive(Debug)]
pub(crate) struct PigeonsProtocol {
    ssh_port: u16,
}

impl PigeonsProtocol {
    pub(crate) const ALPN: &[u8] = b"/pigeons/1";
    pub(crate) const STREAM_PREFACE: [u8; 1] = [0];

    /// create a new pigeons "home" that will forward incoming connections from
    /// a bound endpoint to the given local ssh server port
    pub(crate) fn new(ssh_port: u16) -> Self {
        Self { ssh_port }
    }
}

impl ProtocolHandler for PigeonsProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let endpoint_id = connection.remote_id();

        match connection.accept_bi().await {
            Ok((mut iroh_send, mut iroh_recv)) => {
                tracing::info!("pigeon arrived from {endpoint_id}");
                if let Err(error) = read_stream_preface(&mut iroh_recv).await {
                    tracing::error!("pigeon from {endpoint_id} had an invalid preface: {error}");
                    return Ok(());
                }

                match TcpStream::connect(format!("127.0.0.1:{}", self.ssh_port)).await {
                    Ok(ssh_stream) => {
                        tracing::info!("delivering to local sshd on port {}", self.ssh_port);
                        ssh_stream.set_nodelay(true).ok();

                        let (mut local_read, mut local_write) = ssh_stream.into_split();

                        if let Err(error) = copy_both(
                            &mut local_read,
                            &mut local_write,
                            &mut iroh_recv,
                            &mut iroh_send,
                        )
                        .await
                        {
                            tracing::error!("pigeon from {endpoint_id} failed in transit: {error}");
                        }
                        if let Err(error) = iroh_send.stopped().await {
                            tracing::debug!(
                                "pigeon from {endpoint_id} closed before delivery acknowledgement: {error}"
                            );
                        }
                        tracing::info!("pigeon from {endpoint_id} returned home");
                    }
                    Err(e) => {
                        tracing::error!("pigeon couldn't reach sshd: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::error!("pigeon dropped its message: {e}");
            }
        }

        Ok(())
    }
}

async fn read_stream_preface<R>(reader: &mut R) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut preface = [0; PigeonsProtocol::STREAM_PREFACE.len()];
    reader.read_exact(&mut preface).await?;
    if preface != PigeonsProtocol::STREAM_PREFACE {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid pigeons stream preface",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::{RoostConfig, Tunnel};
    use iroh::RelayUrl;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stream_preface_is_required_and_consumed() {
        let (mut writer, mut reader) = tokio::io::duplex(16);
        writer
            .write_all(&PigeonsProtocol::STREAM_PREFACE)
            .await
            .unwrap();
        writer.write_all(b"SSH-").await.unwrap();
        read_stream_preface(&mut reader).await.unwrap();
        let mut banner = [0; 4];
        reader.read_exact(&mut banner).await.unwrap();
        assert_eq!(&banner, b"SSH-");

        let (mut writer, mut reader) = tokio::io::duplex(1);
        writer.write_all(&[1]).await.unwrap();
        assert_eq!(
            read_stream_preface(&mut reader).await.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[tokio::test]
    async fn clean_remote_close_is_observed_as_stream_eof() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_port = listener.local_addr().unwrap().port();
        let service = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                if socket.write_all(b"SSH-2.0-pigeons-test\r\n").await.is_err() {
                    continue;
                }
                let mut request = Vec::new();
                if socket.read_to_end(&mut request).await.is_err() {
                    continue;
                }
                if request == b"half-close-payload" {
                    socket.write_all(b"DRAINED").await.unwrap();
                    socket.shutdown().await.unwrap();
                    break;
                }
            }
        });

        let relay: RelayUrl = "http://127.0.0.1:9".parse().unwrap();
        let mut server_builder = Tunnel::builder_ephemeral().await.unwrap();
        server_builder.roost = Some(RoostConfig { ssh_port });
        server_builder.relay_urls = vec![relay.clone()];
        let server = server_builder.build().await.unwrap();
        let server_addr = server.endpoint().addr();
        assert!(server_addr.ip_addrs().next().is_some());

        let mut client_builder = Tunnel::builder_ephemeral().await.unwrap();
        client_builder.relay_urls = vec![relay];
        let client = client_builder.build().await.unwrap();
        let connection = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client
                .endpoint()
                .connect(server_addr, PigeonsProtocol::ALPN),
        )
        .await
        .unwrap()
        .unwrap();
        let (mut send, mut receive) = connection.open_bi().await.unwrap();
        send.write_all(&PigeonsProtocol::STREAM_PREFACE)
            .await
            .unwrap();
        send.write_all(b"half-close-payload").await.unwrap();
        send.shutdown().await.unwrap();

        let response =
            tokio::time::timeout(std::time::Duration::from_secs(10), receive.read_to_end(128))
                .await
                .expect("remote response should finish")
                .expect("clean peer close must be stream EOF");
        assert_eq!(response, b"SSH-2.0-pigeons-test\r\nDRAINED");

        service.await.unwrap();
        client.close().await.unwrap();
        server.close().await.unwrap();
    }
}
