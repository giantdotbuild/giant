//! Default tty renderer - consumes events and renders to stdout.
//!
//! See TDD-0010 for layout and modes (tty / plain / NDJSON pass-through).

use crate::events::Event;
use std::io::Write;

#[derive(Debug, Clone, Copy)]
pub enum Mode {
    Tty,
    Plain,
    Ndjson,
}

pub struct Renderer<W: Write> {
    out: W,
    mode: Mode,
}

impl<W: Write> Renderer<W> {
    pub fn new(out: W, mode: Mode) -> Self {
        Self { out, mode }
    }

    pub fn consume(&mut self, ev: Event) -> std::io::Result<()> {
        match self.mode {
            Mode::Ndjson => {
                let line = serde_json::to_string(&ev).expect("event serialization");
                writeln!(self.out, "{line}")
            }
            Mode::Plain | Mode::Tty => {
                // TDD-0010 layout - placeholder line for now.
                writeln!(self.out, "{ev:?}")
            }
        }
    }
}

/// Decide the mode at startup based on env, tty detection, and flags.
pub fn detect_mode(force_color: bool, ndjson: bool, stdout_is_tty: bool) -> Mode {
    if ndjson {
        Mode::Ndjson
    } else if stdout_is_tty || force_color {
        Mode::Tty
    } else {
        Mode::Plain
    }
}
