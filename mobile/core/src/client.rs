//! The connection to one roost and everything that runs over it.
//!
//! A [`Client`] owns an iroh endpoint for the life of the app and, while
//! connected, one SSH session to the configured roost. Short commands (listing
//! sessions, killing one, installing the key) run on exec channels and report
//! back through [`Events::result`]; terminals are PTY channels that stream
//! bytes both ways until tmux exits.
//!
//! Every public method returns immediately. Work happens on the tokio runtime
//! the client was created on, and completes through the [`Events`] callbacks,
//! which are invoked from runtime threads. The FFI layer in [`crate::ffi`]
//! adapts those to C function pointers; tests implement the trait directly.

use std::{
    collections::HashMap,
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use iroh::EndpointId;
use iroh_pigeons::Tunnel;
use russh::{
    ChannelMsg, Disconnect,
    client::{self, DisconnectReason, Handle},
    keys::{PrivateKey, PrivateKeyWithHashAlg, ssh_key},
};
use russh_sftp::client::SftpSession;
use serde::Serialize;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    runtime,
    sync::mpsc,
    time,
};

use crate::{
    files::{self, DirEntry, DirListing, EntryKind, FileContent},
    identity, shell, tmux,
};

/// Identifies one asynchronous request; its outcome arrives in
/// [`Events::result`] with the same id.
pub type RequestId = u64;
/// Identifies one open terminal.
pub type TerminalId = u64;

/// Where the connection to the roost stands. The discriminants are the values
/// crossing the C ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ConnectionState {
    Disconnected = 0,
    /// Dialing the roost over iroh.
    Dialing = 1,
    /// SSH handshake and authentication.
    Authenticating = 2,
    Connected = 3,
    /// The last connect attempt failed; the detail says why.
    Failed = 4,
}

/// How a terminal ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalExit {
    /// Exit status of the remote command, when sshd reported one.
    pub exit_code: Option<u32>,
    /// Set when the terminal ended because of an error rather than the
    /// command finishing.
    pub error: Option<String>,
}

/// Callbacks from the client. All methods are called from runtime threads and
/// must not block for long.
pub trait Events: Send + Sync + 'static {
    fn state(&self, state: ConnectionState, detail: Option<&str>);
    /// `Ok` carries the request's payload (JSON for structured requests),
    /// `Err` a human-readable message.
    fn result(&self, request: RequestId, result: Result<String, String>);
    fn terminal_output(&self, terminal: TerminalId, bytes: &[u8]);
    fn terminal_exit(&self, terminal: TerminalId, exit: TerminalExit);
}

/// User-configurable knobs that shape the commands sent to the roost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// `work.sh`'s `$CODE_DIR`, as a remote path. `~` expands remotely.
    pub code_dir: String,
    /// Where `work.sh` lives on the roost.
    pub work_script: String,
    /// Turn on tmux's mouse mode when attaching, so dragging scrolls panes.
    pub tmux_mouse: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            code_dir: "~/code".to_string(),
            work_script: "~/work.sh".to_string(),
            tmux_mouse: true,
        }
    }
}

/// Output of a command run with [`Client::exec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecOutput {
    pub exit_code: Option<u32>,
    pub stdout: String,
    pub stderr: String,
}

const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Handle to the core. Cheap to clone; all clones share one endpoint and
/// connection.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("endpoint_id", &self.endpoint_id())
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

struct Inner {
    rt: runtime::Handle,
    tunnel: Tunnel,
    ssh_key: Arc<PrivateKey>,
    events: Arc<dyn Events>,
    settings: Mutex<Settings>,
    state: Mutex<ConnectionState>,
    ssh: Mutex<Option<Arc<Handle<SshHandler>>>>,
    /// Opened on first use, dropped with the SSH session.
    sftp: Mutex<Option<Arc<SftpSession>>>,
    /// Serialises SFTP session setup so concurrent first uses share one.
    sftp_setup: tokio::sync::Mutex<()>,
    /// Bumped by every connect and disconnect, so a late failure from an
    /// abandoned connection cannot overwrite the state of the current one.
    generation: AtomicU64,
    terminals: Mutex<HashMap<TerminalId, mpsc::UnboundedSender<TermCmd>>>,
    next_id: AtomicU64,
}

