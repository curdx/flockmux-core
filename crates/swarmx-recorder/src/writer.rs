//! asciicast v2 writer: header line + per-chunk `[delta, "o", data]` events.
//!
//! All disk I/O lives on a dedicated tokio task so the PTY pump never
//! blocks. The channel is unbounded — local-disk writes are fast enough
//! that bounding it would just hide a bug if it ever fell behind, and the
//! pump is the slow consumer of the PTY anyway.

use anyhow::{Context, Result};
use bytes::Bytes;
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::fs::File;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::sync::{mpsc, oneshot};

/// Config for opening a new recording.
#[derive(Debug, Clone)]
pub struct RecorderConfig {
    pub agent_id: String,
    pub cols: u16,
    pub rows: u16,
    /// Unix-ms timestamp the recording is logically anchored to. The cast
    /// header `timestamp` field is derived from this (asciicast v2 stores
    /// seconds-since-epoch). All per-event deltas are measured from
    /// [`Recorder::start`] returning, not from this value.
    pub started_at_ms: i64,
    pub file_path: PathBuf,
    /// Soft cap on bytes written to this single `.cast` (0 = unlimited). Past
    /// it the writer keeps draining the channel but stops writing to disk, so a
    /// runaway agent can't produce an unbounded recording file.
    pub max_bytes: u64,
}

/// Default soft cap for a single recording (64 MiB). A chatty agent rarely
/// exceeds a few MiB; this bounds the pathological case without a config knob.
pub const DEFAULT_MAX_CAST_BYTES: u64 = 64 * 1024 * 1024;

/// Result emitted by the writer task when the recording finalizes.
#[derive(Debug, Clone)]
pub struct FinalizeResult {
    pub finalized_at_ms: i64,
    pub duration_ms: i64,
    /// Total bytes recorded across every chunk (== sum of chunk lengths).
    pub last_seq: i64,
}

/// Cheap-to-clone push-side handle. The PTY pump holds at least one of
/// these; cloning it adds another producer. When every clone is dropped
/// the writer task observes EOF on the channel and finalizes.
#[derive(Clone)]
pub struct RecorderHandle {
    tx: mpsc::UnboundedSender<Bytes>,
}

impl RecorderHandle {
    /// Non-blocking enqueue. Silently no-ops if the writer task has
    /// already exited (e.g. earlier disk error).
    pub fn write_chunk(&self, chunk: Bytes) {
        // `send` on UnboundedSender only errors if the receiver is gone.
        let _ = self.tx.send(chunk);
    }
}

/// The recorder. The constructor opens the file + writes the header
/// synchronously (before returning) so spawn-time errors surface
/// immediately. The background writer task starts before the constructor
/// returns and runs until every [`RecorderHandle`] is dropped.
pub struct Recorder {
    /// The "owner" handle — the constructor returns one *implicit* handle
    /// inside the struct so callers must explicitly call [`Self::handle`]
    /// to get one for the pump. Dropping `Recorder` (or calling
    /// [`Self::wait_finalize`]) closes this owner copy.
    owner_handle: RecorderHandle,
    finalize_rx: oneshot::Receiver<FinalizeResult>,
}

impl Recorder {
    /// Open the .cast file, write the asciicast v2 header, and spawn the
    /// writer task. Errors before the writer task starts: file open / header
    /// write / parent directory missing.
    pub async fn start(cfg: RecorderConfig) -> Result<Self> {
        if let Some(parent) = cfg.file_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create recording dir {}", parent.display()))?;
        }
        let f = File::create(&cfg.file_path)
            .await
            .with_context(|| format!("create cast file {}", cfg.file_path.display()))?;
        let mut writer = BufWriter::new(f);

