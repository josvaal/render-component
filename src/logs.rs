//! Shared log sink (C18): every producer (npm install, ng serve, control
//! server, watcher) appends here; the TUI renders the tail in a bottom panel
//! and headless mode mirrors every line to stderr. Nothing fails silently.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Keep at most this many lines (ring behavior via drain).
pub const MAX_LINES: usize = 5000;

#[derive(Clone, Debug)]
pub struct LogSink {
    inner: Arc<Mutex<Vec<String>>>,
    mirror_stderr: Arc<AtomicBool>,
}

impl LogSink {
    /// `mirror_stderr = true` also prints every line to stderr (headless mode).
    pub fn new(mirror_stderr: bool) -> Self {
        LogSink {
            inner: Arc::new(Mutex::new(Vec::new())),
            mirror_stderr: Arc::new(AtomicBool::new(mirror_stderr)),
        }
    }

    pub fn push(&self, line: impl AsRef<str>) {
        let stamped = format!("[{}] {}", hhmmss(), line.as_ref());
        if self.mirror_stderr.load(Ordering::Relaxed) {
            eprintln!("{stamped}");
        }
        let mut guard = self.inner.lock().unwrap();
        guard.push(stamped);
        if guard.len() > MAX_LINES {
            let drop = guard.len() - MAX_LINES;
            guard.drain(..drop);
        }
    }

    /// The last `n` lines, oldest first. Used by the TUI panel via `range`;
    /// kept for programmatic consumers.
    #[allow(dead_code)]
    pub fn tail(&self, n: usize) -> Vec<String> {
        let guard = self.inner.lock().unwrap();
        let start = guard.len().saturating_sub(n);
        guard[start..].to_vec()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// Lines in `[start, end)` (clamped), oldest first — used by the
    /// scrollable logs panel (C21).
    pub fn range(&self, start: usize, end: usize) -> Vec<String> {
        let guard = self.inner.lock().unwrap();
        let start = start.min(end).min(guard.len());
        let end = end.min(guard.len());
        guard[start..end].to_vec()
    }

    /// Write the FULL log to `path` (owner can share it verbatim). Returns the
    /// number of lines written.
    pub fn dump_to_file(&self, path: &Path) -> std::io::Result<usize> {
        let guard = self.inner.lock().unwrap();
        fs::write(path, guard.join("\n") + "\n")?;
        Ok(guard.len())
    }
}

/// UTC wall clock as HH:MM:SS (no chrono dependency; good enough for logs).
fn hhmmss() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{:02}:{:02}:{:02}",
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_returns_lines_in_order() {
        let sink = LogSink::new(false);
        sink.push("first");
        sink.push("second");
        sink.push("third");
        let tail = sink.tail(2);
        assert_eq!(tail.len(), 2);
        assert!(tail[0].ends_with("second"), "got: {tail:?}");
        assert!(tail[1].ends_with("third"));
    }

    #[test]
    fn buffer_is_bounded() {
        let sink = LogSink::new(false);
        for i in 0..(MAX_LINES + 50) {
            sink.push(format!("line {i}"));
        }
        let tail = sink.tail(MAX_LINES);
        assert_eq!(tail.len(), MAX_LINES);
        assert!(
            tail[0].ends_with("line 50"),
            "oldest kept line is line 50 (1050 - 1000 cap)"
        );
        assert!(
            tail.last()
                .unwrap()
                .ends_with(&format!("line {}", MAX_LINES + 49))
        );
    }

    #[test]
    fn range_is_clamped_and_ordered() {
        let sink = LogSink::new(false);
        for i in 0..20 {
            sink.push(format!("line {i}"));
        }
        assert_eq!(sink.len(), 20);
        let mid = sink.range(5, 10);
        assert_eq!(mid.len(), 5);
        assert!(mid[0].ends_with("line 5"));
        assert!(mid[4].ends_with("line 9"));
        // out-of-bounds end clamps to len
        assert_eq!(sink.range(15, 999).len(), 5);
        // empty range when start >= len
        assert!(sink.range(100, 200).is_empty());
    }

    #[test]
    fn dump_writes_full_log_to_file() {
        let sink = LogSink::new(false);
        for i in 0..25 {
            sink.push(format!("dump line {i}"));
        }
        let path = std::env::temp_dir().join("render-component-test-dump.log");
        let written = sink.dump_to_file(&path).expect("dump ok");
        assert_eq!(written, 25);
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("dump line 0"));
        assert!(content.contains("dump line 24"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn lines_are_timestamped() {
        let sink = LogSink::new(false);
        sink.push("hello");
        let line = sink.tail(1).remove(0);
        // [HH:MM:SS] hello
        assert!(
            line.starts_with('[') && line.contains("] hello"),
            "got: {line}"
        );
    }
}
