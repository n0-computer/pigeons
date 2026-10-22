//! Resumable session transport.
//!
//! A session is a byte stream with absolute offsets and cumulative
//! acknowledgements on top of a replaceable QUIC attachment. When the
//! attachment is lost, the same session id, generation counter, offsets,
//! and retained buffers survive on both sides; the new attachment replays
//! unacknowledged bytes and deduplicates already-delivered ones.
//!
//! Wire protocol (big-endian, strict bounds, one bidirectional QUIC stream
//! per attachment):
//! - Client handshake: op u8 (`0=Open`, `1=Resume`), session id [u8; 32],
//!   generation u64, received offset u64, received FIN u8.
//! - Roost response: status u8 (`0=Ready`, `1=UnknownSession`,
//!   `2=Capacity`, `3=BackendUnavailable`, `4=InvalidState`); Ready adds
//!   received offset u64 and received FIN u8.
//! - Frames both directions: `0=Data` (offset u64, length u32, 1..=16 KiB
//!   payload), `1=Ack` (cumulative delivered offset u64, FIN-ack u8),
//!   `2=Fin` (final offset u64), `3=Reset` (reason u8: 0=Cancelled,
//!   1=LocalIo, 2=InvalidState).

use std::{
    collections::VecDeque,
    fmt,
    io::Error,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use iroh::{
    Endpoint, EndpointAddr, SecretKey,
    endpoint::{ConnectingError, Connection, ConnectionError, RecvStream, SendStream},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, mpsc, watch},
    task::JoinHandle,
    time::{interval, sleep, timeout},
};

/// Maximum retained bytes per direction awaiting acknowledgement.
pub(crate) const REPLAY_CAP: u64 = 256 * 1024;
/// Maximum queued bytes per direction awaiting the local application.
pub(crate) const QUEUE_CAP: u64 = 256 * 1024;
/// Maximum payload of one Data frame.
const FRAME_CAP: u32 = 16 * 1024;
/// Maximum application chunk delivered to the app writer in one go.
const APP_CHUNK: usize = 64 * 1024;

pub(crate) const OP_OPEN: u8 = 0;
pub(crate) const OP_RESUME: u8 = 1;
pub(crate) const ST_READY: u8 = 0;
pub(crate) const ST_UNKNOWN_SESSION: u8 = 1;
pub(crate) const ST_CAPACITY: u8 = 2;
pub(crate) const ST_BACKEND_UNAVAILABLE: u8 = 3;
pub(crate) const ST_INVALID_STATE: u8 = 4;
const FR_DATA: u8 = 0;
const FR_ACK: u8 = 1;
const FR_FIN: u8 = 2;
const FR_RESET: u8 = 3;
const RS_CANCELLED: u8 = 0;
const RS_LOCAL_IO: u8 = 1;
const RS_INVALID_STATE: u8 = 2;

pub(crate) const HANDSHAKE_LEN: usize = 1 + 32 + 8 + 8 + 1;
pub(crate) const READY_LEN: usize = 1 + 8 + 1;

/// A fatal session state: wire-protocol violation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionError;

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("session wire protocol error")
    }
}

/// Error reported by the roost in its handshake response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerError {
    /// No such session on this roost for this endpoint.
    UnknownSession,
    /// The roost is at its session limit.
    Capacity,
    /// The roost could not reach its local sshd.
    BackendUnavailable,
    /// The roost rejected the session state.
    InvalidState,
}

/// Client-to-roost handshake request.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Handshake {
    pub op: u8,
    pub session_id: [u8; 32],
    /// Attempt number; the first attempt uses 0 and it increments on every
    /// (re)attachment.
    pub generation: u64,
    /// Bytes of the roost's outbound direction this side has delivered to
    /// its local application.
    pub received_offset: u64,
    pub received_fin: bool,
}

impl Handshake {
    pub(crate) const LEN: usize = HANDSHAKE_LEN;

    pub(crate) fn encode(&self, buf: &mut Vec<u8>) {
        buf.clear();
        buf.push(self.op);
        buf.extend_from_slice(&self.session_id);
        buf.extend_from_slice(&self.generation.to_be_bytes());
        buf.extend_from_slice(&self.received_offset.to_be_bytes());
        buf.push(u8::from(self.received_fin));
        debug_assert_eq!(buf.len(), HANDSHAKE_LEN);
    }

    pub(crate) fn decode(buf: &[u8]) -> Result<Self, SessionError> {
        let Some(buf) = buf.get(..HANDSHAKE_LEN) else {
            return Err(SessionError);
        };
        let mut session_id = [0u8; 32];
        session_id.copy_from_slice(&buf[1..33]);
        Ok(Self {
            op: buf[0],
            session_id,
            generation: be64(&buf[33..41]),
            received_offset: be64(&buf[41..49]),
            received_fin: fin_flag(buf[49])?,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), SessionError> {
        match self.op {
            OP_OPEN | OP_RESUME => Ok(()),
            _ => Err(SessionError),
        }
    }
}

/// Roost-to-client handshake response.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Ready {
    /// Bytes of the client's outbound direction the roost has delivered to
    /// its local application.
    pub received_offset: u64,
    pub received_fin: bool,
}

impl Ready {
    /// `Ok(Ok(_))` means Ready; `Ok(Err(_))` is a roost rejection.
    pub(crate) fn decode(buf: &[u8]) -> Result<Result<Self, PeerError>, SessionError> {
        let Some(buf) = buf.get(..READY_LEN) else {
            return Err(SessionError);
        };
        let fin = fin_flag(buf[9])?;
        Ok(match buf[0] {
            ST_READY => Ok(Self {
                received_offset: be64(&buf[1..9]),
                received_fin: fin,
            }),
            ST_UNKNOWN_SESSION => Err(PeerError::UnknownSession),
            ST_CAPACITY => Err(PeerError::Capacity),
            ST_BACKEND_UNAVAILABLE => Err(PeerError::BackendUnavailable),
            ST_INVALID_STATE => Err(PeerError::InvalidState),
            _ => return Err(SessionError),
        })
    }
}

/// Encode a roost handshake response: status byte, the delivered offset of
/// the client's outbound direction, and its FIN flag. Error statuses carry
/// a zero offset and no FIN.
pub(crate) fn encode_ready(
    status: u8,
    received_offset: u64,
    received_fin: bool,
    buf: &mut Vec<u8>,
) {
    buf.clear();
    buf.push(status);
    buf.extend_from_slice(&received_offset.to_be_bytes());
    buf.push(u8::from(received_fin));
    debug_assert_eq!(buf.len(), READY_LEN);
}

fn fin_flag(v: u8) -> Result<bool, SessionError> {
    match v {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(SessionError),
    }
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b.try_into().unwrap())
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap())
}

/// One post-handshake wire frame.
#[derive(Debug, PartialEq)]
pub(crate) enum Frame {
    /// `offset` is cumulative; `data` has length 1..=FRAME_CAP.
    Data { offset: u64, data: Vec<u8> },
    /// Cumulative delivered offset; `fin` is true once the peer's Fin has
    /// been delivered to the local application.
    Ack { offset: u64, fin: bool },
    /// The peer's sender-side application reached EOF at `offset`.
    Fin { offset: u64 },
    /// The peer terminated the session.
    Reset { reason: u8 },
}

impl Frame {
    pub(crate) fn encode(&self, buf: &mut Vec<u8>) {
        buf.clear();
        match self {
            Self::Data { offset, data } => {
                debug_assert!((1..=FRAME_CAP as usize).contains(&data.len()));
                buf.push(FR_DATA);
                buf.extend_from_slice(&offset.to_be_bytes());
                buf.extend_from_slice(&(data.len() as u32).to_be_bytes());
                buf.extend_from_slice(data);
            }
            Self::Ack { offset, fin } => {
                buf.push(FR_ACK);
                buf.extend_from_slice(&offset.to_be_bytes());
                buf.push(u8::from(*fin));
            }
            Self::Fin { offset } => {
                buf.push(FR_FIN);
                buf.extend_from_slice(&offset.to_be_bytes());
            }
            Self::Reset { reason } => {
                buf.push(FR_RESET);
                buf.push(*reason);
            }
        }
    }
}

/// Incremental frame parser: feed raw stream bytes, get complete frames.
#[derive(Debug, Default)]
pub(crate) struct FrameParser {
    pending: Vec<u8>,
}

impl FrameParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Appends raw bytes and returns every fully parsed frame.
    ///
    /// Returns `Err` when the pending prefix is a malformed frame.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, SessionError> {
        self.pending.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            match self.parse_one()? {
                Some(frame) => frames.push(frame),
                None => return Ok(frames),
            }
        }
    }

    fn parse_one(&mut self) -> Result<Option<Frame>, SessionError> {
        let Some(&kind) = self.pending.first() else {
            return Ok(None);
        };
        match kind {
            FR_DATA => {
                if self.pending.len() < 13 {
                    return Ok(None);
                }
                let offset = be64(&self.pending[1..9]);
                let len = be32(&self.pending[9..13]) as usize;
                if len == 0 || len > FRAME_CAP as usize {
                    return Err(SessionError);
                }
                if self.pending.len() < 13 + len {
                    return Ok(None);
                }
                let mut data = Vec::with_capacity(len);
                data.extend_from_slice(&self.pending[13..13 + len]);
                self.pending.drain(..13 + len);
                Ok(Some(Frame::Data { offset, data }))
            }
            FR_ACK => {
                if self.pending.len() < 10 {
                    return Ok(None);
                }
                let fin = fin_flag(self.pending[9])?;
                let offset = be64(&self.pending[1..9]);
                self.pending.drain(..10);
                Ok(Some(Frame::Ack { offset, fin }))
            }
            FR_FIN => {
                if self.pending.len() < 9 {
                    return Ok(None);
                }
                let offset = be64(&self.pending[1..9]);
                self.pending.drain(..9);
                Ok(Some(Frame::Fin { offset }))
            }
            FR_RESET => {
                if self.pending.len() < 2 {
                    return Ok(None);
                }
                let reason = self.pending[1];
                if reason > RS_INVALID_STATE {
                    return Err(SessionError);
                }
                self.pending.drain(..2);
                Ok(Some(Frame::Reset { reason }))
            }
            _ => Err(SessionError),
        }
    }
}

