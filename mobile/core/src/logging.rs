//! Forward `tracing` output to the host app.
//!
//! The app shows these lines in a debug screen, which is the only way to see
//! what the core did on a phone running a TestFlight build.

use std::{
    io,
    sync::{Arc, RwLock},
};

use tracing::{Level, Metadata};
use tracing_subscriber::{
    EnvFilter, fmt::MakeWriter, layer::SubscriberExt, util::SubscriberInitExt,
};

/// Receives one formatted log line at a time.
pub type Sink = Arc<dyn Fn(Level, &str) + Send + Sync>;

static SINK: RwLock<Option<Sink>> = RwLock::new(None);

/// Default filter: this crate and the tunnel at debug, everything else at info.
pub const DEFAULT_FILTER: &str = "info,pigeons_mobile=debug,iroh_pigeons=debug";

/// Install the global subscriber (once) and point it at `sink`. Later calls
/// only swap the sink, so a new client replaces an old one's sink.
pub fn install(sink: Option<Sink>) {
    *SINK.write().unwrap_or_else(|e| e.into_inner()) = sink;
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(DEFAULT_FILTER))
        .expect("default filter parses");
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .without_time()
                .with_target(true)
                .with_writer(SinkWriter),
        )
        .try_init();
}

#[derive(Clone, Copy, Debug)]
struct SinkWriter;

impl<'a> MakeWriter<'a> for SinkWriter {
    type Writer = LineWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LineWriter::new(Level::INFO)
    }

    fn make_writer_for(&'a self, meta: &Metadata<'_>) -> Self::Writer {
        LineWriter::new(*meta.level())
    }
}

/// Buffers one event's bytes and hands them to the sink as a line on drop.
#[derive(Debug)]
pub struct LineWriter {
    level: Level,
    buf: Vec<u8>,
}

impl LineWriter {
    fn new(level: Level) -> Self {
        Self {
            level,
            buf: Vec::new(),
        }
    }
}

impl io::Write for LineWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let sink = SINK.read().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sink) = sink {
            let line = String::from_utf8_lossy(&self.buf);
            sink(self.level, line.trim_end());
        }
    }
}
