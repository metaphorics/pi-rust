//! ProcessTerminal — real stdin/stdout terminal with raw mode + Kitty detect.
//!
//! Port of `packages/tui/src/terminal.ts` `ProcessTerminal`.
//!
//! # Platform
//!
//! - Unix: rustix termios raw mode, SIGWINCH self-signal to refresh size,
//!   bracketed paste, Kitty keyboard protocol negotiation.
//! - Windows: stub that documents the missing VT/console path (cfg-gated).
//!
//! # Input segmentation
//!
//! Uses [`inkferro_core::input::Segmenter`] for opaque raw byte segments so
//! pi `matchesKey` retains mode-dependent/raw-sequence semantics. Pastes are
//! re-wrapped with bracketed-paste markers; key/text bytes are forwarded
//! losslessly as UTF-8 (lossy only if invalid).

use std::env;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use inkferro_core::input::{Segment, Segmenter};

use crate::keys::set_kitty_protocol_active;
use crate::terminal::Terminal;

pub const TERMINAL_PROGRESS_KEEPALIVE_MS: u64 = 1000;
pub const TERMINAL_PROGRESS_ACTIVE_SEQUENCE: &str = "\x1b]9;4;3\x07";
pub const TERMINAL_PROGRESS_CLEAR_SEQUENCE: &str = "\x1b]9;4;0;\x07";
pub const APPLE_TERMINAL_SHIFT_ENTER_SEQUENCE: &str = "\x1b[13;2u";
pub const DESIRED_KITTY_KEYBOARD_PROTOCOL_FLAGS: u32 = 7;
pub const KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT_MS: u64 = 150;
/// Kitty progressive enhancement query: push flags 7, query flags, DA sentinel.
pub const KITTY_KEYBOARD_PROTOCOL_QUERY: &str = "\x1b[>7u\x1b[?u\x1b[c";
/// OSC 11 background query plus DA fallback sentinel.
pub const TERMINAL_BACKGROUND_COLOR_QUERY: &str = "\x1b]11;?\x07\x1b[c";

/// Parse an OSC 11 `rgb:RR/GG/BB` response terminated by BEL or ST.
///
/// The terminal may return 8-, 12-, or 16-bit channels; each is scaled to an
/// 8-bit sRGB component. A DA response deliberately parses as `None`: it is
/// the fallback sentinel indicating OSC 11 support is absent.
#[must_use]
pub fn parse_terminal_background_color_response(sequence: &str) -> Option<(u8, u8, u8)> {
    let start = sequence.find("\x1b]11;rgb:")?;
    if device_attributes_response_offset(sequence).is_some_and(|offset| offset < start) {
        return None;
    }
    let value_start = start + "\x1b]11;rgb:".len();
    let response = &sequence[value_start..];
    let terminator = match (response.find('\x07'), response.find("\x1b\\")) {
        (Some(bel), Some(st)) => bel.min(st),
        (Some(bel), None) => bel,
        (None, Some(st)) => st,
        (None, None) => return None,
    };
    let value = &response[..terminator];
    let mut channels = value.split('/');
    let parse_channel = |channel: &str| {
        if !(2..=4).contains(&channel.len())
            || !channel.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return None;
        }
        let maximum = (1_u32 << (channel.len() * 4)) - 1;
        let parsed = u32::from_str_radix(channel, 16).ok()?;
        Some(((parsed * 255 + maximum / 2) / maximum) as u8)
    };
    let red = parse_channel(channels.next()?)?;
    let green = parse_channel(channels.next()?)?;
    let blue = parse_channel(channels.next()?)?;
    channels.next().is_none().then_some((red, green, blue))
}

fn device_attributes_response_offset(sequence: &str) -> Option<usize> {
    let mut offset = 0;
    while let Some(found) = sequence[offset..].find("\x1b[") {
        let start = offset + found;
        let tail = &sequence[start + 2..];
        if matches!(tail.as_bytes().first(), Some(b'?' | b'>')) {
            let body = &tail[1..];
            if let Some(end) = body.find('c') {
                let digits = &body[..end];
                if digits
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte == b';')
                {
                    return Some(start);
                }
            }
        }
        offset = start + 2;
    }
    None
}

