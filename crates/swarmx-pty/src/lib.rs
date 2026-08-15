//! Cross-platform PTY wrapper around `portable-pty` with an async-friendly
//! byte-stream interface.
//!
//! `portable-pty` exposes blocking `std::io::Read` / `Write`, so we run a
//! reader OS thread that pumps bytes into a `tokio::sync::mpsc` channel,
//! and a writer OS thread that pulls bytes from another channel and writes
//! them to the master. That sandwich is the *minimum* glue needed —
//! `spawn_blocking` is wrong here because both threads live as long as the
//! PTY does and would saturate the blocking pool.
//!
//! This is the Rust port of hermes-agent's `hermes_cli/pty_bridge.py`
//! (POSIX-only, byte-safe IO via raw fd reads/writes), refined with
//! golutra's `src-tauri/src/runtime/pty.rs` cross-platform polish.

use anyhow::{Context, Result};
use bytes::Bytes;
use parking_lot::Mutex;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Default I/O channel capacity (in messages, not bytes). 256 lets a fast
/// producer get a head-start without losing the natural back-pressure: when
/// full, `tx.blocking_send` parks the reader thread, which parks `read()`,
/// which parks the kernel's PTY ring buffer, which parks the child. The
/// chain is what we want — `broadcast` would `Lagged`-drop bytes, which
/// would shred ANSI state machines.
const DEFAULT_CHANNEL_CAP: usize = 256;

/// One PTY + child pair. Output flows through `output_rx`, input through
/// `input_tx`. The reader thread exits when the kernel returns EOF / EIO.
/// Drop the bridge to terminate the child — see [`PtyBridge::kill`] for the
/// process-group teardown (SIGTERM → grace → SIGKILL on the whole group).
pub struct PtyBridge {
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    input_tx: mpsc::Sender<Bytes>,
    /// Held so we can `take()` and `join()` in `Drop`, ensuring threads
    /// don't outlive the bridge if the caller forgets.
    reader_thread: Option<thread::JoinHandle<()>>,
    writer_thread: Option<thread::JoinHandle<()>>,
}

pub struct SpawnOpts<'a> {
    pub argv: &'a [String],
    pub cwd: Option<&'a Path>,
    pub env: HashMap<String, String>,
    pub cols: u16,
    pub rows: u16,
}

pub struct PtyHandles {
    pub bridge: PtyBridge,
    pub output_rx: mpsc::Receiver<Bytes>,
}

