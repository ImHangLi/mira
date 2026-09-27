//! Canonical run logs: `000001.jsonl` segments, ≤8 KiB records split on UTF-8
//! boundaries, buffered writes flushed every 250 ms or 64 KiB, and a per-run byte cap that
//! rotates out the oldest segments even while the run is active.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mira_protocol::ids::LogSeq;
use mira_protocol::limits::MAX_LOG_TEXT_BYTES;
use mira_protocol::run::{LogRecord, LogStream};
use mira_protocol::time::Timestamp;
use mira_protocol::view::LogLevel;

const SEGMENT_MAX_BYTES: u64 = 4 * 1024 * 1024;
const FLUSH_BYTES: usize = 64 * 1024;
pub const FLUSH_EVERY: Duration = Duration::from_millis(250);
/// Recent records kept in memory for cheap tail reads and streaming.
pub const RING_RECORDS: usize = 2000;

pub type SharedLog = Arc<Mutex<RunLog>>;

struct Segment {
    number: u32,
    first_seq: u64,
    bytes: u64,
}

pub struct RunLog {
    dir: PathBuf,
    next_seq: u64,
    segments: VecDeque<Segment>,
    writer: Option<BufWriter<File>>,
    unflushed: usize,
    last_flush: Instant,
    cap_bytes: u64,
    /// Segment size: at most 4 MiB, and small enough that the cap is honored.
    segment_max: u64,
    total_bytes: u64,
    ring: VecDeque<LogRecord>,
    /// Records that could not be written (IO failure); never silently zero.
    pub dropped: u64,
    /// Records split because they exceeded 8 KiB.
    pub truncated: u64,
    pub write_error: Option<String>,
    partial: [Vec<u8>; 2],
    /// Per-stream ANSI escape state, so sequences split across reads are still removed.
    ansi: [AnsiStrip; 2],
}

/// Removes ANSI escape sequences from pipe output: CSI (`ESC [ ... final`), OSC, DCS, SOS,
/// PM, and APC strings (`ESC ] ... BEL` or `ESC ] ... ESC \`), and two-byte `ESC x`. Only the
/// parser state is kept between reads, so memory stays bounded. A newline always ends an
/// open sequence and is kept, so a stray ESC cannot hide the following lines.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum AnsiStrip {
    #[default]
    Ground,
    Esc,
    EscIntermediate,
    Csi,
    Str,
    StrEsc,
}

impl AnsiStrip {
    fn feed(&mut self, bytes: &[u8], out: &mut Vec<u8>) {
        for &b in bytes {
            if b == b'\n' {
                *self = Self::Ground;
                out.push(b);
                continue;
            }
            *self = match (*self, b) {
                (Self::Ground, 0x1b) => Self::Esc,
                (Self::Ground, _) => {
                    out.push(b);
                    Self::Ground
                }
                (Self::Esc, b'[') => Self::Csi,
                (Self::Esc, b']' | b'P' | b'X' | b'^' | b'_') => Self::Str,
                (Self::Esc, 0x20..=0x2f) => Self::EscIntermediate,
                (Self::Esc, 0x1b) => Self::Esc,
                (Self::Esc, 0x80..) => {
                    // Not an escape sequence: keep the byte so UTF-8 text stays intact.
                    out.push(b);
                    Self::Ground
                }
                (Self::Esc, _) => Self::Ground,
                (Self::EscIntermediate, 0x20..=0x2f) => Self::EscIntermediate,
                (Self::EscIntermediate, _) => Self::Ground,
                (Self::Csi, 0x20..=0x3f) => Self::Csi,
                (Self::Csi, 0x1b) => Self::Esc,
                (Self::Csi, 0x40..=0x7e) => Self::Ground,
                (Self::Csi, _) => {
                    out.push(b);
                    Self::Ground
                }
                (Self::Str, 0x07) => Self::Ground,
                (Self::Str, 0x1b) => Self::StrEsc,
                (Self::Str, _) => Self::Str,
                (Self::StrEsc, b'\\') => Self::Ground,
                (Self::StrEsc, 0x1b) => Self::StrEsc,
                (Self::StrEsc, _) => Self::Str,
            };
        }
    }
}

fn segment_name(n: u32) -> String {
    format!("{n:06}.jsonl")
}

/// Splits text into ≤8 KiB chunks on character boundaries.
fn chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while rest.len() > MAX_LOG_TEXT_BYTES {
        let mut end = MAX_LOG_TEXT_BYTES;
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        out.push(&rest[..end]);
        rest = &rest[end..];
    }
    out.push(rest);
    out
}