        // asciicast v2 header. `env` is optional but most players display
        // SHELL/TERM in their info pane; we synthesize sane defaults.
        let header = CastHeader {
            version: 2,
            width: cfg.cols,
            height: cfg.rows,
            timestamp: (cfg.started_at_ms / 1000).max(0) as u64,
            env: HeaderEnv {
                shell: "/bin/sh",
                term: "xterm-256color",
            },
        };
        let header_json = serde_json::to_string(&header).context("serialize cast header")?;
        writer
            .write_all(header_json.as_bytes())
            .await
            .context("write cast header")?;
        writer.write_all(b"\n").await.context("write header newline")?;
        writer.flush().await.context("flush cast header")?;

        let (tx, rx) = mpsc::unbounded_channel::<Bytes>();
        let (fin_tx, fin_rx) = oneshot::channel::<FinalizeResult>();
        let path = cfg.file_path.clone();
        let started_at_ms = cfg.started_at_ms;
        let max_bytes = cfg.max_bytes;

        tokio::spawn(writer_loop(
            writer,
            rx,
            fin_tx,
            path,
            started_at_ms,
            max_bytes,
        ));

        Ok(Self {
            owner_handle: RecorderHandle { tx },
            finalize_rx: fin_rx,
        })
    }

    /// Clone of the producer handle. Hand one to the PTY pump.
    pub fn handle(&self) -> RecorderHandle {
        self.owner_handle.clone()
    }

    /// Drop the owner-side handle and await finalization. The writer task
    /// only completes after every other [`RecorderHandle`] clone (typically
    /// held by the PTY pump) has also been dropped.
    pub async fn wait_finalize(self) -> Result<FinalizeResult> {
        drop(self.owner_handle);
        self.finalize_rx
            .await
            .context("recorder finalize oneshot dropped before completion")
    }
}

async fn writer_loop(
    mut writer: BufWriter<File>,
    mut rx: mpsc::UnboundedReceiver<Bytes>,
    fin_tx: oneshot::Sender<FinalizeResult>,
    path: PathBuf,
    started_at_ms: i64,
    max_bytes: u64,
) {
    // Monotonic clock for per-event deltas — robust to wall-clock jumps.
    let start_instant = Instant::now();
    let mut last_seq: i64 = 0;
    let mut io_failed = false;
    let mut written: u64 = 0;
    let mut truncated = false;
    // Incremental UTF-8 decode state: trailing bytes of a sequence split by
    // the 8 KiB PTY read boundary (≤3 bytes) are carried into the next chunk
    // so the character decodes whole instead of becoming two U+FFFDs.
    let mut pending: Vec<u8> = Vec::new();

    while let Some(chunk) = rx.recv().await {
        last_seq += chunk.len() as i64;
        if io_failed {
            // Keep draining so the channel can close cleanly, but skip the
            // disk writes — we've already lost write integrity.
            continue;
        }
        // Soft single-file cap (0 = unlimited): once past it, keep draining but
        // stop writing so a runaway agent can't produce an unbounded .cast.
        if max_bytes > 0 && written >= max_bytes {
            if !truncated {
                tracing::warn!(
                    path = %path.display(),
                    max_bytes,
                    "cast recording hit size cap; truncating further output"
                );
                truncated = true;
            }
            continue;
        }
        let delta = start_instant.elapsed().as_secs_f64();
        // asciicast v2 requires `data` to be a valid UTF-8 string. Decode
        // incrementally across chunks: a multi-byte char split by the PTY
        // read boundary must not be mangled into U+FFFD on both sides —
        // the recording is permanent, unlike the live stream. Genuinely
        // invalid bytes still degrade to U+FFFD so the file stays parseable.
        let data = decode_chunk(&mut pending, &chunk);
        let event: (f64, &str, &str) = (delta, "o", &data);
        let line = match serde_json::to_string(&event) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?e, path = %path.display(), "serialize cast event");
                continue;
            }
        };
        if let Err(e) = writer.write_all(line.as_bytes()).await {
            tracing::warn!(?e, path = %path.display(), "write cast line");
            io_failed = true;
            continue;
        }
        if let Err(e) = writer.write_all(b"\n").await {
            tracing::warn!(?e, path = %path.display(), "write cast newline");
            io_failed = true;
            continue;
        }
        written += line.len() as u64 + 1;
        // asciicast files are commonly tailed live (asciinema-player can
        // stream a growing file). Without per-event flush the BufWriter
        // holds everything until shutdown — so a live recording looks
        // empty on disk until the agent exits.
        if let Err(e) = writer.flush().await {
            tracing::warn!(?e, path = %path.display(), "flush cast event");
            io_failed = true;
            continue;
        }
    }
    // Stream ended with an incomplete UTF-8 tail still pending (genuinely
    // truncated output): record it lossily rather than dropping the bytes,
    // matching what per-chunk lossy decoding would have written.
    if !pending.is_empty()
        && !io_failed
        && (max_bytes == 0 || written < max_bytes)
    {
        let delta = start_instant.elapsed().as_secs_f64();
        let data = String::from_utf8_lossy(&pending);
        let event: (f64, &str, &str) = (delta, "o", &data);
        match serde_json::to_string(&event) {
            Ok(line) => {
                // `written` is not tracked here: the cap was already checked
                // above and nothing reads the counter after the loop.
                if let Err(e) = writer.write_all(line.as_bytes()).await {
                    tracing::warn!(?e, path = %path.display(), "write cast tail");
                } else if let Err(e) = writer.write_all(b"\n").await {
                    tracing::warn!(?e, path = %path.display(), "write cast tail newline");
                }
            }
            Err(e) => tracing::warn!(?e, path = %path.display(), "serialize cast tail"),
        }
    }
    if let Err(e) = writer.flush().await {
        tracing::warn!(?e, path = %path.display(), "flush cast on close");
    }
    if let Err(e) = writer.shutdown().await {
        tracing::debug!(?e, path = %path.display(), "shutdown cast on close");
    }
    let duration_ms = start_instant.elapsed().as_millis() as i64;
    let finalized_at_ms = now_ms().max(started_at_ms);
    let _ = fin_tx.send(FinalizeResult {
        finalized_at_ms,
        duration_ms,
        last_seq,
    });
}

