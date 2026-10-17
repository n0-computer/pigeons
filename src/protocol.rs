use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use iroh::{
    EndpointId,
    endpoint::{Connection, RecvStream, SendStream},
    protocol::{AcceptError, ProtocolHandler},
};
use parking_lot::Mutex;
use tokio::{
    net::TcpStream,
    sync::{Notify, Semaphore},
    time::{sleep, timeout},
};

use crate::session::{
    self, Engine, EngineHandle, HANDSHAKE_LEN, Handshake, OP_OPEN, OpenAttachment, PeerError,
    READY_LEN, ST_BACKEND_UNAVAILABLE, ST_CAPACITY, ST_INVALID_STATE, ST_READY, ST_UNKNOWN_SESSION,
    encode_ready, run_attachment,
};

/// Total sessions (live + reserved) a roost will hold.
const SESSION_CAP_TOTAL: usize = 256;
/// Sessions one peer may hold at a time.
const SESSION_CAP_PER_PEER: usize = 16;
/// Concurrent handshakes the roost will process; excess connections are
/// dropped without a response.
const HANDSHAKE_PERMITS: usize = 64;
/// A handshake that stalls beyond this is dead weight; drop the connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Backend sshd connect deadline for a new session.
const BACKEND_TIMEOUT: Duration = Duration::from_secs(10);
/// `shutdown` waits this long for cancelled sessions to retire.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// The framework drops the connection as soon as the accept handler
/// returns; noq has no flush/ack-wait API, so an error reply must stay on
/// the wire this long before the drop or it is lost in flight.
const REPLY_GRACE: Duration = Duration::from_millis(200);

type SessionKey = (EndpointId, [u8; 32]);

/// One retained roost session. The engine owns the sshd socket and all
/// replay state; the session outlives each QUIC attachment and is retired
/// only by a terminal engine signal or roost shutdown.
#[derive(Debug)]
struct Session {
    engine: EngineHandle,
}

/// A registry slot: reserved while its sshd socket is being established,
/// live once the engine is running.
#[derive(Debug)]
struct Entry {
    /// `None` while the session's sshd socket is still being established.
    session: Option<Arc<Session>>,
    /// Wakes peers waiting on this entry's publish or failure.
    changed: Arc<Notify>,
}

impl Entry {
    fn new() -> Self {
        Self {
            session: None,
            changed: Arc::new(Notify::new()),
        }
    }
}

#[derive(Debug, Default)]
struct RegistryInner {
    entries: HashMap<SessionKey, Entry>,
}

/// Retained roost sessions, keyed by `(remote endpoint id, session id)`.
///
/// Client identity is never trusted beyond this key: everything a client
/// claims about its own state arrives in the handshake and is bounds-checked
/// by the engine on attach.
///
/// Waits on entry resolution are race-free by construction: a waiter
/// registers its owned notifier *while holding the registry lock* on a
/// reserved entry, so any subsequent publish or removal (which happens
/// after the lock is released) can never miss the wakeup. `Notify` records
/// every `notify_waiters` call against not-yet-waiting notifiers.
#[derive(Debug)]
struct Registry {
    inner: Arc<Mutex<RegistryInner>>,
    /// Wakes the shutdown loop when any entry's live-session state changes.
    changed: Arc<Notify>,
}