/// Byte-count gate a producer waits on while the gated buffer is full.
#[derive(Debug, Clone)]
pub(crate) struct Gate {
    count: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl Gate {
    pub(crate) fn new() -> Self {
        Self {
            count: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Registers bytes entering the gated buffer.
    pub(crate) fn added(&self, n: usize) {
        self.count.fetch_add(n, Ordering::SeqCst);
    }

    /// Removes bytes from the gated buffer and wakes producers.
    pub(crate) fn freed(&self, n: usize) {
        self.count.fetch_sub(n, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// Waits until the count drops strictly below `cap`.
    ///
    /// The permit is registered before the re-check so a concurrent
    /// `freed` cannot slip between the check and the await.
    pub(crate) async fn wait_below(&self, cap: usize) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count() < cap {
                return;
            }
            notified.await;
        }
    }
}

/// Snapshot of the direction the local application receives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecvState {
    pub delivered: u64,
    pub peer_fin: bool,
}

/// One direction's state: the sender half (local application bytes) and the
/// receiver half (bytes delivered to the local application).
#[derive(Debug, Default)]
struct Dir {
    /// Bytes generated by the local application.
    sent: u64,
    /// Bytes the peer confirmed it delivered to its application.
    acked: u64,
    /// Retained bytes covering `[acked, sent)`.
    replay: VecDeque<u8>,
    fin: bool,
    /// `sent` at the moment the local application reached EOF.
    fin_final: Option<u64>,
    fin_acked: bool,
    /// A Fin frame has been sent for this direction.
    fin_sent: bool,
    /// Received bytes not yet delivered to the local application.
    queued: VecDeque<u8>,
    /// Bytes delivered to the local application.
    delivered: u64,
    peer_fin: bool,
    /// `delivered`-equivalent final offset announced by the peer.
    peer_fin_final: Option<u64>,
    /// EOF delivery to the local app has been requested.
    eof_requested: bool,
    fin_delivered: bool,
}

/// Events driving the engine actor.
#[derive(Debug)]
pub(crate) enum In {
    AppRead {
        dir: usize,
        data: Vec<u8>,
    },
    AppEof {
        dir: usize,
    },
    AppReadErr {
        err: Error,
    },
    AppWritten {
        offset: u64,
    },
    AppWriteErr,
    Attach {
        generation: u64,
        writer: mpsc::UnboundedSender<Frame>,
        /// The peer's delivered offset in this side's outbound direction.
        peer_delivered: u64,
        peer_fin: bool,
    },
    NetDown {
        generation: u64,
    },
    NetFrame {
        generation: u64,
        dir: usize,
        frame: Frame,
    },
    NetBad {
        generation: u64,
    },
    Cancel,
}

/// Signals emitted by the engine actor (terminal signals end the actor).
#[derive(Clone, Debug)]
pub(crate) enum EngineSignal {
    /// Both directions completed cleanly.
    Done,
    /// Fatal session error.
    Fatal(String),
    /// Session was cancelled.
    Cancelled,
}

/// Commands for the local application writer task.
#[derive(Debug)]
enum AppCmd {
    Chunk { offset: u64, data: Vec<u8> },
    Eof { offset: u64 },
}

struct AttachState {
    generation: u64,
    writer: mpsc::UnboundedSender<Frame>,
}

#[derive(Clone, Debug)]
pub(crate) struct EngineHandle {
    pub(crate) tx: mpsc::UnboundedSender<In>,
    pub(crate) terminal: watch::Receiver<Option<EngineSignal>>,
    pub(crate) queue_gate: Gate,
    pub(crate) state: watch::Receiver<RecvState>,
    pub(crate) app_dir: usize,
}

pub(crate) struct Engine {
    app_dir: usize,
    dirs: [Dir; 2],
    attach: Option<AttachState>,
    write_in_flight: bool,
    terminal: bool,
    app_tx: mpsc::UnboundedSender<AppCmd>,
    terminal_tx: watch::Sender<Option<EngineSignal>>,
    state_tx: watch::Sender<RecvState>,
    queue_gate: Gate,
    replay_gate: Gate,
    tasks: Vec<JoinHandle<()>>,
}

impl Engine {
    /// Spawns the actor, the application reader, and the application writer.
    ///
    /// `app_dir` is the direction the local application sends: 0 for clients
    /// (app stdin -> roost), 1 for the roost (sshd -> client).
    pub(crate) fn start<R, W>(app_read: R, app_write: W, app_dir: usize) -> EngineHandle
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        assert!((0..2).contains(&app_dir), "app_dir must be 0 or 1");
        let (tx, rx) = mpsc::unbounded_channel();
        let (terminal_tx, terminal) = watch::channel(None);
        let queue_gate = Gate::new();
        let replay_gate = Gate::new();
        let (state_tx, state) = watch::channel(RecvState::default());
        let handle_queue_gate = queue_gate.clone();
        let (app_tx, app_rx) = mpsc::unbounded_channel();

        let report = tx.clone();
        let replay_gate_ref = replay_gate.clone();
        let reader = tokio::spawn(async move {
            read_app(app_read, app_dir, &replay_gate_ref, report).await;
        });
        let report = tx.clone();
        let writer = tokio::spawn(async move {
            write_app(app_write, app_rx, report).await;
        });

        let engine = Self {
            app_dir,
            dirs: [Dir::default(), Dir::default()],
            attach: None,
            write_in_flight: false,
            terminal: false,
            app_tx,
            terminal_tx,
            state_tx,
            queue_gate,
            replay_gate,
            tasks: vec![reader, writer],
        };
        tokio::spawn(async move {
            engine.run(rx).await;
        });

        EngineHandle {
            tx,
            terminal,
            queue_gate: handle_queue_gate,
            state,
            app_dir,
        }
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<In>) {
        let mut keepalive = interval(KEEPALIVE_INTERVAL);
        loop {
            let event = tokio::select! {
                biased;
                event = rx.recv() => match event {
                    Some(event) => event,
                    None => break,
                },
                _ = keepalive.tick() => {
                    if self.attach.is_some() {
                        // No-progress cumulative ack: the offset is what we
                        // have delivered from the peer's send direction,
                        // never past the peer's `sent`. The fin flag
                        // re-announces that the peer's Fin reached our app,
                        // so a fin ack lost with a dead connection recovers.
                        let recv = &self.dirs[1 - self.app_dir];
                        self.emit(Frame::Ack {
                            offset: recv.delivered,
                            fin: recv.peer_fin_final == Some(recv.delivered),
                        });
                    }
                    continue;
                }
            };
            if let Some(signal) = self.handle(event) {
                self.finish(signal);
                break;
            }
        }
        if !self.terminal {
            self.finish(EngineSignal::Cancelled);
        }
        for task in &self.tasks {
            task.abort();
        }
    }

    /// Marks the engine finished and publishes the terminal signal.
    fn finish(&mut self, signal: EngineSignal) {
        self.terminal = true;
        let _ = self.terminal_tx.send(Some(signal));
    }

    fn handle(&mut self, event: In) -> Option<EngineSignal> {
        if self.terminal {
            return None;
        }
        match event {
            In::Cancel => self.cancel(),
            In::AppRead { dir, data } if dir == self.app_dir => self.on_app_read(data),
            In::AppEof { dir } if dir == self.app_dir => self.on_app_eof(),
            In::AppReadErr { err } => {
                self.fatal(RS_LOCAL_IO, format!("reading local application: {err}"))
            }
            In::AppWritten { offset } => self.on_app_written(offset),
            In::AppWriteErr => self.fatal(RS_LOCAL_IO, "writing to local application failed"),
            In::Attach {
                generation,
                writer,
                peer_delivered,
                peer_fin,
            } => self.on_attach(generation, writer, peer_delivered, peer_fin),
            In::NetDown { generation } => self.on_net_down(generation),
            In::NetFrame {
                generation,
                dir,
                frame,
            } => self.on_net_frame(generation, dir, frame),
            In::NetBad { generation } => self.on_net_bad(generation),
            _ => None,
        }
    }

    /// Sends `frame` on the active attachment. A frame sent after the
    /// attachment is already gone is dropped: the attachment's end is
    /// observed by the net task that owns the stream.
    fn emit(&mut self, frame: Frame) {
        match self.attach.as_ref().map(|a| a.writer.send(frame)) {
            Some(Ok(())) => {}
            _ => self.attach = None,
        }
    }

    fn on_app_read(&mut self, data: Vec<u8>) -> Option<EngineSignal> {
        let start = self.dirs[self.app_dir].sent;
        let dir = &mut self.dirs[self.app_dir];
        dir.sent += data.len() as u64;
        dir.replay.extend(data.iter().copied());
        // Invariant guard; the read-time gate in `read_app` keeps the
        // replay under REPLAY_CAP, so this is unreachable in production.
        let over = dir.replay.len() as u64 > REPLAY_CAP;
        let attached = self.attach.is_some();
        if over {
            return self.fatal(RS_LOCAL_IO, "replay buffer exceeded its cap");
        }
        if attached {
            self.send_data(start, &data);
        }
        None
    }

    /// Emits `data` as Data frames at `offset`, chunked to the frame cap.
    fn send_data(&mut self, offset: u64, data: &[u8]) {
        let mut off = offset;
        for chunk in data.chunks(FRAME_CAP as usize) {
            self.emit(Frame::Data {
                offset: off,
                data: chunk.to_vec(),
            });
            off += chunk.len() as u64;
        }
    }

    fn on_app_eof(&mut self) -> Option<EngineSignal> {
        let (due, offset) = {
            let dir = &mut self.dirs[self.app_dir];
            dir.fin = true;
            dir.fin_final = Some(dir.sent);
            (
                self.attach.is_some() && !dir.fin_sent && dir.acked >= dir.sent,
                dir.sent,
            )
        };
        if due {
            tracing::debug!(app_dir = self.app_dir, offset, "fin sent: app reached eof");
            self.dirs[self.app_dir].fin_sent = true;
            self.emit(Frame::Fin { offset });
        } else {
            let dir = &self.dirs[self.app_dir];
            tracing::debug!(
                app_dir = self.app_dir,
                sent = dir.sent,
                acked = dir.acked,
                attach = self.attach.is_some(),
                "app reached eof; fin deferred"
            );
        }
        self.check_done()
    }

    fn on_net_down(&mut self, generation: u64) -> Option<EngineSignal> {
        if self
            .attach
            .as_ref()
            .is_none_or(|a| a.generation != generation)
        {
            return None;
        }
        self.attach = None;
        None
    }

    /// A malformed wire frame from the peer: only fatal while the report is
    /// from the attachment that is actually active.
    fn on_net_bad(&mut self, generation: u64) -> Option<EngineSignal> {
        if self
            .attach
            .as_ref()
            .is_none_or(|a| a.generation != generation)
        {
            return None;
        }
        self.fatal(RS_INVALID_STATE, "malformed wire frame from peer")
    }

    fn on_net_frame(&mut self, generation: u64, dir: usize, frame: Frame) -> Option<EngineSignal> {
        match &self.attach {
            Some(a) if a.generation == generation => {}
            _ => return None,
        }
        match frame {
            Frame::Data { offset, data } => self.on_data(dir, offset, &data),
            Frame::Ack { offset, fin } => self.on_ack(dir, offset, fin),
            Frame::Fin { offset } => self.on_fin(dir, offset),
            Frame::Reset { reason } => self.fatal(
                RS_INVALID_STATE,
                format!("peer reset the session (reason code {reason})"),
            ),
        }
    }

    fn on_data(&mut self, dir: usize, offset: u64, data: &[u8]) -> Option<EngineSignal> {
        if dir != 1 - self.app_dir {
            return self.fatal(RS_INVALID_STATE, "data frame in unexpected direction");
        }
        let recv = &mut self.dirs[dir];
        let expected = recv.delivered + recv.queued.len() as u64;
        if offset > expected {
            return self.fatal(
                RS_INVALID_STATE,
                format!("data gap at {offset}, expected {expected}"),
            );
        }
        let append_from = expected.max(offset);
        let skip = (append_from - offset).min(data.len() as u64) as usize;
        let fresh = &data[skip..];
        if !fresh.is_empty() {
            recv.queued.extend(fresh.iter().copied());
        }
        // `net_reader` charged the whole frame; release the replayed
        // portion (up to the frame) that never entered the queue. A fully
        // stale frame has skip == data.len() and adds nothing.
        if skip > 0 {
            self.queue_gate.freed(skip);
        }
        self.pump_app()
    }

    fn on_ack(&mut self, dir: usize, offset: u64, fin: bool) -> Option<EngineSignal> {
        if dir != self.app_dir {
            return self.fatal(RS_INVALID_STATE, "ack frame in unexpected direction");
        }
        let sender = &mut self.dirs[dir];
        if offset > sender.sent {
            let sent = sender.sent;
            return self.fatal(RS_INVALID_STATE, format!("ack {offset} past sent {sent}"));
        }
        if let Some(final_offset) = sender.fin_final
            && offset > final_offset
        {
            return self.fatal(RS_INVALID_STATE, "ack past final offset");
        }
        if fin && sender.fin_final != Some(offset) {
            return self.fatal(RS_INVALID_STATE, "fin ack at unexpected offset");
        }
        if fin {
            if !sender.fin_acked {
                tracing::debug!(app_dir = self.app_dir, "fin acked by peer");
            }
            sender.fin_acked = true;
        }
        let mut drained = 0usize;
        let mut fin_due = false;
        if offset > sender.acked {
            drained = (offset - sender.acked) as usize;
            sender.acked = offset;
            sender.replay.drain(..drained);
            if sender.fin && sender.acked >= sender.sent {
                if !sender.fin_acked {
                    tracing::debug!(app_dir = self.app_dir, "fin acked by peer (cumulative ack)");
                }
                sender.fin_acked = true;
            }
            fin_due = sender.fin && !sender.fin_sent && sender.acked >= sender.sent;
        }
        self.replay_gate.freed(drained);
        if fin_due {
            tracing::debug!(
                app_dir = self.app_dir,
                offset = self.dirs[dir].sent,
                "fin sent"
            );
            self.dirs[dir].fin_sent = true;
            self.emit(Frame::Fin {
                offset: self.dirs[dir].sent,
            });
        }
        self.check_done()
    }

    fn on_fin(&mut self, dir: usize, offset: u64) -> Option<EngineSignal> {
        if dir != 1 - self.app_dir {
            return self.fatal(RS_INVALID_STATE, "fin frame in unexpected direction");
        }
        let recv = &mut self.dirs[dir];
        if recv.peer_fin {
            return None;
        }
        let expected = recv.delivered + recv.queued.len() as u64;
        if offset != expected {
            return self.fatal(
                RS_INVALID_STATE,
                format!("fin at {offset}, expected {expected}"),
            );
        }
        recv.peer_fin = true;
        recv.peer_fin_final = Some(offset);
        tracing::debug!(
            app_dir = self.app_dir,
            offset,
            delivered = recv.delivered,
            "fin received from peer"
        );
        self.state_tx
            .send(RecvState {
                delivered: recv.delivered,
                peer_fin: true,
            })
            .ok();
        // pump_app -> maybe_eof delivers the app's EOF once the final byte
        // has landed; the writer's confirmation closes the fin.
        self.pump_app()
    }

    fn on_attach(
        &mut self,
        generation: u64,
        writer: mpsc::UnboundedSender<Frame>,
        peer_delivered: u64,
        peer_fin: bool,
    ) -> Option<EngineSignal> {
        if self
            .attach
            .as_ref()
            .is_some_and(|a| generation <= a.generation)
        {
            return None;
        }
        let sender = &mut self.dirs[self.app_dir];
        let (sent, fin_final, acked) = (sender.sent, sender.fin_final, sender.acked);
        if peer_delivered < acked {
            return self.fatal(
                RS_INVALID_STATE,
                format!("resume claims delivery {peer_delivered} below last ack {acked}",),
            );
        }
        if peer_delivered > sent {
            return self.fatal(
                RS_INVALID_STATE,
                format!("resume claims delivery {peer_delivered} past sent {sent}"),
            );
        }
        if peer_fin && fin_final != Some(peer_delivered) {
            return self.fatal(RS_INVALID_STATE, "resume claims fin before final offset");
        }
        let old_acked = sender.acked;
        sender.acked = sender.acked.max(peer_delivered);
        if peer_fin {
            sender.fin_acked = true;
        }
        // Re-send the Fin until the peer acks it: a Fin lost with a dead
        // connection leaves `fin_sent` set without `fin_acked`, and without
        // the re-send the session can never complete on resume.
        let fin_due = sender.fin && !sender.fin_acked && sender.acked >= sender.sent;
        let replay_start = sender.acked;
        let sent = sender.sent;
        let drained = (sender.acked - old_acked) as usize;
        sender.replay.drain(..drained);
        self.replay_gate.freed(drained);

        self.attach = Some(AttachState { generation, writer });

        let ack = {
            let recv = &self.dirs[1 - self.app_dir];
            Frame::Ack {
                offset: recv.delivered,
                fin: recv.peer_fin_final == Some(recv.delivered),
            }
        };
        self.emit(ack);
        if fin_due {
            self.dirs[self.app_dir].fin_sent = true;
            self.emit(Frame::Fin { offset: sent });
        }
        let mut off = replay_start;
        while off < sent {
            let end = (off + FRAME_CAP as u64).min(sent);
            let data = {
                let sender = &self.dirs[self.app_dir];
                let base = sender.acked;
                sender
                    .replay
                    .range((off - base) as usize..(end - base) as usize)
                    .copied()
                    .collect()
            };
            self.emit(Frame::Data { offset: off, data });
            off = end;
        }
        self.check_done()
    }

    fn on_app_written(&mut self, offset: u64) -> Option<EngineSignal> {
        let dir = 1 - self.app_dir;
        let recv = &mut self.dirs[dir];
        if offset < recv.delivered {
            return self.fatal(
                RS_INVALID_STATE,
                format!("app writer reported decreasing offset {offset}"),
            );
        }
        if offset == recv.delivered {
            // EOF delivery confirmed (a duplicate report is a no-op).
            if recv.eof_requested && !recv.fin_delivered {
                recv.fin_delivered = true;
                tracing::debug!(app_dir = self.app_dir, offset, "peer fin delivered to app");
                if self.attach.is_some() {
                    self.emit(Frame::Ack { offset, fin: true });
                }
                return self.check_done();
            }
            return None;
        }
        let drained = (offset - recv.delivered) as usize;
        recv.queued.drain(..drained);
        recv.delivered = offset;
        self.queue_gate.freed(drained);
        self.write_in_flight = false;
        self.state_tx
            .send(RecvState {
                delivered: offset,
                peer_fin: recv.peer_fin,
            })
            .ok();
        // Cumulative ack so the peer's replay buffer can advance.
        if self.attach.is_some() {
            self.emit(Frame::Ack { offset, fin: false });
        }
        self.pump_app()
    }

    /// Feeds queued bytes to the app writer, one chunk in flight at a time.
    fn pump_app(&mut self) -> Option<EngineSignal> {
        let dir = 1 - self.app_dir;
        if self.write_in_flight {
            return self.maybe_eof();
        }
        if self.dirs[dir].queued.is_empty() {
            return self.maybe_eof();
        }
        let n = APP_CHUNK.min(self.dirs[dir].queued.len());
        let offset = self.dirs[dir].delivered;
        let data: Vec<u8> = self.dirs[dir].queued.iter().take(n).copied().collect();
        self.write_in_flight = true;
        match self.app_tx.send(AppCmd::Chunk { offset, data }) {
            Ok(()) => None,
            Err(_) => self.fatal(RS_LOCAL_IO, "app writer channel closed"),
        }
    }

    /// Requests EOF delivery once the peer's final byte has landed.
    fn maybe_eof(&mut self) -> Option<EngineSignal> {
        let dir = 1 - self.app_dir;
        let recv = &mut self.dirs[dir];
        if recv.peer_fin
            && !recv.eof_requested
            && let Some(final_offset) = recv.peer_fin_final
            && recv.delivered >= final_offset
        {
            recv.eof_requested = true;
            tracing::debug!(
                app_dir = self.app_dir,
                offset = final_offset,
                "app eof requested"
            );
            return match self.app_tx.send(AppCmd::Eof {
                offset: final_offset,
            }) {
                Ok(()) => None,
                Err(_) => self.fatal(RS_LOCAL_IO, "app writer channel closed"),
            };
        }
        None
    }

    fn check_done(&self) -> Option<EngineSignal> {
        let mine = &self.dirs[self.app_dir];
        let theirs = &self.dirs[1 - self.app_dir];
        if mine.fin_acked && theirs.peer_fin && theirs.fin_delivered {
            tracing::debug!(app_dir = self.app_dir, "session done");
            Some(EngineSignal::Done)
        } else {
            None
        }
    }

    fn cancel(&mut self) -> Option<EngineSignal> {
        if self.attach.is_some() {
            self.emit(Frame::Reset {
                reason: RS_CANCELLED,
            });
        }
        self.attach = None;
        self.terminal = true;
        Some(EngineSignal::Cancelled)
    }

    fn fatal(&mut self, reason: u8, message: impl Into<String>) -> Option<EngineSignal> {
        let message = message.into();
        tracing::warn!(reason, %message, "engine fatal");
        if self.attach.is_some() {
            self.emit(Frame::Reset { reason });
        }
        self.attach = None;
        self.terminal = true;
        Some(EngineSignal::Fatal(message))
    }
}

async fn read_app<R: AsyncRead + Unpin + Send>(
    mut reader: R,
    dir: usize,
    replay_gate: &Gate,
    report: mpsc::UnboundedSender<In>,
) {
    let mut buf = [0u8; 8 * 1024];
    // Leave one app chunk of headroom so a read that passed the gate can
    // never push the replay past REPLAY_CAP.
    let cap = (REPLAY_CAP - APP_CHUNK as u64) as usize;
    loop {
        replay_gate.wait_below(cap).await;
        match reader.read(&mut buf).await {
            Ok(0) => {
                let _ = report.send(In::AppEof { dir });
                return;
            }
            Ok(n) => {
                // Charge the gate at read time so the app is throttled as
                // soon as bytes are read, before the engine even sees them.
                // `on_ack`/`on_attach` free the same bytes when acked.
                replay_gate.added(n);
                if report
                    .send(In::AppRead {
                        dir,
                        data: buf[..n].to_vec(),
                    })
                    .is_err()
                {
                    return;
                }
            }
            Err(err) => {
                let _ = report.send(In::AppReadErr { err });
                return;
            }
        }
    }
}

async fn write_app<W: AsyncWrite + Unpin + Send>(
    mut writer: W,
    mut rx: mpsc::UnboundedReceiver<AppCmd>,
    report: mpsc::UnboundedSender<In>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            AppCmd::Chunk { offset, data } => {
                let written = writer.write_all(&data).await.is_ok();
                let ok = written && writer.flush().await.is_ok();
                if !ok {
                    let _ = report.send(In::AppWriteErr);
                    return;
                }
                if report
                    .send(In::AppWritten {
                        offset: offset + data.len() as u64,
                    })
                    .is_err()
                {
                    return;
                }
            }
            AppCmd::Eof { offset } => {
                if writer.shutdown().await.is_err() {
                    let _ = report.send(In::AppWriteErr);
                    return;
                }
                if report.send(In::AppWritten { offset }).is_err() {
                    return;
                }
            }
        }
    }
}