fn strip_terminal_query_responses(sequence: &[u8]) -> Vec<u8> {
    let mut kept = Vec::with_capacity(sequence.len());
    let mut index = 0;
    while index < sequence.len() {
        let rest = &sequence[index..];
        if rest.starts_with(b"\x1b]11;rgb:") {
            let bel_end = rest
                .iter()
                .position(|byte| *byte == b'\x07')
                .map(|offset| (offset, offset + 1));
            let st_end = rest
                .windows(2)
                .position(|bytes| bytes == b"\x1b\\")
                .map(|offset| (offset, offset + 2));
            if let Some((_, end)) = match (bel_end, st_end) {
                (Some(bel), Some(st)) => Some(if bel.0 < st.0 { bel } else { st }),
                (Some(end), None) | (None, Some(end)) => Some(end),
                (None, None) => None,
            } {
                index += end;
                continue;
            }
        }
        if rest.starts_with(b"\x1b[")
            && let Some(end) = rest.iter().position(|byte| *byte == b'c')
        {
            let body = &rest[2..end];
            if matches!(body.first(), Some(b'?' | b'>'))
                && body[1..]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || *byte == b';')
            {
                index += end + 1;
                continue;
            }
        }
        kept.push(sequence[index]);
        index += 1;
    }
    kept
}

/// Kitty keyboard protocol negotiation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardProtocolNegotiationSequence {
    KittyFlags { flags: u32 },
    DeviceAttributes,
}

/// Parse a complete negotiation response (`CSI ? <flags> u` or DA `CSI ? … c`).
pub fn parse_keyboard_protocol_negotiation_sequence(
    sequence: &str,
) -> Option<KeyboardProtocolNegotiationSequence> {
    if let Some(rest) = sequence.strip_prefix("\x1b[?") {
        if let Some(digits) = rest.strip_suffix('u')
            && !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(flags) = digits.parse::<u32>()
        {
            return Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags });
        }
        if let Some(body) = rest.strip_suffix('c')
            && body.bytes().all(|b| b.is_ascii_digit() || b == b';')
        {
            return Some(KeyboardProtocolNegotiationSequence::DeviceAttributes);
        }
    }
    None
}

fn is_keyboard_protocol_negotiation_sequence_prefix(sequence: &str) -> bool {
    if sequence == "\x1b[" {
        return true;
    }
    if let Some(rest) = sequence.strip_prefix("\x1b[?") {
        return rest.bytes().all(|b| b.is_ascii_digit() || b == b';');
    }
    false
}

pub fn is_apple_terminal_session() -> bool {
    cfg!(target_os = "macos") && env::var("TERM_PROGRAM").ok().as_deref() == Some("Apple_Terminal")
}

/// Normalize Apple Terminal Enter + Shift into Kitty-style shift-enter.
pub fn normalize_apple_terminal_input(
    data: &str,
    is_apple_terminal: bool,
    is_shift_pressed: bool,
) -> String {
    if is_apple_terminal && data == "\r" && is_shift_pressed {
        return APPLE_TERMINAL_SHIFT_ENTER_SEQUENCE.to_string();
    }
    data.to_string()
}

fn resolve_write_log_path() -> Option<PathBuf> {
    let env_path = env::var("PI_TUI_WRITE_LOG").ok()?;
    if env_path.is_empty() {
        return None;
    }
    let path = PathBuf::from(&env_path);
    if path.is_dir() {
        let secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pid = std::process::id();
        Some(path.join(format!("tui-{secs}-{pid}.log")))
    } else {
        Some(path)
    }
}

