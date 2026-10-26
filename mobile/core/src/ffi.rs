//! The C ABI the iOS app binds to.
//!
//! Declared for Swift in `include/pigeons_mobile.h`; the two must be kept in
//! step by hand. The shape is deliberately small:
//!
//! - one opaque [`pm_client`] created with [`pm_client_new`] and released with
//!   [`pm_client_free`];
//! - a [`pm_callbacks`] table the app fills in once, through which every
//!   asynchronous outcome is delivered, from runtime threads;
//! - request functions that return an id immediately and complete through
//!   `on_result` with a JSON payload;
//! - terminal functions keyed by a terminal id, with raw bytes both ways.
//!
//! Strings passed in are borrowed for the duration of the call and must be
//! NUL-terminated UTF-8. Strings returned as `char *` are owned by the caller
//! and freed with [`pm_string_free`]. Strings passed to callbacks are valid
//! only for the duration of the callback.
//!
//! Callbacks never fire after [`pm_client_free`] returns.

#![allow(
    non_camel_case_types,
    clippy::missing_safety_doc,
    reason = "C-facing names and the module-level contract documented above"
)]

use std::{
    ffi::{CStr, CString, c_char, c_void},
    path::PathBuf,
    ptr,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, anyhow};
use iroh::EndpointId;
use tokio::runtime::Runtime;
use tracing::Level;

use crate::{
    client::{Client, ConnectionState, Events, RequestId, Settings, TerminalExit, TerminalId},
    logging,
};

/// Connection states, matching [`ConnectionState`].
pub const PM_STATE_DISCONNECTED: i32 = ConnectionState::Disconnected as i32;
pub const PM_STATE_DIALING: i32 = ConnectionState::Dialing as i32;
pub const PM_STATE_AUTHENTICATING: i32 = ConnectionState::Authenticating as i32;
pub const PM_STATE_CONNECTED: i32 = ConnectionState::Connected as i32;
pub const PM_STATE_FAILED: i32 = ConnectionState::Failed as i32;

/// Log levels passed to `on_log`.
pub const PM_LOG_ERROR: i32 = 1;
pub const PM_LOG_WARN: i32 = 2;
pub const PM_LOG_INFO: i32 = 3;
pub const PM_LOG_DEBUG: i32 = 4;
pub const PM_LOG_TRACE: i32 = 5;

/// Callback table. Any pointer may be null to ignore that event.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct pm_callbacks {
    /// Passed back as the first argument of every callback.
    pub ctx: *mut c_void,
    /// `detail` is nullable.
    pub on_state: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    /// `payload` is JSON when `ok`, an error message otherwise.
    pub on_result: Option<unsafe extern "C" fn(*mut c_void, u64, bool, *const c_char)>,
    pub on_terminal_output: Option<unsafe extern "C" fn(*mut c_void, u64, *const u8, usize)>,
    /// `exit_code` is -1 when unknown; `error` is nullable.
    pub on_terminal_exit: Option<unsafe extern "C" fn(*mut c_void, u64, i32, *const c_char)>,
    pub on_log: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
}

/// Mirror of [`Settings`]. Null strings keep the default.
#[repr(C)]
#[derive(Debug)]
pub struct pm_settings {
    pub code_dir: *const c_char,
    pub work_script: *const c_char,
    pub tmux_mouse: bool,
}

/// Opaque client handle.
pub struct pm_client {
    runtime: Runtime,
    client: Client,
    events: Arc<CEvents>,
}

impl std::fmt::Debug for pm_client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("pm_client")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

/// Adapts [`Events`] to the C callback table.
struct CEvents {
    cb: pm_callbacks,
    closed: AtomicBool,
}

// The table holds raw pointers the host promises are safe to use from any
// thread until `pm_client_free`; that promise is part of the ABI contract.
unsafe impl Send for CEvents {}
unsafe impl Sync for CEvents {}

impl CEvents {
    fn live(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }
}

/// A `CString` for `s`, dropping interior NULs rather than failing.
fn c_string(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("interior NULs removed")
}

fn opt_ptr(s: &Option<CString>) -> *const c_char {
    s.as_ref().map_or(ptr::null(), |c| c.as_ptr())
}