/// Decode one PTY chunk to a valid UTF-8 string, carrying any trailing
/// incomplete multi-byte sequence in `pending` so it can complete in a later
/// chunk. PTY reads are raw 8 KiB slices that can split a multi-byte char
/// (e.g. a 3-byte CJK glyph) across two chunks; decoding each chunk
/// independently with `from_utf8_lossy` would burn a permanent U+FFFD into
/// the recording on BOTH sides of the split. `pending` never exceeds 3 bytes
/// (UTF-8's max sequence length minus one). Genuinely invalid bytes degrade
/// to U+FFFD exactly like `from_utf8_lossy` (same `Utf8Error` semantics).
fn decode_chunk(pending: &mut Vec<u8>, chunk: &[u8]) -> String {
    let mut buf = std::mem::take(pending);
    buf.extend_from_slice(chunk);
    let mut out = String::with_capacity(buf.len());
    let mut rest = buf.as_slice();
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(e) => {
                // The prefix up to valid_up_to is valid UTF-8 by definition.
                out.push_str(&String::from_utf8_lossy(&rest[..e.valid_up_to()]));
                match e.error_len() {
                    // Genuinely invalid byte(s): one U+FFFD, resume after.
                    Some(len) => {
                        out.push('\u{FFFD}');
                        rest = &rest[e.valid_up_to() + len..];
                    }
                    // Incomplete sequence at the buffer tail: defer it to
                    // the next chunk instead of mangling it into U+FFFD.
                    None => {
                        pending.extend_from_slice(&rest[e.valid_up_to()..]);
                        break;
                    }
                }
            }
        }
    }
    out
}

#[derive(Debug, Serialize)]
struct CastHeader<'a> {
    version: u32,
    width: u16,
    height: u16,
    timestamp: u64,
    env: HeaderEnv<'a>,
}

