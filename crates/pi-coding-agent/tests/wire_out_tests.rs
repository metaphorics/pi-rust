//! Tests for `pi_coding_agent::wire_out`.

use parking_lot::Mutex;
use pi_coding_agent::wire_out::WireOut;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// In-memory sink shared across the writer thread and test assertions.
struct CaptureWriter {
    buf: Arc<Mutex<Vec<u8>>>,
}

impl Write for CaptureWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.lock().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn new_capture() -> (WireOut, Arc<Mutex<Vec<u8>>>) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let writer = CaptureWriter {
        buf: Arc::clone(&buf),
    };
    let wire = WireOut::new_with_writer(Box::new(writer));
    (wire, buf)
}

#[test]
fn writes_appear_in_order_for_interleaved_producers() {
    let (wire, buf) = new_capture();
    let wire = Arc::new(wire);
    // Round-robin ownership: thread `t` enqueues tokens t, t+8, t+16, ...
    // A shared turn gate forces true cross-thread interleaving and a known
    // enqueue order that the capture must match exactly.
    let turn = Arc::new(AtomicUsize::new(0));
    let total = 64usize;
    let n_threads = 8usize;

    let mut handles = Vec::new();
    for t in 0..n_threads {
        let wire = Arc::clone(&wire);
        let turn = Arc::clone(&turn);
        handles.push(thread::spawn(move || {
            let mut n = t;
            while n < total {
                while turn.load(Ordering::Acquire) != n {
                    thread::yield_now();
                }
                wire.write(&format!("[{n:03}]"));
                turn.store(n + 1, Ordering::Release);
                n += n_threads;
            }
        }));
    }
    for h in handles {
        h.join().expect("producer join");
    }
    wire.flush();

    let expected: String = (0..total).map(|n| format!("[{n:03}]")).collect();
    let s = String::from_utf8(buf.lock().clone()).expect("utf8");
    assert_eq!(s, expected, "writes must appear in enqueue order");
}

#[test]
fn flush_returns_only_after_all_bytes_visible() {
    let (wire, buf) = new_capture();

    wire.write("hello");
    wire.write(" ");
    wire.write("world");
    // Before flush the writer thread may still be catching up; after flush
    // every enqueued byte must be visible.
    wire.flush();

    let s = String::from_utf8(buf.lock().clone()).expect("utf8");
    assert_eq!(s, "hello world");
}

#[test]
fn empty_write_is_noop() {
    let (wire, buf) = new_capture();

    wire.write("");
    wire.write("x");
    wire.write("");
    wire.flush();

    let s = String::from_utf8(buf.lock().clone()).expect("utf8");
    assert_eq!(s, "x");
}

/// Files under `src/` allowed to call `print!` / `println!`.
///
/// Keep this list tight: json/rpc modes must not write stdout outside wire_out.
/// Allowlisted paths are genuinely CLI / user-facing entrypoints only.
///
/// Current scan (run before expanding): only `src/main.rs` had println!; no
/// other offenders under `src/`. Do not add session/modes/tools paths here.
fn is_print_allowlisted(rel: &str) -> bool {
    // Exact leaf module for the raw writer itself.
    if rel == "src/wire_out.rs" {
        return true;
    }
    // Grandfathered: pre-B1 --version/--help prints in the boot stub.
    // The modes wave (B2) owns main.rs and must route these through
    // wire_out (or stderr) and then remove this exception.
    if rel == "src/main.rs" {
        return true;
    }
    false
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!("read_dir {}: {e}", dir.display());
    });
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.is_dir() {
            collect_rs_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Match `print!(` / `println!(` only when the preceding char is not an
/// identifier char — so `eprint!` / `eprintln!` / `dbg_print!` are ignored.
fn line_has_stdout_print_macro(line: &str) -> bool {
    for (i, _) in line.match_indices("print") {
        let rest = &line[i..];
        let is_println = rest.starts_with("println!(");
        let is_print = rest.starts_with("print!(");
        if !is_println && !is_print {
            continue;
        }
        let prev_ok = if i == 0 {
            true
        } else {
            let prev = line.as_bytes()[i - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_'
        };
        if prev_ok {
            return true;
        }
    }
    false
}

#[test]
fn stdout_purity_no_print_macros_outside_allowlist() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");
    assert!(src_dir.is_dir(), "missing src dir at {}", src_dir.display());

    let mut files = Vec::new();
    collect_rs_files(&src_dir, &mut files);
    files.sort();

    let mut offenders: Vec<String> = Vec::new();
    for path in &files {
        let rel = path
            .strip_prefix(&manifest_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if is_print_allowlisted(&rel) {
            continue;
        }
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
            panic!("read {}: {e}", path.display());
        });
        for (idx, line) in text.lines().enumerate() {
            // Token-boundary match so `eprintln!` / `eprint!` (stderr) pass.
            if line_has_stdout_print_macro(line) {
                offenders.push(format!("{rel}:{}: {}", idx + 1, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "print!/println! outside allowlist (wire_out.rs, grandfathered main.rs). \
         In json/rpc modes only wire_out may write stdout.\n{}",
        offenders.join("\n")
    );
}