impl Events for CEvents {
    fn state(&self, state: ConnectionState, detail: Option<&str>) {
        if let Some(f) = self.cb.on_state
            && self.live()
        {
            let detail = detail.map(c_string);
            unsafe { f(self.cb.ctx, state as i32, opt_ptr(&detail)) }
        }
    }

    fn result(&self, request: RequestId, result: Result<String, String>) {
        if let Some(f) = self.cb.on_result
            && self.live()
        {
            let (ok, payload) = match result {
                Ok(p) => (true, p),
                Err(e) => (false, e),
            };
            let payload = c_string(&payload);
            unsafe { f(self.cb.ctx, request, ok, payload.as_ptr()) }
        }
    }

    fn terminal_output(&self, terminal: TerminalId, bytes: &[u8]) {
        if let Some(f) = self.cb.on_terminal_output
            && self.live()
        {
            unsafe { f(self.cb.ctx, terminal, bytes.as_ptr(), bytes.len()) }
        }
    }

    fn terminal_exit(&self, terminal: TerminalId, exit: TerminalExit) {
        if let Some(f) = self.cb.on_terminal_exit
            && self.live()
        {
            let code = exit
                .exit_code
                .map_or(-1, |c| i32::try_from(c).unwrap_or(-1));
            let error = exit.error.as_deref().map(c_string);
            unsafe { f(self.cb.ctx, terminal, code, opt_ptr(&error)) }
        }
    }
}

impl CEvents {
    fn log(&self, level: Level, line: &str) {
        if let Some(f) = self.cb.on_log
            && self.live()
        {
            let level = match level {
                Level::ERROR => PM_LOG_ERROR,
                Level::WARN => PM_LOG_WARN,
                Level::INFO => PM_LOG_INFO,
                Level::DEBUG => PM_LOG_DEBUG,
                Level::TRACE => PM_LOG_TRACE,
            };
            let line = c_string(line);
            unsafe { f(self.cb.ctx, level, line.as_ptr()) }
        }
    }
}

/// Borrow a C string argument as `&str`.
unsafe fn arg<'a>(ptr: *const c_char, name: &str) -> Result<&'a str> {
    if ptr.is_null() {
        return Err(anyhow!("{name} is null"));
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| anyhow!("{name} is not UTF-8"))
}

unsafe fn opt_arg<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(ptr) }.to_str().ok()
    }
}

unsafe fn set_error(out: *mut *mut c_char, err: &anyhow::Error) {
    if !out.is_null() {
        unsafe { *out = c_string(&format!("{err:#}")).into_raw() };
    }
}

unsafe fn with<T>(client: *const pm_client, default: T, f: impl FnOnce(&pm_client) -> T) -> T {
    match unsafe { client.as_ref() } {
        Some(c) => f(c),
        None => default,
    }
}

/// Run `f` with the client and a string argument, or return `default` when
/// either is missing.
unsafe fn with_str<T: Copy>(
    client: *const pm_client,
    s: *const c_char,
    default: T,
    f: impl FnOnce(&pm_client, &str) -> T,
) -> T {
    unsafe {
        with(client, default, |c| match arg(s, "argument") {
            Ok(s) => f(c, s),
            Err(e) => {
                tracing::error!("ffi: {e}");
                default
            }
        })
    }
}

/// Create a client. `data_dir` must be writable; the device key is stored in
/// it. Binds the iroh endpoint before returning, so call off the main
/// thread. Returns null and sets `*error_out` (free with [`pm_string_free`])
/// on failure.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_new(
    data_dir: *const c_char,
    callbacks: *const pm_callbacks,
    error_out: *mut *mut c_char,
) -> *mut pm_client {
    let build = || -> Result<pm_client> {
        let data_dir = unsafe { arg(data_dir, "data_dir") }?;
        let cb = unsafe { callbacks.as_ref() }
            .copied()
            .ok_or_else(|| anyhow!("callbacks is null"))?;
        let events = Arc::new(CEvents {
            cb,
            closed: AtomicBool::new(false),
        });
        let sink = events.clone();
        logging::install(Some(Arc::new(move |level, line| sink.log(level, line))));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("pigeons-mobile")
            .enable_all()
            .build()?;
        let client = runtime.block_on(Client::new(PathBuf::from(data_dir), events.clone()))?;
        Ok(pm_client {
            runtime,
            client,
            events,
        })
    };
    match build() {
        Ok(client) => Box::into_raw(Box::new(client)),
        Err(e) => {
            unsafe { set_error(error_out, &e) };
            ptr::null_mut()
        }
    }
}