/// Backoff schedule for re-dial attempts; capped at the last entry.
pub(crate) const RETRY_BACKOFFS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

/// An attachment that stayed up at least this long resets the backoff ladder.
pub(crate) const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);

/// Deadline per dial + handshake attempt.
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Grace period to wait for the engine to acknowledge a cancel.
const CANCEL_GRACE: Duration = Duration::from_secs(1);

/// Liveness beacon period while attached. A cumulative ack with no
/// progress doubles as the beacon: QUIC may not surface a dead path
/// (keep-alive activity resets the idle timer), so liveness is proved at
/// the session layer, in both directions.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(20);

/// Frame silence that declares the link dead. Three beacon intervals of
/// margin over relay jitter.
const READER_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// QUIC transport error code: APPLICATION_ERROR.
const CODE_APPLICATION_ERROR: u64 = 0x0C;
/// QUIC crypto alert range (`CRYPTO_ERROR`).
const CODE_CRYPTO_RANGE: Range<u64> = 0x100..0x200;

/// A dial attempt that can never succeed while the roost is unchanged.
#[derive(Debug)]
enum DialFailure {
    /// Retry with backoff: offline, timeout, refusal, capacity, backend down.
    Retryable(anyhow::Error),
    /// The roost rejected the `/pigeons/1` protocol (or the session itself).
    Terminal(anyhow::Error),
}