impl Registry {
    fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner::default())),
            changed: Arc::new(Notify::new()),
        }
    }

    fn clone_arc(&self) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(&self.inner),
            changed: Arc::clone(&self.changed),
        })
    }

    /// Resolve `(peer, id)` to a session. With `open`, a missing session is
    /// reserved under the lock, connected to sshd outside it, and published;
    /// concurrent duplicate opens share the single reservation and open one
    /// socket. A resume never creates.
    async fn resolve(
        &self,
        peer: EndpointId,
        id: [u8; 32],
        open: bool,
        ssh_port: u16,
    ) -> Result<Arc<Session>, PeerError> {
        let key = (peer, id);
        loop {
            let (waiter, session) = {
                let mut inner = self.inner.lock();
                let created = !inner.entries.contains_key(&key);
                if created && !open {
                    // A resume may never create a session.
                    return Err(PeerError::UnknownSession);
                }
                let entry = inner.entries.entry(key).or_insert_with(Entry::new);
                if let Some(session) = &entry.session {
                    (None, Some(session.clone()))
                } else if created {
                    let per_peer = inner
                        .entries
                        .keys()
                        .filter(|(peer_id, _)| *peer_id == peer)
                        .count();
                    if inner.entries.len() > SESSION_CAP_TOTAL || per_peer > SESSION_CAP_PER_PEER {
                        inner.entries.remove(&key);
                        self.changed.notify_waiters();
                        return Err(PeerError::Capacity);
                    }
                    (None, None)
                } else {
                    // Someone else reserved it; register the waiter before
                    // releasing the lock so the publish cannot be missed.
                    (Some(entry.changed.clone().notified_owned()), None)
                }
            };
            if let Some(session) = session {
                return Ok(session);
            }
            if let Some(waiter) = waiter {
                waiter.await;
                continue;
            }
            // We are the creator: connect to sshd outside the lock.
            match timeout(
                BACKEND_TIMEOUT,
                TcpStream::connect(format!("127.0.0.1:{ssh_port}")),
            )
            .await
            {
                Ok(Ok(stream)) => {
                    stream.set_nodelay(true).ok();
                    let (read, write) = stream.into_split();
                    let engine = Engine::start(read, write, 1);
                    let session = Arc::new(Session {
                        engine: engine.clone(),
                    });
                    self.publish(&key, session.clone());
                    return Ok(session);
                }
                _ => {
                    self.remove_reserved(&key);
                    return Err(PeerError::BackendUnavailable);
                }
            }
        }
    }

    /// Install `session` under `key` and spawn the supervisor that retires
    /// it when the engine goes terminal.
    fn publish(&self, key: &SessionKey, session: Arc<Session>) {
        let mut inner = self.inner.lock();
        let changed = match inner.entries.get_mut(key) {
            Some(entry) if entry.session.is_none() => {
                entry.session = Some(session.clone());
                entry.changed.clone()
            }
            _ => return,
        };
        drop(inner);
        let registry = self.clone_arc();
        let supervisor_session = session.clone();
        let supervisor_key = *key;
        tokio::spawn(async move {
            wait_terminal(&supervisor_session).await;
            registry.deregister(&supervisor_key, &supervisor_session);
        });
        changed.notify_waiters();
        self.changed.notify_waiters();
    }

    /// Release a reserved entry whose backend connect failed.
    fn remove_reserved(&self, key: &SessionKey) {
        let mut inner = self.inner.lock();
        let changed = match inner.entries.get(key) {
            Some(entry) if entry.session.is_none() => entry.changed.clone(),
            _ => return,
        };
        inner.entries.remove(key);
        drop(inner);
        changed.notify_waiters();
        self.changed.notify_waiters();
    }

    /// Remove the entry, but only while it still holds `session`.
    fn deregister(&self, key: &SessionKey, session: &Arc<Session>) {
        let mut inner = self.inner.lock();
        let still_current = inner
            .entries
            .get(key)
            .and_then(|e| e.session.as_ref())
            .is_some_and(|current| Arc::ptr_eq(current, session));
        if still_current {
            inner.entries.remove(key);
        }
        drop(inner);
        self.changed.notify_waiters();
    }

    /// Cancel every live session and wait until all are retired, or the
    /// grace deadline passes and stragglers are force-cleared.
    async fn shutdown_all(&self) {
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            let (live, waiter) = {
                let inner = self.inner.lock();
                let live: Vec<Arc<Session>> = inner
                    .entries
                    .values()
                    .filter_map(|e| e.session.clone())
                    .collect();
                let waiter = self.changed.clone().notified_owned();
                (live, waiter)
            };
            if live.is_empty() {
                return;
            }
            for session in &live {
                let _ = session.engine.tx.send(session::In::Cancel);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let mut inner = self.inner.lock();
                for entry in inner.entries.values() {
                    entry.changed.notify_waiters();
                }
                inner.entries.clear();
                self.changed.notify_waiters();
                return;
            }
            let _ = timeout(remaining, waiter).await;
        }
    }
}

/// Wait until `session`'s engine reaches its terminal state.
async fn wait_terminal(session: &Session) {
    let mut terminal = session.engine.terminal.clone();
    if terminal.borrow_and_update().is_none() {
        let _ = terminal.changed().await;
    }
}

#[derive(Debug)]
pub(crate) struct PigeonsProtocol {
    registry: Arc<Registry>,
    /// Bounded, non-blocking handshake slots.
    permits: Arc<Semaphore>,
    ssh_port: u16,
}

impl PigeonsProtocol {
    pub(crate) const ALPN: &[u8] = b"/pigeons/1";

    /// Create a roost handler that exposes the local ssh server on `ssh_port`
    /// to incoming sessions.
    pub(crate) fn new(ssh_port: u16) -> Self {
        Self {
            registry: Arc::new(Registry::new()),
            permits: Arc::new(Semaphore::const_new(HANDSHAKE_PERMITS)),
            ssh_port,
        }
    }
}

