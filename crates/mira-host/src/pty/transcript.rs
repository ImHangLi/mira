//! Plain-text transcript lines from PTY output, for the run log.

use mira_protocol::limits::MAX_LOG_TEXT_BYTES;

use super::screen::printable;

/// Turns PTY output into plain transcript lines for the run log, across chunk boundaries.
/// Escape sequences of every kind are skipped; alternate-screen output is not recorded,
/// because full-screen programs redraw and the current screen is read by snapshot instead.
#[derive(Default)]
pub(super) struct Transcript {
    state: Tx,
    line: Vec<u8>,
    csi: Vec<u8>,
    pending_cr: bool,
    alternate: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Tx {
    #[default]
    Ground,
    Esc,
    EscIntermediate,
    Csi,
    /// OSC, DCS, SOS, PM, or APC string: skipped until BEL or ST.
    Str,
    StrEsc,
}

impl Transcript {
    pub(super) fn feed(&mut self, bytes: &[u8], out: &mut Vec<String>) {
        for &b in bytes {
            match self.state {
                Tx::Ground => self.ground(b, out),
                Tx::Esc => {
                    self.state = match b {
                        b'[' => {
                            self.csi.clear();
                            Tx::Csi
                        }
                        b']' | b'P' | b'X' | b'^' | b'_' => Tx::Str,
                        0x20..=0x2f => Tx::EscIntermediate,
                        0x1b => Tx::Esc,
                        _ => Tx::Ground,
                    }
                }
                Tx::EscIntermediate => {
                    if !(0x20..=0x2f).contains(&b) {
                        self.state = Tx::Ground;
                    }
                }
                Tx::Csi => match b {
                    0x20..=0x3f => {
                        if self.csi.len() < 64 {
                            self.csi.push(b);
                        }
                    }
                    0x40..=0x7e => {
                        self.csi_final(b);
                        self.state = Tx::Ground;
                    }
                    0x1b => self.state = Tx::Esc,
                    _ => self.state = Tx::Ground,
                },
                Tx::Str => match b {
                    0x07 => self.state = Tx::Ground,
                    0x1b => self.state = Tx::StrEsc,
                    _ => {}
                },
                Tx::StrEsc => {
                    self.state = match b {
                        b'\\' => Tx::Ground,
                        0x1b => Tx::StrEsc,
                        _ => Tx::Str,
                    }
                }
            }
        }
    }

    fn ground(&mut self, b: u8, out: &mut Vec<String>) {
        match b {
            0x1b => self.state = Tx::Esc,
            b'\n' => {
                self.pending_cr = false;
                if !self.alternate {
                    self.emit(out);
                }
            }
            b'\r' => self.pending_cr = true,
            0x08 => {
                self.line.pop();
            }
            b'\t' => self.push(b, out),
            0x00..=0x1f | 0x7f => {}
            _ => self.push(b, out),
        }
    }

    fn push(&mut self, b: u8, out: &mut Vec<String>) {
        if self.alternate {
            return;
        }
        if self.pending_cr {
            // A bare carriage return rewrites the line (progress bars): keep the latest text.
            self.pending_cr = false;
            self.line.clear();
        }
        self.line.push(b);
        if self.line.len() >= MAX_LOG_TEXT_BYTES {
            self.emit(out);
        }
    }

    fn csi_final(&mut self, fin: u8) {
        if (fin == b'h' || fin == b'l') && self.csi.first() == Some(&b'?') {
            let alt = self.csi[1..]
                .split(|c| *c == b';')
                .any(|p| matches!(p, b"1049" | b"1047" | b"47"));
            if alt {
                self.alternate = fin == b'h';
                self.line.clear();
                self.pending_cr = false;
            }
        }
    }

    fn emit(&mut self, out: &mut Vec<String>) {
        let text = printable(String::from_utf8_lossy(&self.line).into_owned());
        self.line.clear();
        out.push(text);
    }

    pub(super) fn finish(&mut self, out: &mut Vec<String>) {
        if !self.line.is_empty() && !self.alternate {
            self.emit(out);
        }
    }
}