impl RunLog {
    pub fn create(dir: PathBuf, cap_bytes: u64) -> Self {
        let mut log = Self {
            dir,
            next_seq: 1,
            segments: VecDeque::new(),
            writer: None,
            unflushed: 0,
            last_flush: Instant::now(),
            cap_bytes,
            segment_max: SEGMENT_MAX_BYTES.min((cap_bytes / 4).max(64 * 1024)),
            total_bytes: 0,
            ring: VecDeque::new(),
            dropped: 0,
            truncated: 0,
            write_error: None,
            partial: [Vec::new(), Vec::new()],
            ansi: [AnsiStrip::Ground; 2],
        };
        if let Err(e) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&log.dir)
        {
            log.write_error = Some(format!("cannot create log dir: {e}"));
        }
        log
    }

    /// Opens an existing run log for reading (finished runs from earlier host epochs).
    pub fn open_existing(dir: PathBuf) -> Self {
        let mut log = Self::create_readonly(dir);
        let mut numbers: Vec<u32> = std::fs::read_dir(&log.dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str()?.strip_suffix(".jsonl")?.parse().ok())
                    .collect()
            })
            .unwrap_or_default();
        numbers.sort_unstable();
        for n in numbers {
            let path = log.dir.join(segment_name(n));
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let first = read_segment(&path)
                .first()
                .map_or(log.next_seq, |r| r.log_seq.get());
            if let Some(last) = read_segment(&path).last() {
                log.next_seq = last.log_seq.get() + 1;
            }
            log.total_bytes += bytes;
            log.segments.push_back(Segment {
                number: n,
                first_seq: first,
                bytes,
            });
        }
        log
    }

    fn create_readonly(dir: PathBuf) -> Self {
        Self {
            dir,
            next_seq: 1,
            segments: VecDeque::new(),
            writer: None,
            unflushed: 0,
            last_flush: Instant::now(),
            cap_bytes: u64::MAX,
            segment_max: SEGMENT_MAX_BYTES,
            total_bytes: 0,
            ring: VecDeque::new(),
            dropped: 0,
            truncated: 0,
            write_error: None,
            partial: [Vec::new(), Vec::new()],
            ansi: [AnsiStrip::Ground; 2],
        }
    }

    pub fn first_available(&self) -> Option<LogSeq> {
        self.segments
            .front()
            .map(|s| s.first_seq)
            .or_else(|| self.ring.front().map(|r| r.log_seq.get()))
            .and_then(|v| LogSeq::new(v).ok())
    }

    pub fn last_available(&self) -> Option<LogSeq> {
        (self.next_seq > 1)
            .then(|| LogSeq::new(self.next_seq - 1).ok())
            .flatten()
    }

    fn open_segment(&mut self) -> Result<(), String> {
        let number = self.segments.back().map_or(1, |s| s.number + 1);
        let path = self.dir.join(segment_name(number));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        self.writer = Some(BufWriter::with_capacity(FLUSH_BYTES, file));
        self.segments.push_back(Segment {
            number,
            first_seq: self.next_seq,
            bytes: 0,
        });
        Ok(())
    }

    fn rotate_if_needed(&mut self) {
        let full = self
            .segments
            .back()
            .is_none_or(|s| s.bytes >= self.segment_max);
        if full {
            if let Some(w) = self.writer.as_mut() {
                let _ = w.flush();
            }
            self.writer = None;
            if let Err(e) = self.open_segment() {
                self.write_error = Some(e);
            }
        }
        // Enforce the per-run cap by removing the oldest closed segments.
        while self.total_bytes > self.cap_bytes && self.segments.len() > 1 {
            if let Some(old) = self.segments.pop_front() {
                let _ = std::fs::remove_file(self.dir.join(segment_name(old.number)));
                self.total_bytes = self.total_bytes.saturating_sub(old.bytes);
            }
        }
    }

    fn append(&mut self, record: LogRecord) {
        match serde_json::to_vec(&record) {
            Ok(mut line) if self.write_error.is_none() => {
                line.push(b'\n');
                self.rotate_if_needed();
                let len = line.len() as u64;
                match self.writer.as_mut().map(|w| w.write_all(&line)) {
                    Some(Ok(())) => {
                        if let Some(s) = self.segments.back_mut() {
                            s.bytes += len;
                        }
                        self.total_bytes += len;
                        self.unflushed += line.len();
                    }
                    Some(Err(e)) => {
                        self.write_error = Some(e.to_string());
                        self.dropped += 1;
                    }
                    None => self.dropped += 1,
                }
            }
            _ => self.dropped += 1,
        }
        self.next_seq += 1;
        self.ring.push_back(record);
        if self.ring.len() > RING_RECORDS {
            self.ring.pop_front();
        }
        if self.unflushed >= FLUSH_BYTES {
            self.flush();
        }
    }

    /// Records one line of text (without its newline) and returns the created records.
    pub fn push_line(
        &mut self,
        stream: LogStream,
        level: LogLevel,
        text: &str,
        continued_from_previous: bool,
    ) -> Vec<LogRecord> {
        self.push_line_fields(stream, level, text, continued_from_previous, None)
    }

    /// Like [`Self::push_line`], with optional structured fields on the first record.
    pub fn push_line_fields(
        &mut self,
        stream: LogStream,
        level: LogLevel,
        text: &str,
        continued_from_previous: bool,
        mut fields: Option<serde_json::Map<String, serde_json::Value>>,
    ) -> Vec<LogRecord> {
        let parts = chunks(text);
        let n = parts.len();
        if n > 1 {
            self.truncated += 1;
        }
        let mut out = Vec::with_capacity(n);
        for (i, part) in parts.into_iter().enumerate() {
            let Ok(seq) = LogSeq::new(self.next_seq) else {
                break;
            };
            let record = LogRecord {
                log_seq: seq,
                recorded_at: Timestamp::now(),
                stream,
                level,
                text: part.to_owned(),
                continued: continued_from_previous || i > 0,
                truncated: n > 1 && i + 1 < n,
                fields: fields.take().filter(|f| !f.is_empty()),
            };
            out.push(record.clone());
            self.append(record);
        }
        out
    }

    /// Feeds raw pipe bytes. Complete lines become records; an unterminated tail longer than
    /// 8 KiB is emitted early as a continued record so memory stays bounded.
    pub fn push_bytes(&mut self, stream: LogStream, bytes: &[u8]) -> Vec<LogRecord> {
        let idx = usize::from(stream == LogStream::Stderr);
        // Many tools write ordinary progress to stderr; the stream already marks it, so
        // stderr is not a warning by itself.
        let level = LogLevel::Info;
        let mut out = Vec::new();
        let mut buf = std::mem::take(&mut self.partial[idx]);
        self.ansi[idx].feed(bytes, &mut buf);
        let mut start = 0;
        while let Some(pos) = buf[start..].iter().position(|b| *b == b'\n') {
            let mut line = &buf[start..start + pos];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            out.extend(self.push_line(stream, level, &String::from_utf8_lossy(line), false));
            start += pos + 1;
        }
        // One read can add many chunks, so drain until the tail fits.
        while buf.len() - start > MAX_LOG_TEXT_BYTES {
            let chunk = &buf[start..start + MAX_LOG_TEXT_BYTES];
            // Cut at the last UTF-8 boundary; bytes that are not UTF-8 go out whole, lossily.
            let cut = match std::str::from_utf8(chunk) {
                Ok(_) => MAX_LOG_TEXT_BYTES,
                Err(e) if e.valid_up_to() > 0 => e.valid_up_to(),
                Err(_) => MAX_LOG_TEXT_BYTES,
            };
            let head = String::from_utf8_lossy(&buf[start..start + cut]).into_owned();
            let mut records = self.push_line(stream, level, &head, false);
            if let Some(r) = records.last_mut() {
                r.truncated = true;
            }
            out.extend(records);
            start += cut;
        }
        self.partial[idx] = buf[start..].to_vec();
        out
    }

    /// Emits any unterminated tails (at EOF).
    pub fn finish_streams(&mut self) -> Vec<LogRecord> {
        let mut out = Vec::new();
        for (idx, stream) in [(0, LogStream::Stdout), (1, LogStream::Stderr)] {
            let rest = std::mem::take(&mut self.partial[idx]);
            if !rest.is_empty() {
                out.extend(self.push_line(
                    stream,
                    LogLevel::Info,
                    &String::from_utf8_lossy(&rest),
                    false,
                ));
            }
        }
        out
    }

    pub fn flush_due(&self) -> bool {
        self.unflushed > 0 && self.last_flush.elapsed() >= FLUSH_EVERY
    }

    pub fn flush(&mut self) {
        if let Some(w) = self.writer.as_mut()
            && let Err(e) = w.flush()
        {
            self.write_error = Some(e.to_string());
        }
        self.unflushed = 0;
        self.last_flush = Instant::now();
    }

    /// Up to `limit` records with `log_seq < before` (or the tail), oldest first.
    pub fn read_before(&mut self, before: Option<u64>, limit: usize) -> Vec<LogRecord> {
        self.flush();
        let upper = before.unwrap_or(u64::MAX);
        let from_ring: Vec<LogRecord> = self
            .ring
            .iter()
            .filter(|r| r.log_seq.get() < upper)
            .cloned()
            .collect();
        // The ring suffices when it holds enough records or starts at the oldest kept record.
        let ring_complete = self.ring.front().map(|r| r.log_seq.get())
            == self.segments.front().map(|s| s.first_seq);
        let mut records = if from_ring.len() >= limit || ring_complete || self.segments.is_empty() {
            from_ring
        } else {
            let mut all = Vec::new();
            for s in &self.segments {
                if s.first_seq >= upper {
                    break;
                }
                all.extend(
                    read_segment(&self.dir.join(segment_name(s.number)))
                        .into_iter()
                        .filter(|r| r.log_seq.get() < upper),
                );
            }
            all
        };
        if records.len() > limit {
            records.drain(..records.len() - limit);
        }
        records
    }
}