fn query_size() -> (u16, u16) {
    #[cfg(unix)]
    {
        use rustix::stdio::stdout;
        use rustix::termios;
        if let Ok(ws) = termios::tcgetwinsize(stdout()) {
            let cols = if ws.ws_col == 0 { 80 } else { ws.ws_col };
            let rows = if ws.ws_row == 0 { 24 } else { ws.ws_row };
            return (cols, rows);
        }
    }
    let cols = env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(80);
    let rows = env::var("LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24);
    (cols, rows)
}

#[cfg(unix)]
fn io_err(e: rustix::io::Errno) -> io::Error {
    io::Error::from_raw_os_error(e.raw_os_error())
}

enum NegotiationRead {
    Complete(KeyboardProtocolNegotiationSequence),
    Pending,
    NotNegotiation,
}

/// Set by the SIGWINCH handler; consumed in [`ProcessTerminal::poll_resize`].
#[cfg(unix)]
static WINCH_PENDING: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn winch_handler(_sig: libc::c_int) {
    WINCH_PENDING.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_sigwinch_handler() {
    // SA_RESTART so blocking stdin reads survive the signal.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = winch_handler as *const () as usize;
        libc::sigemptyset(&mut sa.sa_mask);
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
fn install_sigwinch_handler() {}

/// Real process terminal (stdin/stdout).
///
/// On Unix this owns raw mode, a stdin reader thread, and Kitty protocol state.
/// On Windows this is a documented stub — raw console VT is not wired yet.
pub struct ProcessTerminal {
    kitty_protocol_active: bool,
    modify_other_keys_active: bool,
    keyboard_protocol_pushed: bool,
    keyboard_protocol_negotiation_buffer: String,
    negotiation_buffer_since: Option<Instant>,
    input_handler: Option<Box<dyn FnMut(&str) + 'static>>,
    resize_handler: Option<Box<dyn FnMut() + 'static>>,
    write_log_path: Option<PathBuf>,
    progress_active: bool,
    progress_last_emit: Option<Instant>,
    running: Arc<AtomicBool>,
    reader_join: Option<JoinHandle<()>>,
    stdin_tx: Option<Sender<Vec<u8>>>,
    stdin_rx: Option<Receiver<Vec<u8>>>,
    stdin_segmenter: Segmenter,
    /// Bytes typed while a pre-start terminal probe owns stdin.
    pending_stdin_chunks: Vec<Vec<u8>>,
    #[cfg(unix)]
    saved_termios: Option<rustix::termios::Termios>,
    last_cols: u16,
    last_rows: u16,
    /// Optional CSI 2026 telemetry (BSU frames observed on write/stdin).
    pub csi_2026_frames_seen: u64,
}

impl Default for ProcessTerminal {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessTerminal {
    pub fn new() -> Self {
        let (cols, rows) = query_size();
        Self {
            kitty_protocol_active: false,
            modify_other_keys_active: false,
            keyboard_protocol_pushed: false,
            keyboard_protocol_negotiation_buffer: String::new(),
            negotiation_buffer_since: None,
            input_handler: None,
            resize_handler: None,
            write_log_path: resolve_write_log_path(),
            progress_active: false,
            progress_last_emit: None,
            running: Arc::new(AtomicBool::new(false)),
            reader_join: None,
            stdin_tx: None,
            stdin_rx: None,
            stdin_segmenter: Segmenter::default(),
            pending_stdin_chunks: Vec::new(),
            #[cfg(unix)]
            saved_termios: None,
            last_cols: cols,
            last_rows: rows,
            csi_2026_frames_seen: 0,
        }
    }

    pub fn modify_other_keys_active(&self) -> bool {
        self.modify_other_keys_active
    }

    fn write_stdout(data: &str) {
        let mut out = io::stdout().lock();
        let _ = out.write_all(data.as_bytes());
        let _ = out.flush();
    }
    /// Query OSC 11 before starting the TUI, preserving unrelated typed input.
    ///
    /// A DA response is a sentinel that the terminal does not support OSC 11.
    /// The probe never starts a reader thread and restores the preceding
    /// termios state before returning.
    #[must_use]
    pub fn query_background_color(&mut self, timeout_ms: u64) -> Option<(u8, u8, u8)> {
        #[cfg(unix)]
        {
            use rustix::stdio::{stdin, stdout};
            use rustix::termios;

            if self.running.load(Ordering::SeqCst)
                || !termios::isatty(stdin())
                || !termios::isatty(stdout())
                || self.enable_raw_mode().is_err()
            {
                return None;
            }

            Self::write_stdout(TERMINAL_BACKGROUND_COLOR_QUERY);
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut received = Vec::new();
            let result = loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break None;
                }
                let timeout = remaining.as_millis().min(i32::MAX as u128) as i32;
                let mut poll_fd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                // SAFETY: poll_fd points to valid stack storage for one descriptor.
                let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout) };
                if ready <= 0 {
                    break None;
                }
                let mut chunk = [0_u8; 4096];
                // SAFETY: chunk is writable storage and stdin is a valid descriptor.
                let count = unsafe {
                    libc::read(
                        libc::STDIN_FILENO,
                        chunk.as_mut_ptr().cast::<libc::c_void>(),
                        chunk.len(),
                    )
                };
                if count <= 0 {
                    break None;
                }
                received.extend_from_slice(&chunk[..count as usize]);
                let text = String::from_utf8_lossy(&received);
                let osc_offset = text.find("\x1b]11;rgb:");
                if let Some(da_offset) = device_attributes_response_offset(&text)
                    && osc_offset.is_none_or(|osc_offset| da_offset < osc_offset)
                {
                    break None;
                }
                if let Some(color) = parse_terminal_background_color_response(&text) {
                    break Some(color);
                }
            };
            let preserved = strip_terminal_query_responses(&received);
            if !preserved.is_empty() {
                self.pending_stdin_chunks.push(preserved);
            }
            self.restore_termios();
            result
        }
        #[cfg(not(unix))]
        {
            let _ = timeout_ms;
            None
        }
    }

    fn append_write_log(&self, data: &str) {
        let Some(path) = &self.write_log_path else {
            return;
        };
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(data.as_bytes());
        }
    }

    fn enable_modify_other_keys(&mut self) {
        if self.kitty_protocol_active || self.modify_other_keys_active {
            return;
        }
        Self::write_stdout("\x1b[>4;2m");
        self.modify_other_keys_active = true;
    }

    fn disable_modify_other_keys(&mut self) {
        if !self.modify_other_keys_active {
            return;
        }
        Self::write_stdout("\x1b[>4;0m");
        self.modify_other_keys_active = false;
    }

    fn clear_keyboard_protocol_negotiation_buffer(&mut self) {
        self.keyboard_protocol_negotiation_buffer.clear();
        self.negotiation_buffer_since = None;
    }

    fn handle_keyboard_protocol_negotiation_sequence(
        &mut self,
        negotiation: KeyboardProtocolNegotiationSequence,
    ) {
        self.clear_keyboard_protocol_negotiation_buffer();
        match negotiation {
            KeyboardProtocolNegotiationSequence::KittyFlags { flags } => {
                if flags != 0 {
                    self.disable_modify_other_keys();
                    if !self.kitty_protocol_active {
                        self.kitty_protocol_active = true;
                        set_kitty_protocol_active(true);
                    }
                } else {
                    self.enable_modify_other_keys();
                }
            }
            KeyboardProtocolNegotiationSequence::DeviceAttributes => {
                if !self.kitty_protocol_active {
                    self.enable_modify_other_keys();
                }
            }
        }
    }

    fn read_keyboard_protocol_negotiation_sequence(&mut self, sequence: &str) -> NegotiationRead {
        if !self.keyboard_protocol_negotiation_buffer.is_empty() {
            let buffered = format!("{}{sequence}", self.keyboard_protocol_negotiation_buffer);
            if let Some(neg) = parse_keyboard_protocol_negotiation_sequence(&buffered) {
                self.clear_keyboard_protocol_negotiation_buffer();
                return NegotiationRead::Complete(neg);
            }
            if is_keyboard_protocol_negotiation_sequence_prefix(&buffered) {
                self.keyboard_protocol_negotiation_buffer = buffered;
                self.negotiation_buffer_since = Some(Instant::now());
                return NegotiationRead::Pending;
            }
            let flushed = std::mem::take(&mut self.keyboard_protocol_negotiation_buffer);
            self.negotiation_buffer_since = None;
            self.forward_input_sequence(&flushed);
        }

        if let Some(neg) = parse_keyboard_protocol_negotiation_sequence(sequence) {
            return NegotiationRead::Complete(neg);
        }
        if is_keyboard_protocol_negotiation_sequence_prefix(sequence) {
            self.keyboard_protocol_negotiation_buffer = sequence.to_string();
            self.negotiation_buffer_since = Some(Instant::now());
            return NegotiationRead::Pending;
        }
        NegotiationRead::NotNegotiation
    }

    fn forward_input_sequence(&mut self, sequence: &str) {
        let Some(handler) = self.input_handler.as_mut() else {
            return;
        };
        let is_apple = sequence == "\r" && is_apple_terminal_session();
        let input = normalize_apple_terminal_input(sequence, is_apple, false);
        handler(&input);
    }

    /// Feed a raw stdin chunk through the segmenter and dispatch sequences.
    pub fn process_stdin_chunk(&mut self, bytes: &[u8]) {
        if bytes.windows(8).any(|w| w == b"\x1b[?2026h") {
            self.csi_2026_frames_seen = self.csi_2026_frames_seen.saturating_add(1);
        }

        let segments = self.stdin_segmenter.push(bytes);
        for segment in segments {
            match segment {
                Segment::Bytes(raw) => {
                    let sequence = String::from_utf8_lossy(&raw).into_owned();
                    match self.read_keyboard_protocol_negotiation_sequence(&sequence) {
                        NegotiationRead::Pending => {}
                        NegotiationRead::Complete(neg) => {
                            self.handle_keyboard_protocol_negotiation_sequence(neg);
                        }
                        NegotiationRead::NotNegotiation => {
                            self.forward_input_sequence(&sequence);
                        }
                    }
                }
                Segment::Paste(payload) => {
                    let content = String::from_utf8_lossy(&payload);
                    let wrapped = format!("\x1b[200~{content}\x1b[201~");
                    self.forward_input_sequence(&wrapped);
                }
            }
        }
    }

    fn flush_negotiation_buffer_if_stale(&mut self) {
        let stale = self.negotiation_buffer_since.is_some_and(|t| {
            t.elapsed() >= Duration::from_millis(KEYBOARD_PROTOCOL_RESPONSE_FRAGMENT_TIMEOUT_MS)
        });
        if stale && !self.keyboard_protocol_negotiation_buffer.is_empty() {
            let sequence = std::mem::take(&mut self.keyboard_protocol_negotiation_buffer);
            self.negotiation_buffer_since = None;
            self.forward_input_sequence(&sequence);
        }
    }

    fn maybe_emit_progress_keepalive(&mut self) {
        if !self.progress_active {
            return;
        }
        let now = Instant::now();
        let due = match self.progress_last_emit {
            Some(t) => {
                now.duration_since(t) >= Duration::from_millis(TERMINAL_PROGRESS_KEEPALIVE_MS)
            }
            None => true,
        };
        if due {
            Self::write_stdout(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            self.progress_last_emit = Some(now);
        }
    }

    /// Poll window size and fire resize when it changes.
    ///
    /// On Unix, also reacts to SIGWINCH via [`WINCH_PENDING`] (installed in
    /// `start`). Always re-queries size so stale dimensions after suspend are
    /// corrected even if the signal was coalesced.
    pub fn poll_resize(&mut self) {
        #[cfg(unix)]
        let signaled = WINCH_PENDING.swap(false, Ordering::SeqCst);
        #[cfg(not(unix))]
        let signaled = false;

        let (cols, rows) = query_size();
        if signaled || cols != self.last_cols || rows != self.last_rows {
            self.last_cols = cols;
            self.last_rows = rows;
            if let Some(handler) = self.resize_handler.as_mut() {
                handler();
            }
        }
        self.maybe_emit_progress_keepalive();
    }

    /// Drain pending stdin chunks and dispatch to `on_input`. Call from the
    /// TUI event loop (same thread that owns `ProcessTerminal`).
    pub fn poll_input(&mut self) {
        let mut chunks = Vec::new();
        if let Some(rx) = self.stdin_rx.as_ref() {
            while let Ok(c) = rx.try_recv() {
                chunks.push(c);
            }
        }
        for chunk in &chunks {
            self.process_stdin_chunk(chunk);
        }
        self.flush_negotiation_buffer_if_stale();
        self.poll_resize();
    }

    #[cfg(unix)]
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        use rustix::stdio::stdin;
        use rustix::termios::{self, OptionalActions};

        if !termios::isatty(stdin()) {
            return Ok(());
        }
        let current = termios::tcgetattr(stdin()).map_err(io_err)?;
        self.saved_termios = Some(current.clone());
        let mut raw = current;
        raw.make_raw();
        termios::tcsetattr(stdin(), OptionalActions::Now, &raw).map_err(io_err)?;
        Ok(())
    }

    #[cfg(unix)]
    fn restore_termios(&mut self) {
        use rustix::stdio::stdin;
        use rustix::termios::{self, OptionalActions};
        if let Some(saved) = self.saved_termios.take() {
            let _ = termios::tcsetattr(stdin(), OptionalActions::Now, &saved);
        }
    }

    #[cfg(unix)]
    fn signal_sigwinch_self() {
        // After installing the handler, poke ourselves so dimensions refresh
        // post-suspend (TS: process.kill(pid, 'SIGWINCH')).
        use rustix::process::{Signal, getpid, kill_process};
        let _ = kill_process(getpid(), Signal::WINCH);
    }

    #[cfg(not(unix))]
    fn enable_raw_mode(&mut self) -> io::Result<()> {
        // Windows: ENABLE_VIRTUAL_TERMINAL_INPUT would be set here after raw
        // mode, matching TS `enableWindowsVTInput`. Not implemented in this
        // port — callers should not rely on ProcessTerminal on Windows yet.
        Ok(())
    }

    #[cfg(not(unix))]
    fn restore_termios(&mut self) {}

    #[cfg(not(unix))]
    fn signal_sigwinch_self() {}
}