/// Disconnect, stop the runtime, and release the client. Blocks briefly. No
/// callback fires once this returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_free(client: *mut pm_client) {
    if client.is_null() {
        return;
    }
    let client = unsafe { Box::from_raw(client) };
    client.events.closed.store(true, Ordering::Release);
    logging::install(None);
    client.client.disconnect();
    client.runtime.shutdown_timeout(Duration::from_secs(2));
}

/// This device's iroh endpoint ID, hex. Free with [`pm_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_endpoint_id(client: *const pm_client) -> *mut c_char {
    unsafe {
        with(client, ptr::null_mut(), |c| {
            c_string(&c.client.endpoint_id().to_string()).into_raw()
        })
    }
}

/// The `authorized_keys` line for this device. Free with [`pm_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_authorized_key(client: *const pm_client) -> *mut c_char {
    unsafe {
        with(client, ptr::null_mut(), |c| {
            match c.client.authorized_key() {
                Ok(line) => c_string(&line).into_raw(),
                Err(_) => ptr::null_mut(),
            }
        })
    }
}

/// Replace the settings. Takes effect for commands issued afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_set_settings(
    client: *mut pm_client,
    settings: *const pm_settings,
) {
    unsafe {
        with(client, (), |c| {
            let Some(s) = settings.as_ref() else { return };
            let defaults = Settings::default();
            c.client.set_settings(Settings {
                code_dir: opt_arg(s.code_dir)
                    .filter(|v| !v.trim().is_empty())
                    .map_or(defaults.code_dir, |v| v.trim().to_string()),
                work_script: opt_arg(s.work_script)
                    .filter(|v| !v.trim().is_empty())
                    .map_or(defaults.work_script, |v| v.trim().to_string()),
                tmux_mouse: s.tmux_mouse,
            });
        })
    }
}

/// Current connection state, one of the `PM_STATE_*` values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_state(client: *const pm_client) -> i32 {
    unsafe { with(client, PM_STATE_DISCONNECTED, |c| c.client.state() as i32) }
}

/// Connect to `roost_id` (hex or z-base-32 endpoint ID) as `user`. `password`
/// is nullable and is only tried when the device key is rejected. Progress
/// arrives through `on_state`; a bad `roost_id` reports `PM_STATE_FAILED`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_connect(
    client: *mut pm_client,
    roost_id: *const c_char,
    user: *const c_char,
    password: *const c_char,
) {
    unsafe {
        with(client, (), |c| {
            let parsed = arg(roost_id, "roost_id")
                .and_then(|id| {
                    EndpointId::from_str(id.trim()).map_err(|e| anyhow!("invalid roost id: {e}"))
                })
                .and_then(|id| Ok((id, arg(user, "user")?.trim().to_string())));
            match parsed {
                Ok((roost, user)) if !user.is_empty() => {
                    c.client
                        .connect(roost, user, opt_arg(password).map(str::to_string));
                }
                Ok(_) => c
                    .events
                    .state(ConnectionState::Failed, Some("user is empty")),
                Err(e) => c
                    .events
                    .state(ConnectionState::Failed, Some(&format!("{e:#}"))),
            }
        })
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_disconnect(client: *mut pm_client) {
    unsafe { with(client, (), |c| c.client.disconnect()) }
}

/// List tmux sessions and projects. Payload: `{"sessions":[...],"projects":[...]}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_list_sessions(client: *mut pm_client) -> u64 {
    unsafe { with(client, 0, |c| c.client.list_sessions()) }
}

/// Canonicalize a path on the roost (`~` allowed). Payload: `{"path":"..."}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_resolve_path(
    client: *mut pm_client,
    path: *const c_char,
) -> u64 {
    unsafe { with_str(client, path, 0, |c, p| c.client.resolve_path(p.to_string())) }
}

/// List a directory on the roost. Payload: a `DirListing` as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_list_dir(client: *mut pm_client, path: *const c_char) -> u64 {
    unsafe { with_str(client, path, 0, |c, p| c.client.list_dir(p.to_string())) }
}

