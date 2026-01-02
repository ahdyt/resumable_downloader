use crossterm::terminal::size;
use regex::Regex;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

// =====================================
// ANSI stripping + visible-width truncation
// =====================================

lazy_static::lazy_static! {
    static ref ANSI_RE: Regex = Regex::new(r"\x1B\[[0-9;]*[A-Za-z]").unwrap();
}

fn visible_len(s: &str) -> usize {
    ANSI_RE.replace_all(s, "").chars().count()
}

fn truncate_ansi(s: &str, max_visible: usize) -> String {
    let mut out = String::new();
    let mut visible = 0;

    let mut iter = s.chars().peekable();

    while let Some(ch) = iter.peek().cloned() {
        if ch == '\x1B' {
            // Copy ANSI sequence fully
            out.push(ch);
            iter.next();

            while let Some(c) = iter.next() {
                out.push(c);
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }

        if visible >= max_visible {
            break;
        }

        out.push(ch);
        visible += 1;
        iter.next();
    }

    out
}

// =====================================
// Safe single-line update logic
// =====================================

fn safe_update(line: usize, content: &str, total_lines: usize) {
    let (width, _) = size().unwrap_or((120, 0));
    let safe = width.saturating_sub(1);

    let final_text = if visible_len(content) >= safe.into() {
        truncate_ansi(content, safe.into())
    } else {
        content.to_string()
    };

    let mut out = io::stdout();

    // move up from bottom to target line
    let up = total_lines.saturating_sub(line);
    write!(out, "\x1B[?7l").unwrap(); // disable wrap
    write!(out, "\x1B[{}A\r\x1B[2K", up).unwrap();
    write!(out, "{}", final_text).unwrap();
    write!(out, "\x1B[{}B", up).unwrap();
    write!(out, "\x1B[?7h").unwrap(); // re-enable wrap

    out.flush().unwrap();
}

// =====================================
// LineBuffer
// =====================================

#[derive(Clone)]
pub struct LineBuffer {
    inner: Arc<Mutex<LineBufferInner>>,
}

struct LineBufferInner {
    lines: Vec<String>,
}

impl LineBuffer {
    pub fn new(total: usize) -> Self {
        let mut out = io::stdout();
        for _ in 0..total {
            writeln!(out).unwrap();
        }
        out.flush().unwrap();

        Self {
            inner: Arc::new(Mutex::new(LineBufferInner {
                lines: vec![String::new(); total],
            })),
        }
    }

    pub fn len(&self) -> usize {
        let state = self.inner.lock().unwrap();
        state.lines.len()
    }

    pub fn resize(&self, new_size: usize) {
        let mut state = self.inner.lock().unwrap();

        if new_size > state.lines.len() {
            let diff = new_size - state.lines.len();

            let mut out = io::stdout();
            for _ in 0..diff {
                writeln!(out).unwrap();
            }
            out.flush().unwrap();

            state.lines.extend((0..diff).map(|_| String::new()));
        }
    }

    pub fn set(&self, idx: usize, content: impl Into<String>) {
        let mut state = self.inner.lock().unwrap();
        if idx < state.lines.len() {
            state.lines[idx] = content.into();
        }
    }

    // Only update the line that changed, avoids flicker
    pub fn flush_line(&self, idx: usize) {
        let state = self.inner.lock().unwrap();
        let total = state.lines.len();
        if idx < total {
            safe_update(idx, &state.lines[idx], total);
        }
    }
}

// =====================================
// ProgressManager
// =====================================

#[derive(Clone)]
pub struct ProgressManager {
    buf: LineBuffer,
    inner: Arc<Mutex<ProgressState>>,
}

struct ProgressState {
    lines: usize,
}

impl ProgressManager {
    pub fn new() -> Self {
        Self {
            buf: LineBuffer::new(0),
            inner: Arc::new(Mutex::new(ProgressState { lines: 0 })),
        }
    }

    pub fn register(&self) -> usize {
        let mut state = self.inner.lock().unwrap();
        let id = state.lines;
        state.lines += 1;
        let new_total = state.lines;
        drop(state);

        self.buf.resize(new_total);
        id
    }

    // Update single line only
    pub fn update(&self, line: usize, content: &str) {
        self.buf.set(line, content.to_string());
        self.buf.flush_line(line);
    }
}
