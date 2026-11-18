//! Bounded newline framing for an agent's structured stdout.
//!
//! The reader never holds more than `max` bytes of one frame: a longer frame is discarded as it streams past (so
//! memory stays bounded and the pipe keeps draining, which an agent blocked on a full pipe would otherwise stall on)
//! and reported once as a violation. A frame is never truncated into something that parses as a shorter, valid one.
//! Invalid UTF-8 is a violation too, not replaced: a protocol frame that is not text is not a protocol frame.
//! A final frame with no newline is delivered whole (the parser rejects it if it is cut JSON); the reader reports
//! it with `partial` so the caller can say so.

use std::io::{self, BufRead};

/// The longest frame accepted from an agent: 16 MiB, above any tool result or image block the supported clients emit
/// in one event, well below what would let a runaway child exhaust memory.
pub const MAX_FRAME: usize = 16 << 20;
/// The most of one turn's raw stream kept in events.jsonl: 256 MiB.
pub const MAX_EVENTS_LOG: u64 = 256 << 20;

/// One frame of a stream, or why the bytes at this position cannot be one.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// A whole line, without its line ending. `partial`: the stream ended before its newline.
    Line { text: String, partial: bool },
    /// Longer than the cap: reported the moment the cap is crossed, `bytes` being what had been seen by then (more
    /// than the cap); the rest of the frame is dropped unseen.
    Oversized { bytes: u64 },
    /// Not valid UTF-8; its bytes were dropped.
    InvalidUtf8,
}

pub struct FrameReader<R: BufRead> {
    inner: R,
    max: usize,
    /// The rest of an oversized frame is still arriving: dropped unseen until its newline.
    discarding: bool,
    lossy: bool,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R, max: usize) -> Self {
        FrameReader { inner, max: max.max(1), discarding: false, lossy: false }
    }

    /// Decode invalid UTF-8 as U+FFFD instead of refusing it: for free-text output that speaks no protocol.
    pub fn lossy(mut self) -> Self {
        self.lossy = true;
        self
    }

    /// The next frame; None at the end of the stream. An I/O error ends it as the end of the stream would
    /// (the caller's exit-status and completion checks decide what that means).
    pub fn next_frame(&mut self) -> Option<Frame> {
        if self.discarding {
            // Drain the rest of the oversized frame without keeping it.
            loop {
                let chunk = match self.inner.fill_buf() {
                    Ok(c) if !c.is_empty() => c,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    _ => return None,
                };
                match chunk.iter().position(|&b| b == b'\n') {
                    Some(i) => {
                        self.inner.consume(i + 1);
                        self.discarding = false;
                        break;
                    }
                    None => {
                        let n = chunk.len();
                        self.inner.consume(n);
                    }
                }
            }
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut any = false;
        loop {
            let chunk = match self.inner.fill_buf() {
                Ok(c) => c,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return self.finish(buf, any, false),
            };
            if chunk.is_empty() {
                return self.finish(buf, any, false);
            }
            any = true;
            let (take, done) = match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (chunk.len(), false),
            };
            let body = if done { &chunk[..take - 1] } else { chunk };
            if buf.len() + body.len() > self.max {
                // Reported as soon as the cap is crossed, not when the frame ends: a child that stalls mid-frame
                // must not hold the violation back. The rest of the frame is dropped by the next call.
                let seen = (buf.len() + body.len()) as u64;
                self.inner.consume(take);
                self.discarding = !done;
                return Some(Frame::Oversized { bytes: seen });
            }
            buf.extend_from_slice(body);
            self.inner.consume(take);
            if done {
                return self.finish(buf, any, true);
            }
        }
    }

    fn finish(&self, buf: Vec<u8>, any: bool, newline: bool) -> Option<Frame> {
        if !any {
            return None;
        }
        let mut text = match String::from_utf8(buf) {
            Ok(text) => text,
            Err(e) if self.lossy => String::from_utf8_lossy(e.as_bytes()).into_owned(),
            Err(_) => return Some(Frame::InvalidUtf8),
        };
        text.truncate(text.trim_end_matches('\r').len()); // in place: no second copy of a large frame
        Some(Frame::Line { text, partial: !newline })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor, Read};

    /// Delivers its bytes `n` at a time, so frames split at every offset.
    struct Drip<'a>(&'a [u8], usize);
    impl Read for Drip<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.1.min(self.0.len()).min(out.len());
            out[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    fn frames(bytes: &[u8], chunk: usize, max: usize) -> Vec<Frame> {
        let mut r = FrameReader::new(BufReader::with_capacity(chunk.max(1), Drip(bytes, chunk)), max);
        std::iter::from_fn(|| r.next_frame()).collect()
    }

    fn line(t: &str) -> Frame {
        Frame::Line { text: t.into(), partial: false }
    }

    #[test]
    fn frames_survive_every_split_and_coalescing() {
        let data = b"{\"a\":1}\n{\"b\":2}\r\n\n{\"c\":3}\n";
        let want = vec![line("{\"a\":1}"), line("{\"b\":2}"), line(""), line("{\"c\":3}")];
        for chunk in 1..=data.len() + 1 {
            assert_eq!(frames(data, chunk, 64), want, "chunk {chunk}");
        }
    }

    #[test]
    fn an_oversized_frame_is_dropped_not_truncated_and_the_next_frame_survives() {
        let mut data = vec![b'x'; 1000];
        data.extend_from_slice(b"\n{\"ok\":true}\n");
        for chunk in [1, 7, 64, 4096] {
            let got = frames(&data, chunk, 100);
            assert!(matches!(got[0], Frame::Oversized { bytes } if bytes > 100), "chunk {chunk}: {got:?}");
            assert_eq!(got[1..], [line("{\"ok\":true}")], "chunk {chunk}");
        }
    }

    #[test]
    fn a_frame_exactly_at_the_cap_passes_and_one_over_does_not() {
        assert_eq!(frames(b"12345\n", 3, 5), vec![line("12345")]);
        assert!(matches!(frames(b"123456\n", 3, 5)[..], [Frame::Oversized { .. }]));
    }

    #[test]
    fn newline_free_input_never_holds_more_than_the_cap() {
        // 8 MiB with no newline against a 1 KiB cap: one violation, at the end of the stream.
        let data = vec![b'a'; 8 << 20];
        let mut r = FrameReader::new(BufReader::with_capacity(8192, Cursor::new(&data)), 1024);
        assert!(matches!(r.next_frame(), Some(Frame::Oversized { bytes }) if bytes > 1024));
        assert_eq!(r.next_frame(), None);
    }

    #[test]
    fn invalid_utf8_is_refused_not_replaced() {
        for chunk in [1, 3, 64] {
            assert_eq!(frames(b"{\"a\":\"\xff\"}\n{\"b\":1}\n", chunk, 64), vec![Frame::InvalidUtf8, line("{\"b\":1}")]);
        }
    }

    #[test]
    fn a_frame_cut_at_the_end_of_the_stream_is_flagged_partial() {
        let got = frames(b"{\"a\":1}\n{\"b\":", 4, 64);
        assert_eq!(got, vec![line("{\"a\":1}"), Frame::Line { text: "{\"b\":".into(), partial: true }]);
    }

    #[test]
    fn lossy_mode_keeps_free_text_readable_but_still_caps_it() {
        let mut r = FrameReader::new(BufReader::new(Cursor::new(b"a\xffb\n".to_vec())), 64).lossy();
        assert_eq!(r.next_frame(), Some(line("a\u{fffd}b")));
    }

    #[test]
    fn an_empty_stream_has_no_frames() {
        assert!(frames(b"", 4, 64).is_empty());
    }
}