/// Read up to `max_bytes` of a file on the roost. Payload: a `FileContent`
/// as JSON.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_read_file(
    client: *mut pm_client,
    path: *const c_char,
    max_bytes: u64,
) -> u64 {
    unsafe {
        with_str(client, path, 0, |c, p| {
            c.client.read_file(p.to_string(), max_bytes)
        })
    }
}

/// List a project's worktrees. Payload: a JSON array of worktrees.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_list_worktrees(
    client: *mut pm_client,
    project: *const c_char,
) -> u64 {
    unsafe {
        with_str(client, project, 0, |c, p| {
            c.client.list_worktrees(p.to_string())
        })
    }
}

/// List the branches a new worktree of a project can start from. Payload:
/// `{"default":"main"|null,"branches":["..."]}`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_list_branches(
    client: *mut pm_client,
    project: *const c_char,
) -> u64 {
    unsafe {
        with_str(client, project, 0, |c, p| {
            c.client.list_branches(p.to_string())
        })
    }
}

/// Kill a tmux session. Payload: an exec result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_kill_session(
    client: *mut pm_client,
    session: *const c_char,
) -> u64 {
    unsafe {
        with_str(client, session, 0, |c, s| {
            c.client.kill_session(s.to_string())
        })
    }
}

/// Append this device's key to `authorized_keys` on the roost. Payload: an
/// exec result.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_install_authorized_key(client: *mut pm_client) -> u64 {
    unsafe { with(client, 0, |c| c.client.install_authorized_key()) }
}

/// Run an arbitrary command. Payload:
/// `{"exit_code":0,"stdout":"...","stderr":"..."}` (`exit_code` null when unknown).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_exec(client: *mut pm_client, command: *const c_char) -> u64 {
    unsafe { with_str(client, command, 0, |c, cmd| c.client.exec(cmd.to_string())) }
}

/// Open a terminal attached to a tmux session. Returns the terminal id; 0 on
/// bad arguments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_attach_session(
    client: *mut pm_client,
    session: *const c_char,
    cols: u16,
    rows: u16,
) -> u64 {
    unsafe {
        with_str(client, session, 0, |c, s| {
            c.client.attach_session(s.to_string(), cols, rows)
        })
    }
}

/// Open a terminal running `work.sh <project> <tree>`. `base` (nullable) is
/// the branch a brand-new branch starts from; the script asks in the
/// terminal when it is null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_open_worktree(
    client: *mut pm_client,
    project: *const c_char,
    tree: *const c_char,
    base: *const c_char,
    cols: u16,
    rows: u16,
) -> u64 {
    unsafe {
        with_str(client, project, 0, |c, p| match arg(tree, "tree") {
            Ok(t) => c.client.open_worktree(
                p.to_string(),
                t.to_string(),
                opt_arg(base).map(str::to_string),
                cols,
                rows,
            ),
            Err(_) => 0,
        })
    }
}

/// Open a terminal running a login shell.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_client_open_shell(client: *mut pm_client, cols: u16, rows: u16) -> u64 {
    unsafe { with(client, 0, |c| c.client.open_shell(cols, rows)) }
}

/// Send bytes (keystrokes) to a terminal.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_terminal_write(
    client: *mut pm_client,
    terminal: u64,
    bytes: *const u8,
    len: usize,
) {
    if bytes.is_null() || len == 0 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(bytes, len) }.to_vec();
    unsafe { with(client, (), |c| c.client.terminal_write(terminal, data)) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_terminal_resize(
    client: *mut pm_client,
    terminal: u64,
    cols: u16,
    rows: u16,
) {
    unsafe {
        with(client, (), |c| {
            c.client.terminal_resize(terminal, cols, rows)
        })
    }
}

/// Close a terminal. `on_terminal_exit` still fires for it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_terminal_close(client: *mut pm_client, terminal: u64) {
    unsafe { with(client, (), |c| c.client.terminal_close(terminal)) }
}

/// Free a string returned by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pm_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// Library version, static; do not free.
#[unsafe(no_mangle)]
pub extern "C" fn pm_version() -> *const c_char {
    static VERSION: &CStr = c"pigeons-mobile 0.1.0";
    VERSION.as_ptr()
}
