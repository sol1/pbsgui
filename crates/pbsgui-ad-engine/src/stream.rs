//! Bounded-channel plumbing between the tar-building thread and the PBS
//! uploader, so a System State backup streams with bounded memory instead of
//! staging a multi-gigabyte archive on disk.
//!
//! End of stream is deliberately explicit: the reader reports EOF only when the
//! producer confirmed success over the verdict channel. A producer that fails or
//! is dropped mid-stream surfaces as an I/O error, which fails the upload before
//! it can commit a truncated snapshot (the same rule the SQL engine's VDI stream
//! follows).

use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, SyncSender};

/// Buffered writer end: accumulates into chunks and sends them to the reader.
/// The producer thread writes the tar into this.
pub struct ChannelWriter {
    tx: SyncSender<Vec<u8>>,
    buf: Vec<u8>,
}

/// Chunk size for the writer -> reader channel. Big enough to amortize channel
/// overhead, small enough that the bounded channel keeps memory modest.
const CHUNK: usize = 256 * 1024;

impl ChannelWriter {
    pub fn new(tx: SyncSender<Vec<u8>>) -> Self {
        Self {
            tx,
            buf: Vec::with_capacity(CHUNK),
        }
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= CHUNK {
            let rest = self.buf.split_off(CHUNK);
            let chunk = std::mem::replace(&mut self.buf, rest);
            self.tx
                .send(chunk)
                .map_err(|_| std::io::Error::other("the upload side closed"))?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buf.is_empty() {
            let chunk = std::mem::take(&mut self.buf);
            self.tx
                .send(chunk)
                .map_err(|_| std::io::Error::other("the upload side closed"))?;
        }
        Ok(())
    }
}

/// Reader end, handed to the PBS uploader. EOF is reported only when the data
/// channel has drained AND the producer sent a `true` verdict; a `false` verdict
/// or a dropped verdict sender (the producer failed or panicked) becomes an
/// error so the uploader never finalizes a partial stream.
pub struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    done: Receiver<bool>,
    buf: Vec<u8>,
    pos: usize,
    verdict: Option<bool>,
}

impl ChannelReader {
    pub fn new(rx: Receiver<Vec<u8>>, done: Receiver<bool>) -> Self {
        Self {
            rx,
            done,
            buf: Vec::new(),
            pos: 0,
            verdict: None,
        }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.rx.recv() {
                Ok(next) => {
                    self.buf = next;
                    self.pos = 0;
                }
                Err(_) => {
                    let ok = match self.verdict {
                        Some(v) => v,
                        None => {
                            let v = self.done.recv().unwrap_or(false);
                            self.verdict = Some(v);
                            v
                        }
                    };
                    if ok {
                        return Ok(0);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the archive stream ended before the producer confirmed success; \
                         refusing to upload a truncated backup",
                    ));
                }
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn round_trips_and_ends_cleanly_on_a_good_verdict() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut w = ChannelWriter::new(tx);
        w.write_all(b"hello ").unwrap();
        w.write_all(&vec![7u8; CHUNK]).unwrap(); // crosses the chunk boundary
        w.flush().unwrap();
        drop(w);
        done_tx.send(true).unwrap();

        let mut out = Vec::new();
        ChannelReader::new(rx, done_rx)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out.len(), 6 + CHUNK);
        assert_eq!(&out[..6], b"hello ");
    }

    #[test]
    fn errors_when_the_producer_reports_failure() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        tx.send(b"partial".to_vec()).unwrap();
        drop(tx);
        done_tx.send(false).unwrap();

        let mut out = Vec::new();
        let err = ChannelReader::new(rx, done_rx)
            .read_to_end(&mut out)
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn errors_when_the_producer_vanishes() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(4);
        let (done_tx, done_rx) = std::sync::mpsc::channel::<bool>();
        drop(tx);
        drop(done_tx); // producer thread died without a verdict

        let mut out = Vec::new();
        assert!(ChannelReader::new(rx, done_rx)
            .read_to_end(&mut out)
            .is_err());
    }
}
