use std::io::{self, Write};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ProgressManager {
    inner: Arc<Mutex<ProgressState>>,
}

struct ProgressState {
    lines: usize, // how many progress lines exist
}

impl ProgressManager {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(ProgressState { lines: 0 })),
        }
    }

    pub fn register(&self) -> usize {
        let mut state = self.inner.lock().unwrap();
        let id = state.lines;
        state.lines += 1;

        // print an empty line for this progress bar
        println!();

        id
    }

    pub fn update(&self, line: usize, content: &str) {
        let state = self.inner.lock().unwrap();
        let total_lines = state.lines;
        drop(state);

        let mut out = io::stdout();

        // move cursor UP from bottom to target line
        let up = total_lines - line;
        write!(out, "\x1B[{}A", up).unwrap();

        // clear line + write
        write!(out, "\x1B[2K{}\n", content).unwrap();

        // move cursor DOWN back to bottom
        write!(out, "\x1B[{}B", up - 1).unwrap();

        out.flush().unwrap();
    }
}