impl ProtocolHandler for PigeonsProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let peer = connection.remote_id();
        // Hold the permit until the attachment ends so a stalled handshake
        // or a slow session cannot pile up unbounded work.
        let Ok(_permit) = self.permits.try_acquire() else {
            tracing::debug!("roost: handshake capacity reached, dropping connection from {peer}");
            return Ok(());
        };
        let streams = match connection.accept_bi().await {
            Ok(streams) => streams,
            Err(e) => {
                tracing::debug!(%peer, "roost: failed to accept stream: {e}");
                return Ok(());
            }
        };
        self.handle_stream(peer, streams).await;
        Ok(())
    }

    async fn shutdown(&self) {
        self.registry.shutdown_all().await;
    }
}

impl PigeonsProtocol {
    /// Handle one accepted session stream: handshake, resolve the session,
    /// reply, then drive the attachment until the stream or session dies.
    /// The session (and its engine) persists after this returns; a later
    /// QUIC reconnect resumes it.
    async fn handle_stream(
        &self,
        peer: EndpointId,
        (mut send, mut recv): (SendStream, RecvStream),
    ) {
        let mut hs_buf = [0u8; HANDSHAKE_LEN];
        if timeout(HANDSHAKE_TIMEOUT, recv.read_exact(&mut hs_buf))
            .await
            .is_err()
        {
            tracing::debug!(%peer, "roost: handshake read failed or timed out");
            return;
        }
        let handshake = match Handshake::decode(&hs_buf) {
            Ok(handshake) if handshake.validate().is_ok() => handshake,
            Ok(_) | Err(_) => {
                tracing::debug!(%peer, "roost: malformed handshake");
                return;
            }
        };

        let session = match self
            .registry
            .resolve(
                peer,
                handshake.session_id,
                handshake.op == OP_OPEN,
                self.ssh_port,
            )
            .await
        {
            Ok(session) => session,
            Err(peer_err) => {
                tracing::debug!(%peer, "roost: session rejected: {peer_err:?}");
                self.reply(&mut send, status_of(&peer_err), 0, false).await;
                // Let the status byte leave the wire before accept() returns
                // and the framework drops the connection.
                sleep(REPLY_GRACE).await;
                return;
            }
        };

        let recv_state = {
            let mut state = session.engine.state.clone();
            *state.borrow_and_update()
        };
        self.reply(
            &mut send,
            ST_READY,
            recv_state.delivered,
            recv_state.peer_fin,
        )
        .await;

        tracing::info!(
            %peer,
            session = %to_hex(&handshake.session_id),
            generation = handshake.generation,
            open = handshake.op == OP_OPEN,
            "roost: session attached"
        );
        let end = run_attachment(
            session.engine.clone(),
            OpenAttachment {
                send,
                recv,
                received_offset: handshake.received_offset,
                received_fin: handshake.received_fin,
            },
            handshake.generation,
        )
        .await;
        tracing::debug!(%peer, ?end, "roost: session attachment ended");
    }

    /// Write the 10-byte roost handshake response: a status byte, the
    /// delivered offset of the client's outbound direction, and its FIN.
    async fn reply(
        &self,
        send: &mut SendStream,
        status: u8,
        received_offset: u64,
        received_fin: bool,
    ) {
        let mut buf = Vec::with_capacity(READY_LEN);
        encode_ready(status, received_offset, received_fin, &mut buf);
        if send.write_all(&buf).await.is_err() {
            tracing::debug!("roost: failed to write handshake response");
        }
    }
}

fn status_of(peer_err: &PeerError) -> u8 {
    match peer_err {
        PeerError::UnknownSession => ST_UNKNOWN_SESSION,
        PeerError::Capacity => ST_CAPACITY,
        PeerError::BackendUnavailable => ST_BACKEND_UNAVAILABLE,
        PeerError::InvalidState => ST_INVALID_STATE,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer() -> EndpointId {
        iroh::SecretKey::generate().public()
    }

    #[test]
    fn status_of_maps_every_rejection() {
        assert_eq!(status_of(&PeerError::UnknownSession), ST_UNKNOWN_SESSION);
        assert_eq!(status_of(&PeerError::Capacity), ST_CAPACITY);
        assert_eq!(
            status_of(&PeerError::BackendUnavailable),
            ST_BACKEND_UNAVAILABLE
        );
        assert_eq!(status_of(&PeerError::InvalidState), ST_INVALID_STATE);
    }

    #[tokio::test]
    async fn resume_never_creates_a_session() {
        let registry = Registry::new();
        let id = [7u8; 32];
        // No session exists for this peer: a resume must not create one and
        // must not leave a reservation behind.
        let err = registry
            .resolve(peer(), id, false, 12777)
            .await
            .unwrap_err();
        assert_eq!(err, PeerError::UnknownSession);
        assert!(registry.inner.lock().entries.is_empty());
    }
}