#[derive(Debug, Serialize)]
struct HeaderEnv<'a> {
    #[serde(rename = "SHELL")]
    shell: &'a str,
    #[serde(rename = "TERM")]
    term: &'a str,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_chunk_defers_split_multibyte_char() {
        // 中 = E4 B8 AD (3 bytes). Split across two chunk boundaries:
        // no U+FFFD on either side, the char completes in the second chunk.
        let mut pending = Vec::new();
        let out1 = decode_chunk(&mut pending, &[0xE4, 0xB8]);
        assert_eq!(out1, "");
        assert_eq!(pending.len(), 2, "incomplete tail must be deferred");
        let out2 = decode_chunk(&mut pending, &[0xAD]);
        assert_eq!(out2, "中");
        assert!(pending.is_empty());
        assert!(!out1.contains('\u{FFFD}'));
        assert!(!out2.contains('\u{FFFD}'));
    }

    #[test]
    fn decode_chunk_handles_text_around_split() {
        let mut pending = Vec::new();
        let zh = "中文".as_bytes(); // 6 bytes
        // Split mid-character: chunk1 ends after 中's first byte.
        let out1 = decode_chunk(&mut pending, &zh[..1]);
        let out2 = decode_chunk(&mut pending, &zh[1..]);
        assert_eq!(format!("{out1}{out2}"), "中文");
        assert!(pending.is_empty());
    }

    #[test]
    fn decode_chunk_still_replaces_genuinely_invalid_bytes() {
        let mut pending = Vec::new();
        // Truncated sequence followed by ASCII: the incomplete E4 B8 is
        // invalid once 'A' arrives → one U+FFFD (not two), then 'A'.
        let out = decode_chunk(&mut pending, &[0xE4, 0xB8, b'A']);
        assert_eq!(out, "\u{FFFD}A");
        assert!(pending.is_empty());
        // A standalone invalid byte degrades exactly like from_utf8_lossy.
        let out = decode_chunk(&mut pending, &[0xFF]);
        assert_eq!(out, String::from_utf8_lossy(&[0xFF]));
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn recorder_writes_split_multibyte_char_without_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("split.cast");
        let rec = Recorder::start(RecorderConfig {
            agent_id: "t".into(),
            cols: 80,
            rows: 24,
            started_at_ms: 0,
            file_path: path.clone(),
            max_bytes: 0,
        })
        .await
        .unwrap();
        let h = rec.handle();
        let zh = "中".as_bytes();
        h.write_chunk(Bytes::from_static(b"before "));
        // The char straddles two PTY chunks.
        h.write_chunk(Bytes::copy_from_slice(&zh[..2]));
        h.write_chunk(Bytes::copy_from_slice(&zh[2..]));
        h.write_chunk(Bytes::from_static(b" after"));
        drop(h);
        rec.wait_finalize().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains('中'), "cast lost the char: {content}");
        assert!(
            !content.contains('\u{FFFD}'),
            "cast has permanent replacement char: {content}"
        );
    }

    #[tokio::test]
    async fn recorder_flushes_incomplete_tail_lossily_on_eof() {
        // Stream ends mid-sequence (genuinely truncated output): the bytes
        // must still be recorded (lossily), not silently dropped.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tail.cast");
        let rec = Recorder::start(RecorderConfig {
            agent_id: "t".into(),
            cols: 80,
            rows: 24,
            started_at_ms: 0,
            file_path: path.clone(),
            max_bytes: 0,
        })
        .await
        .unwrap();
        let h = rec.handle();
        h.write_chunk(Bytes::from_static(b"ok "));
        h.write_chunk(Bytes::copy_from_slice(&"中".as_bytes()[..2]));
        drop(h);
        rec.wait_finalize().await.unwrap();

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("ok "), "cast lost valid text: {content}");
        assert!(
            content.contains('\u{FFFD}'),
            "truncated tail bytes were dropped instead of lossy-recorded: {content}"
        );
    }
}