enum TermCmd {
    Write(Vec<u8>),
    Resize(u16, u16),
    Close,
}

impl Client {
    /// Load (or create) the identity in `data_dir` and bind the iroh
    /// endpoint. Must be called on a tokio runtime; that runtime runs all the
    /// client's work afterwards.
    pub async fn new(data_dir: PathBuf, events: Arc<dyn Events>) -> Result<Self> {
        let builder = Tunnel::builder_from_ssh_dir(data_dir)
            .await
            .context("loading identity")?;
        let ssh_key = Arc::new(identity::ssh_private_key(&builder.secret_key));
        let tunnel = builder.build().await.context("binding iroh endpoint")?;
        tracing::info!("core ready, endpoint id {}", tunnel.endpoint().id());
        Ok(Self {
            inner: Arc::new(Inner {
                rt: runtime::Handle::current(),
                tunnel,
                ssh_key,
                events,
                settings: Mutex::new(Settings::default()),
                state: Mutex::new(ConnectionState::Disconnected),
                ssh: Mutex::new(None),
                sftp: Mutex::new(None),
                sftp_setup: tokio::sync::Mutex::new(()),
                generation: AtomicU64::new(0),
                terminals: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }),
        })
    }

    /// This device's iroh endpoint ID.
    pub fn endpoint_id(&self) -> EndpointId {
        self.inner.tunnel.endpoint().id()
    }

    /// The `authorized_keys` line that lets this device in.
    pub fn authorized_key(&self) -> Result<String> {
        identity::authorized_key_line(&self.inner.ssh_key)
    }

