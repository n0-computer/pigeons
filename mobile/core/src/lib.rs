//! Rust core for the pigeons iOS app.
//!
//! The app is a thin SwiftUI shell over this crate. Everything that talks to
//! the network lives here:
//!
//! - an iroh endpoint whose secret key is also the app's SSH key (see
//!   [`identity`]), persisted in the app's data directory the same way the
//!   `pigeons` CLI persists its roost key;
//! - one SSH connection to a single roost, carried over
//!   [`iroh_pigeons::Tunnel::open_stream`] rather than TCP;
//! - the tmux and `work.sh` command vocabulary ([`tmux`]): listing sessions,
//!   attaching to them in a PTY, killing them, and opening worktrees.
//!
//! The C ABI the app binds to is in [`ffi`] and declared for Swift in
//! `include/pigeons_mobile.h`. It is a hand-written surface designed around
//! this app: one opaque client handle, a callback table for everything
//! asynchronous, and JSON payloads for structured results.

pub mod client;
pub mod ffi;
pub mod files;
pub mod identity;
pub mod logging;
pub mod shell;
pub mod tmux;

pub use client::{
    Client, ConnectionState, Events, ExecOutput, RequestId, Settings, TerminalExit, TerminalId,
};
