//! Process-global raw stdout writer for json/rpc wire modes.
//!
//! Invariant: in json/rpc modes nothing writes stdout except `wire_out`;
//! diagnostics go to stderr.
//!
//! Port of `packages/coding-agent/src/core/output-guard.ts`
//! (`writeRawStdout` / `flushRawStdout`).

use std::io::{self, Write};
use std::process;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::LazyLock;
use std::thread;
use std::time::Duration;

/// Retry delay matching oracle `RAW_STDOUT_RETRY_DELAY_MS`.
const RAW_STDOUT_RETRY_DELAY: Duration = Duration::from_millis(10);

/// Linux `ENOBUFS` (also the portable value on most Unix).
const ENOBUFS: i32 = 105;

enum Command {
    Write(Vec<u8>),
    Flush(Sender<()>),
}

enum Sink {
    Stdout,
    Custom(Box<dyn Write + Send>),
}

/// Ordered, retrying raw-stdout writer used by json/rpc modes.
pub struct WireOut {
    tx: Sender<Command>,
}

static GLOBAL: LazyLock<WireOut> = LazyLock::new(|| WireOut::spawn(Sink::Stdout));

impl WireOut {
    /// Process-global stdout writer.
    pub fn stdout() -> &'static WireOut {
        &GLOBAL
    }

    /// Injectable writer for tests (captures into an in-memory sink, etc.).
    pub fn new_with_writer(w: Box<dyn Write + Send>) -> WireOut {
        Self::spawn(Sink::Custom(w))
    }

    fn spawn(sink: Sink) -> WireOut {
        let (tx, rx) = mpsc::channel();
        thread::Builder::new()
            .name("wire-out".into())
            .spawn(move || writer_loop(rx, sink))
            .expect("failed to spawn wire-out writer thread");
        WireOut { tx }
    }

    /// Enqueue `text` for sequential stdout write. Empty text is a no-op.
    pub fn write(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Writer thread death is unrecoverable — same as an stdout I/O failure.
        if self.tx.send(Command::Write(text.as_bytes().to_vec())).is_err() {
            process::exit(1);
        }
    }

    /// Block until every previously enqueued chunk has been written and flushed.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Command::Flush(ack_tx)).is_err() {
            process::exit(1);
        }
        if ack_rx.recv().is_err() {
            process::exit(1);
        }
    }
}

fn is_retryable(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    matches!(err.raw_os_error(), Some(ENOBUFS))
}

fn write_all_retry(sink: &mut Sink, data: &[u8]) {
    let mut offset = 0;
    while offset < data.len() {
        let result = match sink {
            Sink::Stdout => io::stdout().lock().write(&data[offset..]),
            Sink::Custom(w) => w.write(&data[offset..]),
        };
        match result {
            Ok(0) => {
                // write() returned 0 on a non-empty buffer → WriteZero / closed sink.
                process::exit(1);
            }
            Ok(n) => offset += n,
            Err(e) if is_retryable(&e) => {
                thread::sleep(RAW_STDOUT_RETRY_DELAY);
            }
            Err(_) => {
                // Unrecoverable stdout error — oracle exits 1.
                process::exit(1);
            }
        }
    }
}

fn flush_sink(sink: &mut Sink) {
    loop {
        let result = match sink {
            Sink::Stdout => io::stdout().lock().flush(),
            Sink::Custom(w) => w.flush(),
        };
        match result {
            Ok(()) => return,
            Err(e) if is_retryable(&e) => {
                thread::sleep(RAW_STDOUT_RETRY_DELAY);
            }
            Err(_) => process::exit(1),
        }
    }
}

fn writer_loop(rx: Receiver<Command>, mut sink: Sink) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Command::Write(data) => write_all_retry(&mut sink, &data),
            Command::Flush(ack) => {
                flush_sink(&mut sink);
                let _ = ack.send(());
            }
        }
    }
}