impl Terminal for ProcessTerminal {
    fn start(
        &mut self,
        on_input: Box<dyn FnMut(&str) + 'static>,
        on_resize: Box<dyn FnMut() + 'static>,
    ) {
        self.input_handler = Some(on_input);
        self.resize_handler = Some(on_resize);

        let _ = self.enable_raw_mode();
        self.stdin_segmenter = Segmenter::default();

        // Bracketed paste enable.
        Self::write_stdout("\x1b[?2004h");

        // SIGWINCH → WINCH_PENDING; then self-signal to refresh size after
        // suspend/resume (signal may have been lost while stopped).
        install_sigwinch_handler();
        Self::signal_sigwinch_self();

        // Query Kitty keyboard protocol; fall back to modifyOtherKeys on DA.
        self.keyboard_protocol_pushed = true;
        self.clear_keyboard_protocol_negotiation_buffer();
        Self::write_stdout(KITTY_KEYBOARD_PROTOCOL_QUERY);
        for chunk in std::mem::take(&mut self.pending_stdin_chunks) {
            self.process_stdin_chunk(&chunk);
        }

        self.running.store(true, Ordering::SeqCst);
        let running = Arc::clone(&self.running);
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        self.stdin_tx = Some(tx.clone());
        self.stdin_rx = Some(rx);

        let join = thread::Builder::new()
            .name("pi-tui-stdin".into())
            .spawn(move || {
                let mut stdin = io::stdin();
                let mut buf = [0u8; 4096];
                while running.load(Ordering::SeqCst) {
                    match stdin.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .ok();
        self.reader_join = join;
    }

    fn poll(&mut self) {
        self.poll_input();
    }

    fn stop(&mut self) {
        if self.progress_active {
            Self::write_stdout(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
            self.progress_active = false;
            self.progress_last_emit = None;
        }

        Self::write_stdout("\x1b[?2004l");

        let should_disable_kitty = self.keyboard_protocol_pushed || self.kitty_protocol_active;
        self.clear_keyboard_protocol_negotiation_buffer();

        if should_disable_kitty {
            Self::write_stdout("\x1b[<u");
            self.keyboard_protocol_pushed = false;
            self.kitty_protocol_active = false;
            set_kitty_protocol_active(false);
        }
        self.disable_modify_other_keys();

        self.running.store(false, Ordering::SeqCst);
        // Drop sender so reader exits on next successful read+send failure.
        // Do NOT join: the reader blocks in stdin.read() and would hang stop()
        // until the next keystroke. Detach and let the OS reclaim on exit.
        self.stdin_tx = None;
        self.stdin_rx = None;
        if let Some(join) = self.reader_join.take() {
            drop(join);
        }
        self.input_handler = None;
        self.resize_handler = None;

        self.restore_termios();
    }

    fn drain_input(&mut self, max_ms: u64, idle_ms: u64) {
        let should_disable_kitty = self.keyboard_protocol_pushed || self.kitty_protocol_active;
        self.clear_keyboard_protocol_negotiation_buffer();
        if should_disable_kitty {
            Self::write_stdout("\x1b[<u");
            self.keyboard_protocol_pushed = false;
            self.kitty_protocol_active = false;
            set_kitty_protocol_active(false);
        }
        self.disable_modify_other_keys();

        let previous = self.input_handler.take();
        let end = Instant::now() + Duration::from_millis(max_ms);
        let mut last_data = Instant::now();
        while Instant::now() < end {
            let mut got = false;
            if let Some(rx) = self.stdin_rx.as_ref() {
                while let Ok(_chunk) = rx.try_recv() {
                    got = true;
                    last_data = Instant::now();
                }
            }
            if !got && last_data.elapsed() >= Duration::from_millis(idle_ms) {
                break;
            }
            thread::sleep(Duration::from_millis(idle_ms.min(10)));
        }
        self.input_handler = previous;
    }

    fn write(&mut self, data: &str) {
        Self::write_stdout(data);
        self.append_write_log(data);
        if data.contains("\x1b[?2026h") {
            self.csi_2026_frames_seen = self.csi_2026_frames_seen.saturating_add(1);
        }
    }

    fn columns(&self) -> u16 {
        query_size().0
    }

    fn rows(&self) -> u16 {
        query_size().1
    }

    fn kitty_protocol_active(&self) -> bool {
        self.kitty_protocol_active
    }

    fn move_by(&mut self, lines: i32) {
        if lines > 0 {
            Self::write_stdout(&format!("\x1b[{lines}B"));
        } else if lines < 0 {
            let n = -lines;
            Self::write_stdout(&format!("\x1b[{n}A"));
        }
    }

    fn hide_cursor(&mut self) {
        Self::write_stdout("\x1b[?25l");
    }

    fn show_cursor(&mut self) {
        Self::write_stdout("\x1b[?25h");
    }

    fn clear_line(&mut self) {
        Self::write_stdout("\x1b[K");
    }

    fn clear_from_cursor(&mut self) {
        Self::write_stdout("\x1b[J");
    }

    fn clear_screen(&mut self) {
        Self::write_stdout("\x1b[2J\x1b[H");
    }

    fn set_title(&mut self, title: &str) {
        Self::write_stdout(&format!("\x1b]0;{title}\x07"));
    }

    /// Suspend teardown (oracle: `ui.stop()` before SIGTSTP): clear
    /// progress, disable bracketed paste, pop Kitty protocol, restore
    /// termios. The stdin reader thread, channels, and handlers stay
    /// alive — `stop()`'s thread detach + a second `start()` would race
    /// two readers on the same fd after resume.
    fn suspend(&mut self) {
        if self.progress_active {
            Self::write_stdout(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
            self.progress_active = false;
            self.progress_last_emit = None;
        }
        Self::write_stdout("\x1b[?2004l");
        let should_disable_kitty = self.keyboard_protocol_pushed || self.kitty_protocol_active;
        self.clear_keyboard_protocol_negotiation_buffer();
        if should_disable_kitty {
            Self::write_stdout("\x1b[<u");
            self.keyboard_protocol_pushed = false;
            self.kitty_protocol_active = false;
            set_kitty_protocol_active(false);
        }
        self.disable_modify_other_keys();
        self.restore_termios();
    }

    /// Resume after SIGCONT (oracle: `ui.start()` in the SIGCONT handler):
    /// mirrors `start()` minus thread/handler setup. Self-SIGWINCH refreshes
    /// dimensions lost while stopped (terminal.ts:152-156).
    fn resume(&mut self) {
        let _ = self.enable_raw_mode();
        self.stdin_segmenter = Segmenter::default();
        Self::write_stdout("\x1b[?2004h");
        Self::signal_sigwinch_self();
        self.keyboard_protocol_pushed = true;
        self.clear_keyboard_protocol_negotiation_buffer();
        Self::write_stdout(KITTY_KEYBOARD_PROTOCOL_QUERY);
    }

    fn set_progress(&mut self, active: bool) {
        if active {
            Self::write_stdout(TERMINAL_PROGRESS_ACTIVE_SEQUENCE);
            self.progress_active = true;
            self.progress_last_emit = Some(Instant::now());
        } else {
            self.progress_active = false;
            self.progress_last_emit = None;
            Self::write_stdout(TERMINAL_PROGRESS_CLEAR_SEQUENCE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kitty_flags() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?7u"),
            Some(KeyboardProtocolNegotiationSequence::KittyFlags { flags: 7 })
        );
    }

    #[test]
    fn parse_device_attributes() {
        assert_eq!(
            parse_keyboard_protocol_negotiation_sequence("\x1b[?1;2c"),
            Some(KeyboardProtocolNegotiationSequence::DeviceAttributes)
        );
    }

    #[test]
    fn process_stdin_kitty_flags_activates_protocol() {
        let mut term = ProcessTerminal::new();
        term.input_handler = Some(Box::new(|_s| {}));
        term.process_stdin_chunk(b"\x1b[?7u");
        assert!(term.kitty_protocol_active());
    }

    #[test]
    fn process_stdin_forwards_printable() {
        let mut term = ProcessTerminal::new();
        use std::sync::{Arc, Mutex};
        let got = Arc::new(Mutex::new(Vec::new()));
        let got2 = Arc::clone(&got);
        term.input_handler = Some(Box::new(move |s| {
            got2.lock().unwrap().push(s.to_string());
        }));
        term.process_stdin_chunk(b"a");
        let guard = got.lock().unwrap();
        assert_eq!(guard.as_slice(), &["a".to_string()]);
    }

    #[test]
    fn parses_st_terminated_osc11_rgb() {
        assert_eq!(
            parse_terminal_background_color_response("\x1b]11;rgb:ffff/0000/8080\x1b\\"),
            Some((255, 0, 128))
        );
    }

    #[test]
    fn parses_bel_terminated_osc11_rgb() {
        assert_eq!(
            parse_terminal_background_color_response("\x1b]11;rgb:ff/80/00\x07"),
            Some((255, 128, 0))
        );
    }

    #[test]
    fn rejects_invalid_osc11_and_device_attributes() {
        assert_eq!(parse_terminal_background_color_response("garbage"), None);
        assert_eq!(parse_terminal_background_color_response("\x1b[?1;2c"), None);
        assert_eq!(device_attributes_response_offset("\x1b[?1;2c"), Some(0));
        assert_eq!(
            parse_terminal_background_color_response("\x1b[?1;2c\x1b]11;rgb:ffff/0000/8080\x1b\\"),
            None
        );
    }
}