    pub fn settings(&self) -> Settings {
        self.inner
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_settings(&self, settings: Settings) {
        *self
            .inner
            .settings
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = settings;
    }

    pub fn state(&self) -> ConnectionState {
        *self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Dial `roost` and log in as `user`. The device key is tried first; when
    /// it is rejected and a password was given, password auth is tried next.
    /// Progress and the outcome arrive through [`Events::state`].
    pub fn connect(&self, roost: EndpointId, user: String, password: Option<String>) {
        let inner = self.inner.clone();
        let generation = inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.inner.rt.spawn(async move {
            inner.drop_session().await;
            if let Err(e) = inner
                .connect(generation, roost, &user, password.as_deref())
                .await
                && inner.generation.load(Ordering::SeqCst) == generation
            {
                tracing::warn!("connect failed: {e:#}");
                inner.set_state(ConnectionState::Failed, Some(&format!("{e:#}")));
            }
        });
    }

    /// Close the SSH session and every terminal on it.
    pub fn disconnect(&self) {
        let inner = self.inner.clone();
        inner.generation.fetch_add(1, Ordering::SeqCst);
        self.inner.rt.spawn(async move {
            inner.drop_session().await;
            inner.set_state(ConnectionState::Disconnected, None);
        });
    }

    /// Run a command on the roost. The result payload is an [`ExecOutput`]
    /// as JSON.
    pub fn exec(&self, command: String) -> RequestId {
        self.request(move |inner| async move {
            let out = inner.run(&command).await?;
            Ok(serde_json::to_string(&out)?)
        })
    }

    /// List tmux sessions and the projects in the code directory. The
    /// payload is a [`tmux::Overview`] as JSON.
    pub fn list_sessions(&self) -> RequestId {
        self.request(|inner| async move {
            let code_dir = inner.settings().code_dir;
            let out = inner.run(&tmux::overview_command(&code_dir)).await?;
            if !tmux::is_no_server(&out.stderr) {
                bail!("{}", out.stderr.trim());
            }
            let overview = tmux::parse_overview(&out.stdout);
            tracing::debug!(
                "{} sessions, {} projects",
                overview.sessions.len(),
                overview.projects.len()
            );
            Ok(serde_json::to_string(&overview)?)
        })
    }

    /// List a project's worktrees. The payload is a JSON array of
    /// [`tmux::Worktree`].
    pub fn list_worktrees(&self, project: String) -> RequestId {
        self.request(move |inner| async move {
            let code_dir = inner.settings().code_dir;
            let out = inner
                .run(&tmux::worktrees_command(&code_dir, &project))
                .await?;
            if out.exit_code != Some(0) {
                bail!("{}", out.stderr.trim());
            }
            Ok(serde_json::to_string(&tmux::parse_worktrees(
                &out.stdout,
                &project,
            ))?)
        })
    }

    /// Kill a tmux session. The payload is an [`ExecOutput`] as JSON.
    pub fn kill_session(&self, session: String) -> RequestId {
        self.exec(tmux::kill_session_command(&session))
    }

    /// Append this device's key to `authorized_keys` on the roost, for use
    /// after a password login. The payload is an [`ExecOutput`] as JSON.
    pub fn install_authorized_key(&self) -> RequestId {
        match self.authorized_key() {
            Ok(line) => self.exec(tmux::install_key_command(&line)),
            Err(e) => self.request(move |_| async move { Err(e) }),
        }
    }

    /// Make a path absolute and canonical, expanding a leading `~`. The
    /// payload is `{"path": "..."}`.
    pub fn resolve_path(&self, path: String) -> RequestId {
        self.request(move |inner| async move {
            let (sftp, path) = inner.sftp_path(&path).await?;
            let path = sftp.canonicalize(path).await.context("resolving path")?;
            Ok(serde_json::to_string(&serde_json::json!({ "path": path }))?)
        })
    }

    /// List a directory. The payload is a [`DirListing`] as JSON.
    pub fn list_dir(&self, path: String) -> RequestId {
        self.request(move |inner| async move {
            let (sftp, path) = inner.sftp_path(&path).await?;
            let path = sftp
                .canonicalize(path)
                .await
                .context("resolving directory")?;
            let mut entries = Vec::new();
            for entry in sftp.read_dir(&path).await.context("listing directory")? {
                let name = entry.file_name();
                let meta = entry.metadata();
                let symlink = meta.file_type().is_symlink();
                let (kind, size, modified) = if symlink {
                    // Describe what the link points at; dangling links are "other".
                    match sftp.metadata(format!("{path}/{name}")).await {
                        Ok(target) => (
                            kind_of(target.is_dir(), target.is_regular()),
                            target.len(),
                            unix_time(&target),
                        ),
                        Err(_) => (EntryKind::Other, 0, None),
                    }
                } else {
                    (
                        kind_of(meta.is_dir(), meta.is_regular()),
                        meta.len(),
                        unix_time(&meta),
                    )
                };
                entries.push(DirEntry {
                    name,
                    kind,
                    symlink,
                    size,
                    modified,
                });
            }
            files::sort_entries(&mut entries);
            tracing::debug!("listed {path}: {} entries", entries.len());
            Ok(serde_json::to_string(&DirListing { path, entries })?)
        })
    }

    /// Read up to `max_bytes` of a file. The payload is a [`FileContent`]
    /// as JSON.
    pub fn read_file(&self, path: String, max_bytes: u64) -> RequestId {
        self.request(move |inner| async move {
            let (sftp, path) = inner.sftp_path(&path).await?;
            let path = sftp.canonicalize(path).await.context("resolving file")?;
            let size = sftp.metadata(&path).await.context("stat")?.len();
            let file = sftp.open(&path).await.context("opening file")?;
            let mut bytes = Vec::with_capacity(size.min(max_bytes) as usize);
            file.take(max_bytes)
                .read_to_end(&mut bytes)
                .await
                .context("reading file")?;
            let binary = files::is_binary(&bytes);
            let truncated = size > bytes.len() as u64;
            tracing::debug!("read {path}: {} of {size} bytes", bytes.len());
            Ok(serde_json::to_string(&FileContent {
                path,
                size,
                truncated,
                binary,
                text: if binary {
                    String::new()
                } else {
                    String::from_utf8_lossy(&bytes).into_owned()
                },
            })?)
        })
    }

    /// Open a terminal attached to a tmux session.
    pub fn attach_session(&self, session: String, cols: u16, rows: u16) -> TerminalId {
        let mouse = self.settings().tmux_mouse;
        self.open_terminal(Some(tmux::attach_command(&session, mouse)), cols, rows)
    }

    /// Open a terminal running `work.sh <project> <tree>`, which creates the
    /// worktree and its session when needed and attaches.
    pub fn open_worktree(&self, project: String, tree: String, cols: u16, rows: u16) -> TerminalId {
        let work_script = self.settings().work_script;
        self.open_terminal(
            Some(tmux::work_command(&work_script, &project, &tree)),
            cols,
            rows,
        )
    }

    /// Open a plain login shell.
    pub fn open_shell(&self, cols: u16, rows: u16) -> TerminalId {
        self.open_terminal(None, cols, rows)
    }

    /// Send keystrokes to a terminal.
    pub fn terminal_write(&self, terminal: TerminalId, bytes: Vec<u8>) {
        self.inner.send_term(terminal, TermCmd::Write(bytes));
    }

    pub fn terminal_resize(&self, terminal: TerminalId, cols: u16, rows: u16) {
        self.inner.send_term(terminal, TermCmd::Resize(cols, rows));
    }

    /// Close a terminal. [`Events::terminal_exit`] still fires.
    pub fn terminal_close(&self, terminal: TerminalId) {
        self.inner.send_term(terminal, TermCmd::Close);
    }

    fn request<F, Fut>(&self, f: F) -> RequestId
    where
        F: FnOnce(Arc<Inner>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<String>> + Send,
    {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let inner = self.inner.clone();
        self.inner.rt.spawn(async move {
            let result = f(inner.clone()).await.map_err(|e| format!("{e:#}"));
            if let Err(e) = &result {
                tracing::warn!("request {id} failed: {e}");
            }
            inner.events.result(id, result);
        });
        id
    }

    fn open_terminal(&self, command: Option<String>, cols: u16, rows: u16) -> TerminalId {
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner.terminals().insert(id, tx);
        let inner = self.inner.clone();
        self.inner.rt.spawn(async move {
            let exit = match inner.run_terminal(id, command, cols, rows, rx).await {
                Ok(exit_code) => TerminalExit {
                    exit_code,
                    error: None,
                },
                Err(e) => {
                    tracing::warn!("terminal {id} failed: {e:#}");
                    TerminalExit {
                        exit_code: None,
                        error: Some(format!("{e:#}")),
                    }
                }
            };
            inner.terminals().remove(&id);
            tracing::debug!("terminal {id} exited: {exit:?}");
            inner.events.terminal_exit(id, exit);
        });
        id
    }
}

impl Inner {
    fn settings(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn terminals(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<TerminalId, mpsc::UnboundedSender<TermCmd>>> {
        self.terminals.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_state(&self, state: ConnectionState, detail: Option<&str>) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
        tracing::info!("state: {state:?} {}", detail.unwrap_or(""));
        self.events.state(state, detail);
    }

    /// The SFTP session for the current connection, opened on first use.
    async fn sftp(&self) -> Result<Arc<SftpSession>> {
        let _setup = self.sftp_setup.lock().await;
        if let Some(sftp) = self.sftp.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(sftp);
        }
        let handle = self.session()?;
        let channel = handle
            .channel_open_session()
            .await
            .context("opening sftp channel")?;
        channel
            .request_subsystem(true, "sftp")
            .await
            .context("requesting the sftp subsystem (is it enabled in sshd_config?)")?;
        let sftp = Arc::new(
            SftpSession::new(channel.into_stream())
                .await
                .context("starting sftp")?,
        );
        tracing::debug!("sftp session opened");
        *self.sftp.lock().unwrap_or_else(|e| e.into_inner()) = Some(sftp.clone());
        Ok(sftp)
    }

    /// The SFTP session plus `path` with a leading `~` expanded to the
    /// login user's home directory.
    async fn sftp_path(&self, path: &str) -> Result<(Arc<SftpSession>, String)> {
        let sftp = self.sftp().await?;
        let path = if path == "~" || path.starts_with("~/") {
            let home = sftp.canonicalize(".").await.context("finding home")?;
            files::expand_home(path, &home)
        } else {
            path.to_string()
        };
        Ok((sftp, path))
    }

    fn drop_sftp(&self) {
        self.sftp.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    fn session(&self) -> Result<Arc<Handle<SshHandler>>> {
        self.ssh
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| anyhow!("not connected to the roost"))
    }

    fn send_term(&self, terminal: TerminalId, cmd: TermCmd) {
        if let Some(tx) = self.terminals().get(&terminal) {
            let _ = tx.send(cmd);
        }
    }

    async fn connect(
        self: &Arc<Self>,
        generation: u64,
        roost: EndpointId,
        user: &str,
        password: Option<&str>,
    ) -> Result<()> {
        self.set_state(ConnectionState::Dialing, Some(&roost.to_string()));
        let stream = time::timeout(DIAL_TIMEOUT, self.tunnel.open_stream(roost))
            .await
            .map_err(|_| anyhow!("timed out dialing the roost"))??;
        self.set_state(ConnectionState::Authenticating, None);

        let config = Arc::new(client::Config {
            inactivity_timeout: None,
            keepalive_interval: Some(Duration::from_secs(15)),
            keepalive_max: 4,
            ..Default::default()
        });
        let handler = SshHandler {
            inner: Arc::downgrade(self),
            generation,
        };
        let mut handle = client::connect_stream(config, stream, handler)
            .await
            .context("ssh handshake")?;

        let key = PrivateKeyWithHashAlg::new(self.ssh_key.clone(), None);
        let mut auth = handle
            .authenticate_publickey(user, key)
            .await
            .context("public key authentication")?;
        if !auth.success()
            && let Some(password) = password
        {
            tracing::info!("device key rejected, trying password");
            auth = handle
                .authenticate_password(user, password)
                .await
                .context("password authentication")?;
        }
        if !auth.success() {
            let _ = handle
                .disconnect(Disconnect::ByApplication, "authentication failed", "en")
                .await;
            bail!(
                "the roost rejected this device's key for {user}. Add the key shown in Settings to ~/.ssh/authorized_keys on the roost, or connect once with a password"
            );
        }

        self.drop_sftp();
        *self.ssh.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(handle));
        self.set_state(ConnectionState::Connected, None);
        Ok(())
    }

    fn on_disconnected(&self, generation: u64, detail: &str) {
        if self.generation.load(Ordering::SeqCst) != generation {
            return;
        }
        self.ssh.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.drop_sftp();
        for (_, tx) in self.terminals().drain() {
            let _ = tx.send(TermCmd::Close);
        }
        self.set_state(ConnectionState::Disconnected, Some(detail));
    }

    async fn drop_session(&self) {
        let handle = self.ssh.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.drop_sftp();
        for (_, tx) in self.terminals().drain() {
            let _ = tx.send(TermCmd::Close);
        }
        if let Some(handle) = handle {
            let _ = time::timeout(
                DISCONNECT_TIMEOUT,
                handle.disconnect(Disconnect::ByApplication, "", "en"),
            )
            .await;
        }
    }

    /// Run `snippet` under the user's login shell and collect its output.
    async fn run(&self, snippet: &str) -> Result<ExecOutput> {
        let handle = self.session()?;
        let mut channel = handle
            .channel_open_session()
            .await
            .context("opening channel")?;
        channel
            .exec(true, shell::in_login_shell(snippet))
            .await
            .context("starting command")?;
        let collect = async {
            let mut out = ExecOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: String::new(),
            };
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            loop {
                match channel.wait().await {
                    Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
                    Some(ChannelMsg::ExtendedData { data, ext: 1 }) => {
                        stderr.extend_from_slice(&data)
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => {
                        out.exit_code = Some(exit_status)
                    }
                    Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                }
            }
            out.stdout = String::from_utf8_lossy(&stdout).into_owned();
            out.stderr = String::from_utf8_lossy(&stderr).into_owned();
            Ok(out)
        };
        time::timeout(EXEC_TIMEOUT, collect)
            .await
            .map_err(|_| anyhow!("command timed out after {}s", EXEC_TIMEOUT.as_secs()))?
    }

    /// Run a PTY channel until the remote command exits or the app closes it.
    async fn run_terminal(
        &self,
        id: TerminalId,
        command: Option<String>,
        cols: u16,
        rows: u16,
        mut rx: mpsc::UnboundedReceiver<TermCmd>,
    ) -> Result<Option<u32>> {
        let handle = self.session()?;
        let mut channel = handle
            .channel_open_session()
            .await
            .context("opening channel")?;
        channel
            .request_pty(
                true,
                tmux::TERM,
                u32::from(cols),
                u32::from(rows),
                0,
                0,
                &[],
            )
            .await
            .context("requesting pty")?;
        match command {
            Some(command) => {
                tracing::debug!("terminal {id}: {command}");
                channel
                    .exec(true, shell::in_login_shell(&command))
                    .await
                    .context("starting command")?
            }
            None => channel
                .request_shell(true)
                .await
                .context("starting shell")?,
        }
        let mut writer = channel.make_writer();
        let mut exit_code = None;
        loop {
            tokio::select! {
                msg = channel.wait() => match msg {
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        self.events.terminal_output(id, &data);
                    }
                    Some(ChannelMsg::ExitStatus { exit_status }) => exit_code = Some(exit_status),
                    Some(ChannelMsg::Close) | None => break,
                    Some(_) => {}
                },
                cmd = rx.recv() => match cmd {
                    Some(TermCmd::Write(bytes)) => {
                        writer.write_all(&bytes).await.context("writing to terminal")?;
                        writer.flush().await.context("flushing terminal")?;
                    }
                    Some(TermCmd::Resize(cols, rows)) => {
                        channel
                            .window_change(u32::from(cols), u32::from(rows), 0, 0)
                            .await
                            .context("resizing terminal")?;
                    }
                    Some(TermCmd::Close) | None => {
                        let _ = channel.close().await;
                        break;
                    }
                },
            }
        }
        Ok(exit_code)
    }
}

/// russh's view of the connection. Holds a weak reference so a connection
/// that outlives its client (briefly, during shutdown) cannot keep it alive.
struct SshHandler {
    inner: Weak<Inner>,
    generation: u64,
}

impl client::Handler for SshHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        // The roost was dialed by its iroh endpoint ID, which is the public
        // key the QUIC connection is authenticated against. Whatever answers
        // on that stream is the roost's sshd by construction, so the SSH host
        // key cannot add anything a known_hosts check would.
        Ok(true)
    }

    async fn disconnected(
        &mut self,
        reason: DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        let detail = match &reason {
            DisconnectReason::ReceivedDisconnect(info) => {
                format!("roost closed the connection: {info:?}")
            }
            DisconnectReason::Error(e) => format!("connection lost: {e:#}"),
        };
        if let Some(inner) = self.inner.upgrade() {
            inner.on_disconnected(self.generation, &detail);
        }
        match reason {
            DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            DisconnectReason::Error(e) => Err(e),
        }
    }
}

fn kind_of(is_dir: bool, is_regular: bool) -> EntryKind {
    if is_dir {
        EntryKind::Dir
    } else if is_regular {
        EntryKind::File
    } else {
        EntryKind::Other
    }
}

fn unix_time(meta: &russh_sftp::client::fs::Metadata) -> Option<i64> {
    meta.mtime.map(i64::from)
}