impl RunLog {
    /// Up to `limit` records with `from <= log_seq <= upto`, oldest first.
    pub fn read_range(&mut self, from: u64, upto: u64, limit: usize) -> Vec<LogRecord> {
        self.flush();
        let in_range = |r: &LogRecord| (from..=upto).contains(&r.log_seq.get());
        let ring_covers = self.ring.front().is_some_and(|r| r.log_seq.get() <= from);
        if ring_covers || self.segments.is_empty() {
            return self
                .ring
                .iter()
                .filter(|r| in_range(r))
                .take(limit)
                .cloned()
                .collect();
        }
        let mut out = Vec::new();
        for (i, s) in self.segments.iter().enumerate() {
            if s.first_seq > upto {
                break;
            }
            // Skip segments that end before `from`.
            if self
                .segments
                .get(i + 1)
                .is_some_and(|next| next.first_seq <= from)
            {
                continue;
            }
            for r in read_segment(&self.dir.join(segment_name(s.number))) {
                if in_range(&r) {
                    out.push(r);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }
}

/// Parses complete LF-terminated records; a torn final line is ignored.
fn read_segment(path: &Path) -> Vec<LogRecord> {
    let Ok(f) = File::open(path) else {
        return vec![];
    };
    let mut out = Vec::new();
    let mut reader = BufReader::new(f);
    let mut line = Vec::new();
    while let Ok(n) = reader.read_until(b'\n', &mut line) {
        if n == 0 || line.last() != Some(&b'\n') {
            break;
        }
        if let Ok(r) = serde_json::from_slice::<LogRecord>(&line[..line.len() - 1]) {
            out.push(r);
        }
        line.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mira-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn output_without_newlines_keeps_the_tail_bounded() {
        let dir = scratch("tail");
        let mut log = RunLog::create(dir.clone(), 64 * 1024 * 1024);
        let chunk = vec![b'a'; 64 * 1024];
        let mut records = 0;
        for _ in 0..16 {
            records += log.push_bytes(LogStream::Stdout, &chunk).len();
            assert!(log.partial[0].len() <= MAX_LOG_TEXT_BYTES);
        }
        // The last full chunk stays buffered until more output or a newline arrives.
        assert_eq!(records, 16 * 64 * 1024 / MAX_LOG_TEXT_BYTES - 1);
        assert_eq!(log.partial[0].len(), MAX_LOG_TEXT_BYTES);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn invalid_utf8_without_newlines_still_drains() {
        let dir = scratch("utf8");
        let mut log = RunLog::create(dir.clone(), 64 * 1024 * 1024);
        let chunk = vec![0xff; 64 * 1024];
        let records = log.push_bytes(LogStream::Stdout, &chunk);
        assert!(log.partial[0].len() <= MAX_LOG_TEXT_BYTES);
        assert!(records.len() >= 7);
        assert!(records.iter().all(|r| r.truncated));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn escape_sequences_are_removed_across_reads() {
        let dir = scratch("ansi");
        let mut log = RunLog::create(dir.clone(), 1024 * 1024);
        let mut records = log.push_bytes(LogStream::Stdout, b"a\x1b[?25hb\x1b[2");
        records.extend(log.push_bytes(
            LogStream::Stdout,
            b"Kc\n\x1b]0;title\x07d\x1b]8;;u\x1b\\e\x1b7f\n",
        ));
        let texts: Vec<_> = records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["abc", "def"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn complete_lines_are_split_on_newlines() {
        let dir = scratch("lines");
        let mut log = RunLog::create(dir.clone(), 1024 * 1024);
        let records = log.push_bytes(LogStream::Stdout, b"one\r\ntwo\nthr");
        let texts: Vec<_> = records.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
        assert_eq!(log.partial[0], b"thr");
        let err = log.push_bytes(LogStream::Stderr, b"progress\n");
        assert_eq!(err[0].level, LogLevel::Info);
        assert_eq!(err[0].stream, LogStream::Stderr);
        let _ = std::fs::remove_dir_all(dir);
    }
}