/// Runs a resumable client session against `remote`: one Open, then Resume
/// forever until the session completes, hits a terminal failure, or `cancel`
/// flips true.
pub(crate) async fn run_client<R, W>(
    endpoint: &Endpoint,
    remote: impl Into<EndpointAddr>,
    alpn: &[u8],
    app_read: R,
    app_write: W,
    mut cancel: watch::Receiver<bool>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let session_id = SecretKey::generate().to_bytes();
    let remote: EndpointAddr = remote.into();
    let engine = Engine::start(app_read, app_write, 0);

    let mut generation: u64 = 0;
    // True once the first handshake completed: from then on the roost
    // knows the session and later dials must resume it, even if earlier
    // dials in the same attempt failed before ever reaching it.
    let mut opened = false;
    let mut backoff_idx = 0usize;
    let mut offline = false;

    loop {
        if *cancel.borrow_and_update() {
            return cancel_session(&engine).await;
        }

        // The engine can go terminal while detached (app IO error, replay
        // overflow); end the session instead of redialing.
        let mut terminal = engine.terminal.clone();
        tokio::select! {
            _ = terminal.changed() => {
                return terminal_to_result(terminal.borrow_and_update().as_ref());
            }
            result = dial_and_handshake(endpoint, remote.clone(), alpn, &session_id, generation, opened, &engine) => match result {
                Ok((_connection, attach)) => {
                    opened = true;
                    if offline {
                        eprintln!("pigeons: reconnected to roost; session resumed");
                        offline = false;
                    }
                    let attached_at = Instant::now();
                    match run_attachment(engine.clone(), attach, generation).await {
                        AttachmentEnd::Done => return Ok(()),
                        AttachmentEnd::Fatal(err) => return Err(err),
                        AttachmentEnd::Cancelled => return Ok(()),
                        AttachmentEnd::Detached | AttachmentEnd::EngineGone => {}
                    }
                    if !offline {
                        eprintln!("pigeons: connection to roost lost; reconnecting");
                        offline = true;
                    }
                    if attached_at.elapsed() < BACKOFF_RESET_AFTER {
                        backoff_idx = (backoff_idx + 1).min(RETRY_BACKOFFS.len() - 1);
                    } else {
                        backoff_idx = 0;
                    }
                }
                Err(DialFailure::Terminal(err)) => return Err(err),
                Err(DialFailure::Retryable(err)) => {
                    if !offline {
                        eprintln!("pigeons: cannot reach roost; retrying");
                        offline = true;
                    }
                    tracing::debug!(%err, generation, "dial failed, retrying");
                }
            },
        }

        generation = generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("session generation counter overflowed"))?;
        sleep_cancelled(RETRY_BACKOFFS[backoff_idx], &mut cancel).await;
    }
}

/// Maps a terminal engine signal to the client result.
fn terminal_to_result(signal: Option<&EngineSignal>) -> anyhow::Result<()> {
    match signal {
        None | Some(EngineSignal::Done) | Some(EngineSignal::Cancelled) => Ok(()),
        Some(EngineSignal::Fatal(err)) => Err(anyhow::anyhow!("{err}")),
    }
}
/// The result of a successful dial + handshake.
pub(crate) struct OpenAttachment {
    pub(crate) send: SendStream,
    pub(crate) recv: RecvStream,
    pub(crate) received_offset: u64,
    pub(crate) received_fin: bool,
}

async fn dial_and_handshake(
    endpoint: &Endpoint,
    remote: impl Into<EndpointAddr>,
    alpn: &[u8],
    session_id: &[u8; 32],
    generation: u64,
    // Whether the first handshake already completed on this session.
    opened: bool,
    engine: &EngineHandle,
) -> Result<(Connection, OpenAttachment), DialFailure> {
    let connecting = match endpoint
        .connect_with_opts(remote, alpn, Default::default())
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(%e, "dial setup failed, retrying");
            return Err(DialFailure::Retryable(e.into()));
        }
    };
    let connection = match timeout(DIAL_TIMEOUT, connecting).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => {
            let terminal = matches!(
                &e,
                ConnectingError::ConnectionError {
                    source,
                    meta: _,
                }
                    if is_protocol_rejection(source)
            );
            return Err(if terminal {
                DialFailure::Terminal(anyhow::anyhow!(
                    "roost rejected the /pigeons/1 protocol: {e}; upgrade the roost to a build that speaks it"
                ))
            } else {
                DialFailure::Retryable(e.into())
            });
        }
        Err(_) => {
            return Err(DialFailure::Retryable(anyhow::anyhow!(
                "dial timed out after {}s",
                DIAL_TIMEOUT.as_secs()
            )));
        }
    };
    let (mut send, mut recv) = match connection.open_bi().await {
        Ok(s) => s,
        Err(e) => return Err(classify_dial_error(e)),
    };

    let recv_state = {
        let mut state = engine.state.clone();
        *state.borrow_and_update()
    };
    let hs = Handshake {
        op: if opened { OP_RESUME } else { OP_OPEN },
        session_id: *session_id,
        generation,
        received_offset: recv_state.delivered,
        received_fin: recv_state.peer_fin,
    };
    let mut hs_buf = Vec::with_capacity(Handshake::LEN);
    hs.encode(&mut hs_buf);
    if send.write_all(&hs_buf).await.is_err() {
        return Err(DialFailure::Retryable(anyhow::anyhow!(
            "writing handshake failed"
        )));
    }

    let mut ready_buf = [0u8; READY_LEN];
    match recv.read_exact(&mut ready_buf).await {
        Ok(()) => {}
        Err(_) => {
            return Err(DialFailure::Retryable(anyhow::anyhow!(
                "roost closed the stream before responding to the handshake"
            )));
        }
    }
    match Ready::decode(&ready_buf) {
        Ok(Ok(ready)) => Ok((
            connection,
            OpenAttachment {
                send,
                recv,
                received_offset: ready.received_offset,
                received_fin: ready.received_fin,
            },
        )),
        Ok(Err(peer)) => Err(match peer {
            PeerError::UnknownSession => DialFailure::Terminal(anyhow::anyhow!(
                "roost does not know this session; it likely restarted — start a new SSH connection"
            )),
            PeerError::InvalidState => {
                DialFailure::Terminal(anyhow::anyhow!("roost rejected the session state"))
            }
            PeerError::Capacity | PeerError::BackendUnavailable => {
                DialFailure::Retryable(anyhow::anyhow!("roost not ready"))
            }
        }),
        Err(_) => Err(DialFailure::Terminal(anyhow::anyhow!(
            "malformed roost handshake response"
        ))),
    }
}

/// Terminal when the roost answered with an APPLICATION_ERROR or crypto
/// transport error (protocol rejection); everything else is retryable.
fn classify_dial_error(conn: ConnectionError) -> DialFailure {
    if is_protocol_rejection(&conn) {
        DialFailure::Terminal(anyhow::anyhow!(
            "roost rejected the /pigeons/1 protocol: {conn}; upgrade the roost to a build that speaks it"
        ))
    } else {
        DialFailure::Retryable(conn.into())
    }
}

fn is_protocol_rejection(conn: &ConnectionError) -> bool {
    let code: u64 = match conn {
        ConnectionError::TransportError(te) => te.code.into(),
        ConnectionError::ConnectionClosed(close) => close.error_code.into(),
        ConnectionError::ApplicationClosed(close) => close.error_code.into_inner(),
        _ => return false,
    };
    code == CODE_APPLICATION_ERROR || CODE_CRYPTO_RANGE.contains(&code)
}

#[derive(Debug)]
pub(crate) enum AttachmentEnd {
    /// The attachment died; session state is retained for the next attempt.
    Detached,
    /// Both directions completed cleanly.
    Done,
    Fatal(anyhow::Error),
    Cancelled,
    /// The engine actor already exited.
    EngineGone,
}

enum NetEvent {
    Frame { dir: usize, frame: Frame },
    Down,
    Bad,
}