impl PtyBridge {
    pub fn spawn(opts: SpawnOpts<'_>) -> Result<PtyHandles> {
        anyhow::ensure!(!opts.argv.is_empty(), "argv must not be empty");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: opts.rows.max(1),
                cols: opts.cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty failed")?;

        let mut cmd = CommandBuilder::new(&opts.argv[0]);
        // Start the child from an EMPTY environment. portable-pty's
        // `CommandBuilder::new` seeds itself from the parent's *full*
        // environment (`std::env::vars_os`), so without this clear every
        // spawned worker would inherit the server's entire env — including
        // ad-hoc shell secrets (AWS_*, GITHUB_TOKEN, DB creds, raw API keys).
        // The caller passes an explicit allowlist via `opts.env`; clearing
        // here makes that allowlist (plus the TERM default below) the ONLY
        // thing the child sees.
        cmd.env_clear();
        for a in &opts.argv[1..] {
            cmd.arg(a);
        }
        if let Some(cwd) = opts.cwd {
            cmd.cwd(cwd);
        }
        // PTY-hosted CLIs probe TERM (e.g. `tput cols`); back-fill a safe
        // default the same way hermes' pty_bridge.py:108-112 does.
        if !opts.env.contains_key("TERM") {
            cmd.env("TERM", "xterm-256color");
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .context("spawn_command failed")?;

        // We're done with the slave side once the child holds it — closing
        // here lets the kernel deliver EOF on master.read() after the child
        // exits, which is how the reader thread knows to terminate.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .context("try_clone_reader failed")?;
        let writer = pair
            .master
            .take_writer()
            .context("take_writer failed")?;

        let master = Arc::new(Mutex::new(pair.master));
        let child = Arc::new(Mutex::new(child));

        let (output_tx, output_rx) = mpsc::channel::<Bytes>(DEFAULT_CHANNEL_CAP);
        let (input_tx, input_rx) = mpsc::channel::<Bytes>(DEFAULT_CHANNEL_CAP);

        let reader_thread =
            spawn_reader(reader, output_tx).context("spawn pty reader thread")?;
        let writer_thread =
            spawn_writer(writer, input_rx).context("spawn pty writer thread")?;

        Ok(PtyHandles {
            bridge: PtyBridge {
                master,
                child,
                input_tx,
                reader_thread: Some(reader_thread),
                writer_thread: Some(writer_thread),
            },
            output_rx,
        })
    }

    pub fn input_sender(&self) -> mpsc::Sender<Bytes> {
        self.input_tx.clone()
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        // Lock the master only briefly; never call resize from the reader
        // thread (which holds the read half of the PTY) to avoid an ioctl
        // race during high-volume output.
        let guard = self.master.lock();
        guard
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("PTY resize failed")
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.lock().process_id()
    }

    /// Non-blocking check for child liveness.
    pub fn is_alive(&self) -> bool {
        match self.child.lock().try_wait() {
            Ok(None) => true,
            _ => false,
        }
    }

    /// Non-blocking: if the child has already exited, return its exit code,
    /// else `None`. Pairs with [`is_alive`] so the server can synthesize a
    /// `ShimExit` when the shim died without emitting its OSC exit marker
    /// (SIGKILL / crash / OOM), instead of leaving the agent forever "alive".
    pub fn try_exit_code(&self) -> Option<i32> {
        match self.child.lock().try_wait() {
            Ok(Some(status)) => Some(status.exit_code() as i32),
            _ => None,
        }
    }

    /// Best-effort terminate. Used by `Drop`; safe to call multiple times.
    ///
    /// `portable_pty::Child::kill` SIGKILLs only the **direct** child — the
    /// `swarmx-shim` process. But the real CLI (claude/codex) is the shim's
    /// child, i.e. a *grandchild* of the server, and anything that CLI spawns
    /// is a great-grandchild. SIGKILLing just the shim leaves the real CLI
    /// reparented to init, still running and still burning API tokens.
    ///
    /// `openpty` puts the shim in its own session (it's the PTY's controlling
    /// process, so `setsid`'d by `portable-pty` at spawn), which means the
    /// shim, the real CLI, and their same-group descendants all share the
    /// shim's process group. So we signal the **group**, not the pid:
    /// `SIGTERM` (let the CLI flush/commit) → short grace → `SIGKILL` the
    /// group → reap the direct child. A safety guard refuses to signal our
    /// own process group (which would kill swarmx-server itself) in the
    /// unlikely event the child was never isolated into its own session.
    pub fn kill(&self) {
        #[cfg(unix)]
        {
            let pid = self.child.lock().process_id().map(|p| p as libc::pid_t);
            if let Some(pid) = pid {
                // SAFETY: getpgid/killpg/kill are async-signal-safe libc
                // calls; we only pass pids we own and guard against our own
                // group below.
                unsafe {
                    let pgid = libc::getpgid(pid);
                    let own_pgid = libc::getpgid(0);
                    // portable-pty `setsid`'s the child at spawn, so the shim
                    // (and the real CLI it forks) share a process group whose
                    // id equals the shim pid. getpgid() can still return -1 if
                    // the pid is racing teardown/reparenting — in that case fall
                    // back to the shim pid itself as the group id rather than
                    // giving up and SIGKILLing only the shim, which would orphan
                    // the grandchild CLI (it stays alive, reparented to init,
                    // still burning API tokens). killpg with a stale/own pid is
                    // harmless (ESRCH); the own-group guard below stays so we
                    // never signal swarmx-server's own group.
                    let group = if pgid > 0 { pgid } else { pid };
                    if group > 0 && group != own_pgid {
                        // Graceful: ask the whole group to terminate.
                        libc::killpg(group, libc::SIGTERM);
                        // Brief grace (~1s) for the CLI to flush. Completion is
                        // judged by the *process group* going empty — NOT by the
                        // shim (direct child) exiting. The shim installs no
                        // SIGTERM handler and dies instantly, so a shim-exit
                        // check would skip the SIGKILL even when the real CLI
                        // trapped SIGTERM and is still running inside the group
                        // — orphaning it to keep burning API tokens.
                        // killpg(group, 0) probes without signalling: ESRCH ==
                        // group is empty. Any other outcome (0, EPERM) means
                        // "still alive" — the safe direction. We also reap the
                        // shim each pass: a zombie still counts as a group
                        // member and would otherwise pin the group until the
                        // final wait() below. The child lock is never held
                        // across the sleep.
                        let mut group_empty = false;
                        for _ in 0..20 {
                            let _ = self.child.lock().try_wait();
                            if libc::killpg(group, 0) != 0
                                && std::io::Error::last_os_error().raw_os_error()
                                    == Some(libc::ESRCH)
                            {
                                group_empty = true;
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        if !group_empty {
                            libc::killpg(group, libc::SIGKILL);
                        }
                    } else {
                        // group == own_pgid: child was never isolated into its
                        // own session; killing the group would take down the
                        // server itself. Last-resort pid-only SIGKILL.
                        warn!(pid, pgid, own_pgid, "kill: child shares our process \
                            group; falling back to pid-only SIGKILL");
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
            }
        }
        // Windows: no process groups/signals — portable-pty's Child::kill hits
        // only the direct child (the shim), orphaning the real CLI + its node
        // descendants. taskkill /T kills the whole tree by pid (/F forces it).
        #[cfg(windows)]
        {
            if let Some(pid) = self.child.lock().process_id() {
                let _ = std::process::Command::new("taskkill")
                    .args(["/F", "/T", "/PID", &pid.to_string()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
        // Reap the direct child (on unix the group was already signalled; on
        // Windows taskkill already felled the tree — this collects exit status).
        let mut child = self.child.lock();
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for PtyBridge {
    fn drop(&mut self) {
        self.kill();
        // Joining the reader/writer threads here would deadlock if either
        // is blocked in a syscall on an fd that's already gone — they exit
        // on their own once the PTY tears down. Detach.
        let _ = self.reader_thread.take();
        let _ = self.writer_thread.take();
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Bytes>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("swarmx-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => {
                        debug!("pty reader: EOF");
                        break;
                    }
                    Ok(n) => {
                        let chunk = Bytes::copy_from_slice(&buf[..n]);
                        if tx.blocking_send(chunk).is_err() {
                            debug!("pty reader: output channel closed");
                            break;
                        }
                    }
                    Err(err) => {
                        // EIO on the master fd after the slave side closes is
                        // the normal exit path on Linux — the child exited,
                        // nothing is wrong. Anything else is a real error.
                        #[cfg(unix)]
                        let is_exit_eio = err.raw_os_error() == Some(libc::EIO);
                        #[cfg(not(unix))]
                        let is_exit_eio = false;
                        if is_exit_eio {
                            debug!(?err, "pty reader: EIO (child exited)");
                        } else {
                            warn!(?err, "pty reader: read error");
                        }
                        break;
                    }
                }
            }
        })
}

fn spawn_writer(
    mut writer: Box<dyn Write + Send>,
    mut rx: mpsc::Receiver<Bytes>,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("swarmx-pty-writer".into())
        .spawn(move || {
            while let Some(chunk) = rx.blocking_recv() {
                if writer.write_all(&chunk).is_err() {
                    debug!("pty writer: write error, exiting");
                    break;
                }
                // PTY masters don't strictly need an explicit flush (writes
                // go straight to the kernel ring), but flushing is cheap
                // and guarantees a write actually reached the child if the
                // underlying impl wraps in a BufWriter.
                let _ = writer.flush();
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    #[cfg(unix)]
    fn cat_echoes_what_we_write() {
        // Absolute path: the child env is cleared (no PATH to resolve a bare
        // `cat`), mirroring how production passes an absolute shim path.
        let handles = PtyBridge::spawn(SpawnOpts {
            argv: &["/bin/cat".into()],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn cat");

        let PtyHandles { bridge, mut output_rx } = handles;
        let input = bridge.input_sender();

        block_on(async move {
            input
                .send(Bytes::from_static(b"hello swarmx\n"))
                .await
                .unwrap();

            // Read until we see what we wrote (cat echoes lines back).
            let mut got = Vec::new();
            for _ in 0..50 {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    output_rx.recv(),
                )
                .await
                {
                    Ok(Some(chunk)) => got.extend_from_slice(&chunk),
                    _ => break,
                }
                if got.windows(b"hello swarmx".len())
                    .any(|w| w == b"hello swarmx")
                {
                    break;
                }
            }
            assert!(
                got.windows(b"hello swarmx".len())
                    .any(|w| w == b"hello swarmx"),
                "did not see echoed bytes; got = {:?}",
                String::from_utf8_lossy(&got)
            );

            // Resize should not error.
            bridge.resize(120, 30).unwrap();
        });
    }

    #[test]
    #[cfg(unix)]
    fn env_is_isolated_from_parent() {
        // A secret in the *parent* env must NOT reach the child. portable-pty
        // seeds CommandBuilder from std::env::vars_os(), so without env_clear()
        // this canary would leak. Only opts.env (+ TERM) should survive.
        std::env::set_var("SWARMX_LEAK_CANARY", "leaked");
        let mut env = HashMap::new();
        env.insert("FOO".to_string(), "bar".to_string());
        let handles = PtyBridge::spawn(SpawnOpts {
            argv: &[
                "/bin/sh".into(),
                "-c".into(),
                "printf 'CANARY=[%s] FOO=[%s]' \"$SWARMX_LEAK_CANARY\" \"$FOO\"".into(),
            ],
            cwd: None,
            env,
            cols: 80,
            rows: 24,
        })
        .expect("spawn sh");

        let PtyHandles { bridge: _bridge, mut output_rx } = handles;
        block_on(async move {
            let mut got = Vec::new();
            for _ in 0..50 {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    output_rx.recv(),
                )
                .await
                {
                    Ok(Some(chunk)) => got.extend_from_slice(&chunk),
                    _ => break,
                }
                if got.windows(b"FOO=[bar]".len()).any(|w| w == b"FOO=[bar]") {
                    break;
                }
            }
            let out = String::from_utf8_lossy(&got);
            assert!(out.contains("FOO=[bar]"), "allowlisted var missing; got = {out:?}");
            assert!(
                out.contains("CANARY=[]"),
                "parent secret leaked into child env; got = {out:?}"
            );
        });
    }

    #[test]
    fn spawn_rejects_empty_argv() {
        let result = PtyBridge::spawn(SpawnOpts {
            argv: &[],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        });
        let Err(err) = result else {
            panic!("expected empty-argv to be rejected, got Ok(_)")
        };
        assert!(err.to_string().contains("argv must not be empty"));
    }

    #[test]
    #[cfg(unix)]
    fn kill_sigkills_group_survivor_after_shim_death() {
        // Regression: the shim (direct child) installs no SIGTERM handler and
        // dies instantly on the group SIGTERM, but the real CLI may trap it.
        // kill() must judge grace completion by the *process group* going
        // empty — not by the shim exiting — and SIGKILL the survivor.
        //
        // sh is the direct child (default SIGTERM disposition → dies). The
        // backgrounded subshell traps/ignores SIGTERM; a non-interactive sh
        // has no job control, so it stays in sh's process group. SIGHUP must
        // be ignored too: sh is the session leader, and when it dies the
        // kernel SIGHUPs the foreground process group — without that the
        // survivor would die of SIGHUP and the test could not tell a
        // missing SIGKILL apart from a delivered one.
        let handles = PtyBridge::spawn(SpawnOpts {
            argv: &[
                "/bin/sh".into(),
                "-c".into(),
                "(trap '' TERM HUP; echo SURVIVOR_READY; sleep 300) & wait".into(),
            ],
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
        })
        .expect("spawn sh");
        let PtyHandles { bridge, mut output_rx } = handles;

        block_on(async move {
            // SURVIVOR_READY prints only after the trap is installed, so the
            // survivor is guaranteed SIGTERM-immune before we kill.
            let mut got = Vec::new();
            for _ in 0..50 {
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    output_rx.recv(),
                )
                .await
                {
                    Ok(Some(chunk)) => got.extend_from_slice(&chunk),
                    _ => break,
                }
                if got.windows(b"SURVIVOR_READY".len()).any(|w| w == b"SURVIVOR_READY") {
                    break;
                }
            }
            assert!(
                got.windows(b"SURVIVOR_READY".len()).any(|w| w == b"SURVIVOR_READY"),
                "survivor never became ready; got = {:?}",
                String::from_utf8_lossy(&got)
            );
        });

        // portable-pty setsid's the child, so the group id is the sh pid;
        // resolve via getpgid anyway in case that ever changes.
        let pid = bridge.pid().expect("sh pid") as libc::pid_t;
        let pgid = unsafe { libc::getpgid(pid) };
        assert!(pgid > 0, "getpgid failed for child {pid}");

        bridge.kill();

        // The SIGKILLed survivor is reaped asynchronously by init after the
        // shim dies, so poll briefly for the group to drain instead of
        // asserting on the first probe (a zombie still counts as a member).
        // With the old shim-exit check this never drains within the timeout:
        // no SIGKILL is sent and the survivor sleeps on for 300s.
        let mut group_empty = false;
        for _ in 0..60 {
            let probe = unsafe { libc::killpg(pgid, 0) };
            if probe != 0
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                group_empty = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            group_empty,
            "process group {pgid} still alive after kill(): SIGTERM-trapping survivor was orphaned"
        );
    }
}