/// Drives one attachment: net reader + writer tasks around the engine, until
/// the stream dies or the session reaches a terminal state.
pub(crate) async fn run_attachment(
    engine: EngineHandle,
    attach: OpenAttachment,
    generation: u64,
) -> AttachmentEnd {
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Frame>();
    let (net_tx, mut net_rx) = mpsc::channel(8);
    let reader = tokio::spawn(net_reader(
        attach.recv,
        engine.queue_gate.clone(),
        net_tx,
        engine.app_dir,
    ));
    let writer = tokio::spawn(net_writer(attach.send, out_rx));

    if engine
        .tx
        .send(In::Attach {
            generation,
            writer: out_tx,
            peer_delivered: attach.received_offset,
            peer_fin: attach.received_fin,
        })
        .is_err()
    {
        reader.abort();
        writer.abort();
        return AttachmentEnd::EngineGone;
    }

    let mut terminal = engine.terminal.clone();
    loop {
        tokio::select! {
            event = net_rx.recv() => match event {
                Some(NetEvent::Frame { dir, frame }) => {
                    if engine.tx.send(In::NetFrame { generation, dir, frame }).is_err() {
                        return finish(&reader, &writer, AttachmentEnd::EngineGone);
                    }
                }
                Some(NetEvent::Bad) => {
                    if engine.tx.send(In::NetBad { generation }).is_err() {
                        return finish(&reader, &writer, AttachmentEnd::EngineGone);
                    }
                }
                Some(NetEvent::Down) => {
                    let _ = engine.tx.send(In::NetDown { generation });
                    break;
                }
                None => break,
            },
            _ = terminal.changed() => {
                let end = match terminal.borrow_and_update().as_ref() {
                    None => AttachmentEnd::EngineGone,
                    Some(EngineSignal::Done) => AttachmentEnd::Done,
                    Some(EngineSignal::Fatal(err)) => AttachmentEnd::Fatal(anyhow::anyhow!("{err}")),
                    Some(EngineSignal::Cancelled) => AttachmentEnd::Cancelled,
                };
                return finish(&reader, &writer, end);
            }
        }
    }
    finish(&reader, &writer, AttachmentEnd::Detached)
}

fn finish(reader: &JoinHandle<()>, writer: &JoinHandle<()>, end: AttachmentEnd) -> AttachmentEnd {
    reader.abort();
    writer.abort();
    end
}

/// Frames the attachment stream into the engine. Data frames are charged
/// against the queue gate before forwarding (and the deduped tail is freed
/// in `on_data`) so the network side is throttled end to end.
async fn net_reader(
    mut recv: RecvStream,
    queue_gate: Gate,
    report: mpsc::Sender<NetEvent>,
    app_dir: usize,
) {
    let mut parser = FrameParser::new();
    let mut buf = [0u8; FRAME_CAP as usize + 16];
    loop {
        match timeout(READER_IDLE_TIMEOUT, recv.read(&mut buf)).await {
            Err(_) => {
                tracing::debug!(
                    "frame silence exceeded {:?}; link presumed dead",
                    READER_IDLE_TIMEOUT
                );
                break;
            }
            Ok(Ok(None)) | Ok(Err(_)) => break,
            Ok(Ok(Some(0))) => continue,
            Ok(Ok(Some(n))) => match parser.feed(&buf[..n]) {
                Err(_) => {
                    let _ = report.send(NetEvent::Bad).await;
                    break;
                }
                Ok(frames) => {
                    for frame in frames {
                        let dir = match &frame {
                            Frame::Data { .. } | Frame::Fin { .. } => 1 - app_dir,
                            _ => app_dir,
                        };
                        if let Frame::Data { data, .. } = &frame {
                            queue_gate.added(data.len());
                            queue_gate.wait_below(QUEUE_CAP as usize).await;
                        }
                        if report.send(NetEvent::Frame { dir, frame }).await.is_err() {
                            return;
                        }
                    }
                }
            },
        }
    }
    let _ = report.send(NetEvent::Down).await;
}

async fn net_writer(mut send: SendStream, mut out_rx: mpsc::UnboundedReceiver<Frame>) {
    let mut buf = Vec::with_capacity(FRAME_CAP as usize + 13);
    while let Some(frame) = out_rx.recv().await {
        frame.encode(&mut buf);
        if send.write_all(&buf).await.is_err() {
            return;
        }
    }
}

async fn sleep_cancelled(delay: Duration, cancel: &mut watch::Receiver<bool>) {
    let mut sleeper = Box::pin(sleep(delay));
    tokio::select! {
        _ = sleeper.as_mut() => {}
        _ = cancel.changed() => {}
    }
}

/// Cancels the session and waits briefly for the engine to acknowledge.
async fn cancel_session(engine: &EngineHandle) -> anyhow::Result<()> {
    let _ = engine.tx.send(In::Cancel);
    let mut terminal = engine.terminal.clone();
    let _ = timeout(CANCEL_GRACE, terminal.changed()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use iroh::{TransportAddr, endpoint::VarInt, protocol::Router};
    use tokio::{
        io::{DuplexStream, duplex},
        net::{TcpListener, UdpSocket},
    };

    use super::*;
    use crate::protocol::PigeonsProtocol;

    #[test]
    fn handshake_roundtrip() {
        let mut buf = Vec::new();
        let hs = Handshake {
            op: OP_RESUME,
            session_id: [7; 32],
            generation: 42,
            received_offset: 1 << 40,
            received_fin: true,
        };
        hs.encode(&mut buf);
        assert_eq!(buf.len(), HANDSHAKE_LEN);
        let dec = Handshake::decode(&buf).unwrap();
        assert_eq!(dec.op, OP_RESUME);
        assert_eq!(dec.session_id, [7; 32]);
        assert_eq!(dec.generation, 42);
        assert_eq!(dec.received_offset, 1 << 40);
        assert!(dec.received_fin);
    }

    #[test]
    fn handshake_rejects_bad_op_and_flag() {
        let mut buf = Vec::new();
        let hs = Handshake {
            op: 9,
            session_id: [0; 32],
            generation: 0,
            received_offset: 0,
            received_fin: false,
        };
        hs.encode(&mut buf);
        assert_eq!(
            Handshake::decode(&buf).unwrap().validate(),
            Err(SessionError)
        );

        let mut bad_fin = buf.clone();
        bad_fin[49] = 2;
        assert_eq!(Handshake::decode(&bad_fin), Err(SessionError));
        assert_eq!(
            Handshake::decode(&buf[..HANDSHAKE_LEN - 1]),
            Err(SessionError)
        );
    }

    #[test]
    fn ready_decode_all_statuses() {
        let decode = |status: u8| {
            let mut buf = [0u8; READY_LEN];
            buf[0] = status;
            Ready::decode(&buf)
        };
        assert_eq!(
            decode(ST_READY),
            Ok(Ok(Ready {
                received_offset: 0,
                received_fin: false
            }))
        );
        assert_eq!(
            decode(ST_UNKNOWN_SESSION),
            Ok(Err(PeerError::UnknownSession))
        );
        assert_eq!(decode(ST_CAPACITY), Ok(Err(PeerError::Capacity)));
        assert_eq!(
            decode(ST_BACKEND_UNAVAILABLE),
            Ok(Err(PeerError::BackendUnavailable))
        );
        assert_eq!(decode(ST_INVALID_STATE), Ok(Err(PeerError::InvalidState)));
        assert_eq!(decode(250), Err(SessionError));

        // A non-binary fin flag is malformed.
        let mut buf = [0u8; READY_LEN];
        buf[0] = ST_READY;
        buf[9] = 2;
        assert_eq!(Ready::decode(&buf), Err(SessionError));
    }

    #[test]
    fn frame_parser_handles_incremental_and_malformed() {
        let mut buf = Vec::new();
        let mut scratch = Vec::new();
        for frame in [
            Frame::Data {
                offset: 5,
                data: vec![1, 2, 3],
            },
            Frame::Ack {
                offset: 5,
                fin: true,
            },
            Frame::Fin { offset: 5 },
            Frame::Reset {
                reason: RS_CANCELLED,
            },
        ] {
            frame.encode(&mut scratch);
            buf.extend_from_slice(&scratch);
        }

        // Incremental: a frame only parses once its last byte arrives.
        for k in 1..=15 {
            let mut p = FrameParser::new();
            let frames = p.feed(&buf[..k]).unwrap();
            assert!(
                frames.is_empty() || frames.len() == 1,
                "prefix {k} parsed {frames:?}"
            );
        }
        let mut p = FrameParser::new();
        assert!(p.feed(&buf[..15]).unwrap().is_empty());
        let mut p = FrameParser::new();
        let frames = p.feed(&buf[..16]).unwrap();
        assert_eq!(frames.len(), 1);
        let mut parser = FrameParser::new();
        let frames = parser.feed(&buf).unwrap();
        assert_eq!(frames.len(), 4);
        assert!(matches!(
            &frames[0],
            Frame::Data { offset: 5, data } if data == &vec![1, 2, 3]
        ));
        assert!(matches!(
            &frames[1],
            Frame::Ack {
                offset: 5,
                fin: true
            }
        ));
        assert!(matches!(&frames[2], Frame::Fin { offset: 5 }));
        assert!(matches!(&frames[3], Frame::Reset { reason: 0 }));
        assert!(parser.feed(b"").unwrap().is_empty());

        // Malformed: zero-length data, oversized data, bad kind, bad flags.
        let mut oversized = vec![FR_DATA];
        oversized.extend_from_slice(&0u64.to_be_bytes());
        oversized.extend_from_slice(&10_000_000u32.to_be_bytes());
        for bad in [
            vec![FR_DATA, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            oversized,
            vec![9, 1],
            vec![FR_ACK, 0, 0, 0, 0, 0, 0, 0, 0, 7],
            vec![FR_RESET, 9],
            vec![FR_RESET, 3],
        ] {
            let mut p = FrameParser::new();
            assert_eq!(p.feed(&bad), Err(SessionError), "bad: {bad:?}");
        }
    }

    #[tokio::test]
    async fn gate_wakes_when_freed() {
        let gate = Gate::new();
        gate.added(10);

        // While the count is at or above the cap, wait_below must not return.
        assert!(
            timeout(Duration::from_millis(50), gate.wait_below(5))
                .await
                .is_err(),
            "wait_below returned while the gate was full"
        );

        gate.freed(10);

        // Once freed below the cap, wait_below returns immediately.
        gate.wait_below(5).await;
        assert_eq!(gate.count(), 0);
    }

    // -- Engine tests ------------------------------------------------------

    const T: Duration = Duration::from_millis(500);

    /// Spawns an engine whose app IO cannot interfere: the read side is
    /// never written or closed (the reader task waits forever), and
    /// `app_out` is what the engine writes to the local app.
    fn start_engine(app_dir: usize) -> (EngineHandle, DuplexStream, DuplexStream) {
        let (app_in, engine_read) = duplex(1 << 20);
        let (engine_write, app_out) = duplex(1 << 20);
        let engine = Engine::start(engine_read, engine_write, app_dir);
        (engine, app_in, app_out)
    }

    async fn next_frame(rx: &mut mpsc::UnboundedReceiver<Frame>) -> Option<Frame> {
        timeout(T, rx.recv()).await.ok().flatten()
    }

    async fn next_signal(engine: &mut EngineHandle) -> Option<EngineSignal> {
        let mut terminal = engine.terminal.clone();
        if terminal.borrow().is_some() {
            return terminal.borrow().clone();
        }
        if timeout(T, terminal.changed()).await.is_err() {
            return None;
        }
        terminal.borrow().clone()
    }

    async fn assert_no_signal(engine: &mut EngineHandle) {
        assert!(
            next_signal(engine).await.is_none(),
            "unexpected engine signal"
        );
    }

    async fn read_app_bytes(app_out: &mut DuplexStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        timeout(T, app_out.read_exact(&mut buf))
            .await
            .expect("timed out waiting for app bytes")
            .expect("app stream closed early");
        buf
    }

    fn attach(
        engine: &EngineHandle,
        generation: u64,
        peer_delivered: u64,
        peer_fin: bool,
    ) -> mpsc::UnboundedReceiver<Frame> {
        let (tx, rx) = mpsc::unbounded_channel();
        engine
            .tx
            .send(In::Attach {
                generation,
                writer: tx,
                peer_delivered,
                peer_fin,
            })
            .unwrap();
        rx
    }

    #[tokio::test]
    async fn engine_emits_keepalive_beacon_while_attached() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        // Prime the send axis before attaching so the attach replays it:
        // the app sends 100 bytes.
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: vec![0u8; 100],
            })
            .unwrap();
        let mut frames = attach(&engine, 0, 0, false);
        // The peer acks all 100 bytes, so the send axis's `acked` = 100.
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Ack {
                    offset: 100,
                    fin: false,
                },
            })
            .unwrap();
        // The attach handshake ack arrives first, then the replayed data.
        assert!(
            matches!(
                next_frame(&mut frames).await,
                Some(Frame::Ack {
                    offset: 0,
                    fin: false
                })
            ),
            "attach ack"
        );
        assert!(
            matches!(
                next_frame(&mut frames).await,
                Some(Frame::Data { offset: 0, .. })
            ),
            "replayed app data"
        );
        // The beacon must report the receive axis (zero here), not the
        // send axis (100): a foreign offset is fatal at the peer.
        let beacon = timeout(KEEPALIVE_INTERVAL + Duration::from_secs(2), frames.recv())
            .await
            .expect("no keepalive beacon within one interval")
            .expect("writer channel closed");
        assert_eq!(
            beacon,
            Frame::Ack {
                offset: 0,
                fin: false
            },
            "beacon must be a no-progress ack on the receive axis"
        );
        assert_no_signal(&mut engine).await;
    }

    #[tokio::test]
    async fn engine_replays_unacked_bytes_on_resume() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: b"hello".to_vec(),
            })
            .unwrap();

        let mut frames = attach(&engine, 0, 0, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Data { offset: 0, data }) if data == b"hello"
        ));

        // The peer acks 3 of 5 bytes; 2 stay in the replay buffer.
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Ack {
                    offset: 3,
                    fin: false,
                },
            })
            .unwrap();
        assert_no_signal(&mut engine).await;

        // The network drops; the engine idles without a terminal signal.
        engine.tx.send(In::NetDown { generation: 0 }).unwrap();
        assert_no_signal(&mut engine).await;

        // Resuming with 3 received resends only the unacked tail.
        let mut frames = attach(&engine, 1, 3, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Data { offset: 3, data }) if data == b"lo"
        ));

        // Resuming with less than the last ack is a protocol violation.
        let _frames = attach(&engine, 2, 2, false);
        match next_signal(&mut engine).await {
            Some(EngineSignal::Fatal(err)) => {
                assert!(err.to_string().contains("below last ack"))
            }
            other => panic!("expected fatal, got {other:?}"),
        }
    }

    /// Regression: a Fin frame lost with a dead connection must be re-sent
    /// on resume. Once sent, the Fin was never re-sent again, so a session
    /// whose final Fin was lost could never complete on resume: the peer
    /// waits for it forever and the client redials into a permanent
    /// no-progress cycle.
    #[tokio::test]
    async fn engine_resends_lost_fin_on_resume() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: b"hello".to_vec(),
            })
            .unwrap();
        let mut frames = attach(&engine, 0, 0, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Data { offset: 0, data }) if data == b"hello"
        ));
        // The peer acks all 5 bytes; the app reaches EOF, so the Fin goes out.
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Ack {
                    offset: 5,
                    fin: false,
                },
            })
            .unwrap();
        engine.tx.send(In::AppEof { dir: 0 }).unwrap();
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Fin { offset: 5 })
        ));

        // The connection dies with the Fin lost: the peer never saw it.
        engine.tx.send(In::NetDown { generation: 0 }).unwrap();
        assert_no_signal(&mut engine).await;

        // Resume: the peer claims full data delivery but no fin delivery.
        // The lost Fin must be re-sent on the new attachment.
        let mut resumed = attach(&engine, 1, 5, false);
        assert!(matches!(
            next_frame(&mut resumed).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
        assert!(
            matches!(
                next_frame(&mut resumed).await,
                Some(Frame::Fin { offset: 5 })
            ),
            "a lost Fin must be re-sent on resume"
        );
        assert_no_signal(&mut engine).await;

        // The peer delivers the re-sent Fin to its app (fin ack) and its own
        // application reaches EOF (peer Fin at 0): the session completes.
        engine
            .tx
            .send(In::NetFrame {
                generation: 1,
                dir: 0,
                frame: Frame::Ack {
                    offset: 5,
                    fin: true,
                },
            })
            .unwrap();
        engine
            .tx
            .send(In::NetFrame {
                generation: 1,
                dir: 1,
                frame: Frame::Fin { offset: 0 },
            })
            .unwrap();
        assert!(matches!(
            next_frame(&mut resumed).await,
            Some(Frame::Ack {
                offset: 0,
                fin: true
            })
        ));
        assert!(
            matches!(next_signal(&mut engine).await, Some(EngineSignal::Done)),
            "session must complete after the re-sent Fin is delivered"
        );
    }

    #[tokio::test]
    async fn engine_idle_app_data_accumulates_and_resumes() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: b"ab".to_vec(),
            })
            .unwrap();
        let mut frames = attach(&engine, 0, 0, false);
        let _ = next_frame(&mut frames).await; // Ack
        let _ = next_frame(&mut frames).await; // Data
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Ack {
                    offset: 2,
                    fin: false,
                },
            })
            .unwrap();
        engine.tx.send(In::NetDown { generation: 0 }).unwrap();
        assert_no_signal(&mut engine).await;

        // The app keeps running while the network is down.
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: b"cd".to_vec(),
            })
            .unwrap();
        assert_no_signal(&mut engine).await;

        let mut frames = attach(&engine, 1, 2, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Data { offset: 2, data }) if data == b"cd"
        ));
    }

    #[tokio::test]
    async fn engine_stale_frames_and_attach_are_noop() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        let mut frames = attach(&engine, 5, 0, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));

        // A lower-generation attach produces nothing.
        let mut stale = attach(&engine, 3, 0, false);
        assert!(
            next_frame(&mut stale).await.is_none(),
            "stale attach must produce no frames"
        );
        // Lower-generation frames are ignored.
        engine
            .tx
            .send(In::NetFrame {
                generation: 3,
                dir: 1,
                frame: Frame::Data {
                    offset: 0,
                    data: vec![42],
                },
            })
            .unwrap();
        assert!(
            timeout(T, engine.state.wait_for(|s| s.delivered > 0))
                .await
                .is_err(),
            "stale frame must not be applied"
        );
        engine.tx.send(In::NetDown { generation: 3 }).unwrap();
        assert_no_signal(&mut engine).await;

        // A higher generation re-attaches fine.
        let mut frames = attach(&engine, 6, 0, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
    }

    #[tokio::test]
    async fn engine_data_gap_is_fatal() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        let mut frames = attach(&engine, 0, 0, false);
        let _ = next_frame(&mut frames).await;
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 1,
                frame: Frame::Data {
                    offset: 100,
                    data: vec![1],
                },
            })
            .unwrap();
        match next_signal(&mut engine).await {
            Some(EngineSignal::Fatal(err)) => assert!(err.to_string().contains("data gap")),
            other => panic!("expected data gap fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_frames_in_wrong_direction_are_fatal() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        let mut frames = attach(&engine, 0, 0, false);
        let _ = next_frame(&mut frames).await;
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Data {
                    offset: 0,
                    data: vec![1],
                },
            })
            .unwrap();
        match next_signal(&mut engine).await {
            Some(EngineSignal::Fatal(err)) => {
                assert!(err.to_string().contains("unexpected direction"))
            }
            other => panic!("expected fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_ack_beyond_sent_is_fatal() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: vec![0, 1],
            })
            .unwrap();
        let mut frames = attach(&engine, 0, 0, false);
        let _ = next_frame(&mut frames).await; // Ack
        let _ = next_frame(&mut frames).await; // Data

        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Ack {
                    offset: 7,
                    fin: false,
                },
            })
            .unwrap();
        match next_signal(&mut engine).await {
            Some(EngineSignal::Fatal(err)) => assert!(err.to_string().contains("past sent")),
            other => panic!("expected fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_replay_overflow_is_fatal() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: vec![0u8; REPLAY_CAP as usize + 1],
            })
            .unwrap();
        match next_signal(&mut engine).await {
            Some(EngineSignal::Fatal(err)) => assert!(err.to_string().contains("replay buffer")),
            other => panic!("expected fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn replay_backpressure_holds_the_app_reader() {
        // No attachment, so nothing ever acks: the replay keeps every byte
        // the engine reads. Pushing far more than REPLAY_CAP into the app
        // must stall the reader, not fatal the engine.
        let (mut engine, mut app_in, _app_out) = start_engine(0);
        let chunk = vec![0u8; 16 * 1024];
        for _ in 0..64 {
            app_in.write_all(&chunk).await.unwrap();
        }
        sleep(Duration::from_millis(200)).await;
        assert!(
            engine.terminal.borrow_and_update().is_none(),
            "engine went terminal with unacked app data: {:?}",
            engine.terminal.borrow_and_update()
        );
    }

    #[tokio::test]
    async fn gate_wait_below_wakes_on_concurrent_free() {
        // Regression: a `freed` landing between the condition check and the
        // await must not hang the waiter.
        for _ in 0..200 {
            let gate = Gate::new();
            gate.added(10);
            let waiter = tokio::spawn({
                let gate = gate.clone();
                async move { gate.wait_below(5).await }
            });
            sleep(Duration::from_micros(20)).await;
            gate.freed(6);
            assert!(timeout(T, waiter).await.is_ok(), "waiter missed the free");
        }
    }

    #[tokio::test]
    async fn engine_decreasing_app_written_is_fatal() {
        let (mut engine, _app_in, mut app_out) = start_engine(0);
        let mut frames = attach(&engine, 0, 0, false);
        let _ = next_frame(&mut frames).await; // initial Ack
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 1,
                frame: Frame::Data {
                    offset: 0,
                    data: b"abcde".to_vec(),
                },
            })
            .unwrap();
        // The writer reports its own progress; wait until it lands.
        assert_eq!(read_app_bytes(&mut app_out, 5).await, b"abcde");
        timeout(T, engine.state.wait_for(|s| s.delivered == 5))
            .await
            .expect("app writer did not report progress")
            .expect("state channel closed");

        // A decreasing progress report is a protocol violation.
        engine.tx.send(In::AppWritten { offset: 3 }).unwrap();
        match next_signal(&mut engine).await {
            Some(EngineSignal::Fatal(err)) => {
                assert!(err.to_string().contains("decreasing offset"))
            }
            other => panic!("expected fatal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_completes_when_both_fins_land() {
        let (mut engine, _app_in, mut app_out) = start_engine(0);
        engine
            .tx
            .send(In::AppRead {
                dir: 0,
                data: b"ab".to_vec(),
            })
            .unwrap();
        engine.tx.send(In::AppEof { dir: 0 }).unwrap();

        let mut frames = attach(&engine, 0, 0, false);
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 0,
                fin: false
            })
        ));
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Data { offset: 0, data }) if data == b"ab"
        ));

        // The peer acks the final byte with fin; the engine sends its Fin.
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 0,
                frame: Frame::Ack {
                    offset: 2,
                    fin: true,
                },
            })
            .unwrap();
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Fin { offset: 2 })
        ));

        // The peer sends data then fin; the engine writes them to the app.
        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 1,
                frame: Frame::Data {
                    offset: 0,
                    data: b"xy".to_vec(),
                },
            })
            .unwrap();
        assert_eq!(read_app_bytes(&mut app_out, 2).await, b"xy");
        timeout(T, engine.state.wait_for(|s| s.delivered == 2))
            .await
            .expect("app write not accounted")
            .expect("state channel closed");

        // The writer's progress report acks the delivered data.
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 2,
                fin: false
            })
        ));

        engine
            .tx
            .send(In::NetFrame {
                generation: 0,
                dir: 1,
                frame: Frame::Fin { offset: 2 },
            })
            .unwrap();
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Ack {
                offset: 2,
                fin: true
            })
        ));
        match next_signal(&mut engine).await {
            Some(EngineSignal::Done) => {}
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn engine_cancel_resets_and_signals() {
        let (mut engine, _app_in, _app_out) = start_engine(0);
        let mut frames = attach(&engine, 0, 0, false);
        let _ = next_frame(&mut frames).await;
        engine.tx.send(In::Cancel).unwrap();
        match next_signal(&mut engine).await {
            Some(EngineSignal::Cancelled) => {}
            other => panic!("expected Cancelled, got {other:?}"),
        }
        assert!(matches!(
            next_frame(&mut frames).await,
            Some(Frame::Reset {
                reason: RS_CANCELLED
            })
        ));
    }

    // Loopback tests: real QUIC connections between two in-process endpoints
    // (relay disabled, explicit loopback address), a fake echo "sshd" backend.

    /// Binds an endpoint to a fresh loopback UDP port and returns the address.
    async fn bound_loopback_endpoint() -> (Endpoint, SocketAddr) {
        use iroh::endpoint::{RelayMode, presets};
        let probe = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let endpoint = Endpoint::builder(presets::N0)
            .relay_mode(RelayMode::Disabled)
            .bind_addr(addr.to_string())
            .unwrap()
            .bind()
            .await
            .unwrap();
        (endpoint, addr)
    }

    /// Spawns a roost: endpoint, router, `PigeonsProtocol` against a fake
    /// echo backend. Returns the dial address and the backend accept count.
    async fn spawn_roost() -> (EndpointAddr, Arc<AtomicUsize>, Router) {
        let (roost, roost_addr) = bound_loopback_endpoint().await;
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ssh_port = backend.local_addr().unwrap().port();
        let accepts = Arc::new(AtomicUsize::new(0));
        let accepts_bg = accepts.clone();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = backend.accept().await.unwrap();
                accepts_bg.fetch_add(1, Ordering::Relaxed);
                let mut buf = [0u8; 4096];
                loop {
                    match sock.read(&mut buf).await.unwrap() {
                        0 => break,
                        n => {
                            sock.write_all(&buf[..n]).await.unwrap();
                        }
                    }
                }
            }
        });
        let router = Router::builder(roost.clone())
            .accept(PigeonsProtocol::ALPN, PigeonsProtocol::new(ssh_port))
            .spawn();
        (
            EndpointAddr::from_parts(roost.id(), [TransportAddr::Ip(roost_addr)]),
            accepts,
            router,
        )
    }

    /// Reads exactly `n` bytes from the client's app output.
    async fn app_reads(app_out: &mut DuplexStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        timeout(Duration::from_secs(15), app_out.read_exact(&mut buf))
            .await
            .expect("timed out waiting for app output")
            .expect("app output stream closed");
        buf
    }

    #[tokio::test]
    async fn loopback_session_survives_connection_loss() {
        let (roost_addr, accepts, _router) = spawn_roost().await;

        let (app_in, engine_read) = duplex(1 << 20);
        let (engine_write, mut app_out) = duplex(1 << 20);
        let engine = Engine::start(engine_read, engine_write, 0);
        let session_id = SecretKey::generate().to_bytes();

        // Generation 0: open.
        let (client, _) = bound_loopback_endpoint().await;
        let (connection, attach) = dial_and_handshake(
            &client,
            roost_addr.clone(),
            PigeonsProtocol::ALPN,
            &session_id,
            0,
            false,
            &engine,
        )
        .await
        .expect("open dial failed");
        let task = tokio::spawn(run_attachment(engine.clone(), attach, 0));
        let mut app_in = app_in;
        app_in.write_all(b"hello\n").await.unwrap();
        assert_eq!(app_reads(&mut app_out, 6).await, b"hello\n");

        // Connection loss: close the gen-0 connection, but keep the client
        // endpoint alive (a real fly client keeps its identity across
        // redials). The roost session must survive with its backend socket.
        connection.close(VarInt::from(0x0Cu32), b"test disconnect");
        let end = timeout(Duration::from_secs(15), task)
            .await
            .expect("old attachment did not end")
            .unwrap();
        assert!(
            matches!(end, AttachmentEnd::Detached),
            "expected Detached, got {end:?}"
        );

        // Generation 1: resume over the same endpoint (same peer id).
        let (_connection, attach) = dial_and_handshake(
            &client,
            roost_addr.clone(),
            PigeonsProtocol::ALPN,
            &session_id,
            1,
            true,
            &engine,
        )
        .await
        .expect("resume dial failed");
        let task = tokio::spawn(run_attachment(engine.clone(), attach, 1));
        app_in.write_all(b"world\n").await.unwrap();
        assert_eq!(app_reads(&mut app_out, 6).await, b"world\n");
        assert_eq!(
            accepts.load(Ordering::Relaxed),
            1,
            "resume must reuse the roost-side session and its backend socket"
        );

        // Completion: closing the app sends FIN; the echo backend sees EOF,
        // closes, and both engines complete.
        drop(app_in);
        let end = timeout(Duration::from_secs(15), task)
            .await
            .expect("resumed attachment did not end")
            .unwrap();
        assert!(
            matches!(end, AttachmentEnd::Done),
            "expected Done, got {end:?}"
        );
    }

    /// A connection drop with data in flight: unacked bytes written before
    /// the drop must be replayed on resume, in order, before the session
    /// completes.
    #[tokio::test]
    async fn loopback_resume_replays_in_flight_data() {
        let (roost_addr, accepts, _router) = spawn_roost().await;

        let (app_in, engine_read) = duplex(1 << 20);
        let (engine_write, mut app_out) = duplex(1 << 20);
        let engine = Engine::start(engine_read, engine_write, 0);
        let session_id = SecretKey::generate().to_bytes();

        let (client, _) = bound_loopback_endpoint().await;
        let (connection, attach) = dial_and_handshake(
            &client,
            roost_addr.clone(),
            PigeonsProtocol::ALPN,
            &session_id,
            0,
            false,
            &engine,
        )
        .await
        .expect("open dial failed");
        let task = tokio::spawn(run_attachment(engine.clone(), attach, 0));

        // Fill the pipe, then drop the connection before the roost has acked
        // everything.
        let mut app_in = app_in;
        for i in 1..=100 {
            app_in
                .write_all(format!("L{i}\n").as_bytes())
                .await
                .unwrap();
        }
        connection.close(VarInt::from(0x0Cu32), b"test disconnect");
        let end = timeout(Duration::from_secs(15), task)
            .await
            .expect("old attachment did not end")
            .unwrap();
        assert!(
            matches!(end, AttachmentEnd::Detached),
            "expected Detached, got {end:?}"
        );

        let (_connection, attach) = dial_and_handshake(
            &client,
            roost_addr.clone(),
            PigeonsProtocol::ALPN,
            &session_id,
            1,
            true,
            &engine,
        )
        .await
        .expect("resume dial failed");
        let task = tokio::spawn(run_attachment(engine.clone(), attach, 1));

        for i in 1..=100 {
            let line = format!("L{i}\n");
            assert_eq!(app_reads(&mut app_out, line.len()).await, line.as_bytes());
        }
        assert_eq!(
            accepts.load(Ordering::Relaxed),
            1,
            "resume must reuse the roost-side session and its backend socket"
        );

        drop(app_in);
        let end = timeout(Duration::from_secs(15), task)
            .await
            .expect("resumed attachment did not end")
            .unwrap();
        assert!(
            matches!(end, AttachmentEnd::Done),
            "expected Done, got {end:?}"
        );
    }

    /// Regression: the first dial that reaches the roost may carry a
    /// nonzero generation (earlier attempts failed before the handshake).
    /// The op is driven by whether the session was ever established, not
    /// by the generation counter — otherwise the client resumes a session
    /// the roost never created and is rejected forever.
    #[tokio::test]
    async fn loopback_first_dial_at_high_generation_opens() {
        let (roost_addr, accepts, _router) = spawn_roost().await;
        let (_app_in, engine_read) = duplex(1 << 20);
        let (engine_write, _app_out) = duplex(1 << 20);
        let engine = Engine::start(engine_read, engine_write, 0);
        let session_id = SecretKey::generate().to_bytes();
        let (client, _) = bound_loopback_endpoint().await;

        let (_connection, _attach) = dial_and_handshake(
            &client,
            roost_addr,
            PigeonsProtocol::ALPN,
            &session_id,
            7,
            false,
            &engine,
        )
        .await
        .expect("first dial must open the session even at generation > 0");
        assert_eq!(
            accepts.load(Ordering::Relaxed),
            1,
            "open must create the backend session"
        );
    }

    #[tokio::test]
    async fn loopback_wrong_alpn_is_a_protocol_rejection() {
        let (roost_addr, _accepts, _router) = spawn_roost().await;
        let (client, _) = bound_loopback_endpoint().await;
        let connecting = client
            .connect_with_opts(roost_addr, b"/pigeons/0", Default::default())
            .await
            .expect("dial setup failed");
        match timeout(Duration::from_secs(15), connecting)
            .await
            .expect("dial timed out")
        {
            Err(ConnectingError::ConnectionError { source, .. }) => {
                assert!(
                    is_protocol_rejection(&source),
                    "expected an application-level rejection, got {source:?}"
                );
            }
            other => panic!("expected a connection error, got {other:?}"),
        }
    }

    // Network-interruption test (manual, Linux-only, `#[ignore]`): runs a
    // real `pigeons roost` binary inside a docker container (isolated
    // network namespace, veth into the docker bridge), dials it from an
    // in-process fly client, drops the container's network link for 90 s
    // and restores it with a changed address, then verifies the session
    // reconnects and resumes byte-exactly.
    //
    // Needs docker on PATH, outbound internet for the container, and
    // working iroh relays. In labs where the default relay hostnames do
    // not resolve, set PIGEONS_TEST_RELAYS to a comma-separated list of
    // reachable relay URLs for the roost in the container. Takes ~4–5 min.

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual: requires docker + internet, ~5 min"]
    async fn network_namespace_reconnect() {
        use std::{
            env, io,
            process::{Command, id},
            thread,
        };

        use iroh::endpoint::presets;
        use tokio::io::{AsyncBufReadExt, BufReader};

        // Fly-side engine diagnostics (RUST_LOG, e.g. `warn,iroh_pigeons=debug`).
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(io::stderr)
            .try_init();

        const LINES: u64 = 100;
        const WARM_LINES: u64 = 5;
        const BATCH: u64 = 20;
        const DOWN: Duration = Duration::from_secs(90);
        const OUTER: Duration = Duration::from_secs(420);
        // Spaces the batches so the transfer is still in flight when the
        // flap lands, even on a fast relay: without it a sub-second
        // transfer can finish before eth0 goes down and the stall below
        // is never observed.
        const PACE: Duration = Duration::from_secs(30);

        fn docker(args: &[&str]) -> String {
            let out = Command::new("docker")
                .args(args)
                .output()
                .expect("docker on PATH");
            assert!(
                out.status.success(),
                "docker {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        // The built roost binary lives next to this test binary
        // (target/<profile>/pigeons).
        let exe = env::current_exe().expect("current exe");
        let bin = exe
            .parent()
            .expect("deps dir")
            .parent()
            .expect("target dir")
            .join("pigeons");
        assert!(bin.exists(), "pigeons binary not found at {bin:?}");

        // Optional lab-specific relays for the roost in the container; the
        // in-process fly uses iroh's default relays.
        let relay_args: Vec<String> = env::var("PIGEONS_TEST_RELAYS")
            .map(|v| {
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(|u| format!("--relay-url {u}"))
                    .collect()
            })
            .unwrap_or_default();

        // Fake "sshd": echo server on the ssh port (session traffic is a
        // byte bridge; no real sshd needed).
        let mut script = String::from(
            "set -e\n\
             export DEBIAN_FRONTEND=noninteractive\n\
             apt-get update -qq >/dev/null 2>&1 || true\n\
             apt-get install -y -qq --no-install-recommends iproute2 >/dev/null 2>&1 || true\n\
             mkdir -p /home/roost/.ssh\n\
             python3 - <<'PY' &\n\
             import socket, threading, sys\n\
             srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)\n\
             srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n\
             srv.bind(('127.0.0.1', 22222))\n\
             srv.listen(4)\n\
             def handle(c):\n\
             \ttry:\n\
             \t\twhile True:\n\
             \t\t\td = c.recv(4096)\n\
             \t\t\tif not d:\n\
             \t\t\t\tbreak\n\
             \t\t\tc.sendall(d)\n\
             \tfinally:\n\
             \t\tc.close()\n\
             while True:\n\
             \tc, _ = srv.accept()\n\
             \tthreading.Thread(target=handle, args=(c,), daemon=True).start()\n\
             PY\n\
             sleep 1\n\
             exec env HOME=/home/roost RUST_LOG=debug /pigeons roost --ssh-port 22222 ",
        );
        script.push_str(&relay_args.join(" "));

        let name = format!("pigeons-nettest-{}", id());
        struct Guard(String);
        impl Drop for Guard {
            fn drop(&mut self) {
                if thread::panicking() {
                    // SIGTERM (2s) so the roost flushes buffered session
                    // diagnostics before the log is read; SIGKILL loses them.
                    let _ = Command::new("docker")
                        .args(["stop", "-t", "2", &self.0])
                        .output();
                    if let Ok(out) = Command::new("docker").args(["logs", &self.0]).output() {
                        // Merge both docker streams: the container's stderr
                        // (roost tracing) arrives on the CLI's stderr here, so
                        // reading stdout alone would drop every session line.
                        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
                        combined.push_str(&String::from_utf8_lossy(&out.stderr));
                        // Keep pigeons session/protocol lines plus task
                        // panics/aborts; the rest is iroh/rustls/noq noise.
                        let keep: Vec<&str> = combined
                            .lines()
                            .filter(|l| {
                                l.contains("iroh_pigeons")
                                    || l.contains("panic")
                                    || l.contains("Aborting")
                            })
                            .collect();
                        eprintln!(
                            "--- container pigeons logs ({} lines) ---\n{}",
                            keep.len(),
                            keep.join("\n")
                        );
                    }
                }
                let _ = Command::new("docker").args(["rm", "-f", &self.0]).output();
            }
        }
        let _guard = Guard(name.clone());

        docker(&[
            "run",
            "-d",
            "--cap-add=NET_ADMIN",
            "--name",
            name.as_str(),
            "-v",
            format!("{}:/pigeons:ro", bin.display()).as_str(),
            "python:3.13-slim",
            "bash",
            "-c",
            &script,
        ]);

        // Wait for the roost to report its endpoint id.
        let started = Instant::now();
        let id: iroh::EndpointId = loop {
            let logs = docker(&["logs", name.as_str()]);
            if let Some(pos) = logs.find("roost is running! id: ")
                && let Ok(id) = logs.get(pos + 22..pos + 22 + 64).unwrap_or("").parse()
            {
                break id;
            }
            assert!(
                started.elapsed() < Duration::from_secs(240),
                "roost did not start; container logs:\n{logs}"
            );
            thread::sleep(Duration::from_secs(2));
        };
        eprintln!("nettest: roost endpoint {id}");

        // In-process fly: dial by endpoint id (discovery + relay rendezvous).
        let endpoint = iroh::Endpoint::bind(presets::N0)
            .await
            .expect("bind fly endpoint");
        let (mut app_in, engine_read) = duplex(1 << 20);
        let (engine_write, app_out) = duplex(1 << 20);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let fly = tokio::spawn({
            let endpoint = endpoint.clone();
            async move {
                run_client(
                    &endpoint,
                    id,
                    PigeonsProtocol::ALPN,
                    engine_read,
                    engine_write,
                    cancel_rx,
                )
                .await
            }
        });

        // Interruption: once WARM_LINES have round-tripped, drop the
        // container's only network link for DOWN, then restore it with a
        // changed address — the roost must re-publish and the fly must
        // re-dial.
        let (lines_tx, mut lines_rx) = watch::channel(0u64);
        let interrupt_name = name.clone();
        let interrupt = tokio::spawn(async move {
            while *lines_rx.borrow_and_update() < WARM_LINES {
                if lines_rx.changed().await.is_err() {
                    return;
                }
            }
            let show = docker(&[
                "exec",
                interrupt_name.as_str(),
                "ip",
                "-o",
                "-4",
                "addr",
                "show",
                "eth0",
            ]);
            let cidr = show
                .lines()
                .find_map(|l| {
                    let t: Vec<&str> = l.split_whitespace().collect();
                    t.iter()
                        .position(|&x| x == "inet")
                        .and_then(|p| t.get(p + 1).copied())
                })
                .expect("container eth0 has an IPv4 address");
            // Deleting the address removes the default route; capture it to
            // restore after the IP change (without it the container has no
            // egress and the roost can never re-reach the relay).
            let default_route = docker(&[
                "exec",
                interrupt_name.as_str(),
                "ip",
                "route",
                "show",
                "default",
            ]);
            docker(&[
                "exec",
                interrupt_name.as_str(),
                "ip",
                "link",
                "set",
                "eth0",
                "down",
            ]);
            sleep(DOWN).await;
            let (ip, prefix) = cidr.split_once('/').unwrap_or((cidr, "16"));
            let new_ip = ip
                .rsplit_once('.')
                .map(|(h, _)| format!("{h}.254"))
                .unwrap_or_else(|| "172.17.0.254".into());
            let recover = format!(
                "ip link set eth0 up && ip addr del {cidr} dev eth0 \
                 && ip addr add {new_ip}/{prefix} dev eth0 \
                 && ip route replace {default_route}"
            );
            docker(&[
                "exec",
                interrupt_name.as_str(),
                "sh",
                "-c",
                recover.as_str(),
            ]);
        });

        // Transfer: LINES byte-exact lines through the tunnel, written in
        // bursts of BATCH so the transfer is not bound by per-line relay
        // round trips, and paced by PACE between bursts so the transfer is
        // still in flight when the flap lands, even on a fast relay. Lines
        // are buffered on the client while the link is down and flush once
        // the session resumes.
        let (fly_res, max_stall) = timeout(OUTER, async {
            let mut buf = BufReader::new(app_out);
            let mut last = Instant::now();
            let mut max_stall = Duration::ZERO;
            for start in (1..=LINES).step_by(BATCH as usize) {
                let end = (start + BATCH).min(LINES + 1);
                for i in start..end {
                    app_in
                        .write_all(format!("L{i}\n").as_bytes())
                        .await
                        .expect("write line");
                }
                for i in start..end {
                    let mut line = String::new();
                    timeout(Duration::from_secs(300), buf.read_line(&mut line))
                        .await
                        .expect("line timed out after network recovery")
                        .expect("stream closed before echo");
                    assert_eq!(line, format!("L{i}\n"));
                    let now = Instant::now();
                    max_stall = max_stall.max(now - last);
                    last = now;
                    let _ = lines_tx.send(i);
                }
                // Pace between bursts (not after the last) so the transfer
                // spans the flap even on a fast relay.
                if end < LINES + 1 {
                    sleep(PACE).await;
                }
            }
            drop(app_in); // EOF: echo backend closes, session completes
            (fly.await.expect("fly task panicked"), max_stall)
        })
        .await
        .expect("network test exceeded its overall deadline");
        fly_res.expect("run_client reported an error");
        interrupt.await.expect("interrupt task panicked");

        assert!(
            max_stall >= Duration::from_secs(80),
            "max stall {max_stall:?}: the 90 s network interruption was not observed"
        );
    }
}
