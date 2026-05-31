//! PaneRuntime: owns one PTY + child process. A blocking reader thread forwards
//! output bytes to an async channel; input and resize go straight to the PTY.

use std::io;
use std::io::Read;
use std::io::Write;
use std::sync::Mutex;

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

/// Output produced by the pane's child process, or a one-time exit signal.
#[derive(Debug)]
pub enum PaneOutput {
    Bytes(Vec<u8>),
    Exited,
}

/// Holds the child handle so dropping the pane kills AND reaps the process.
///
/// Phase 1 uses a single, immediate `kill()` rather than the SPEC's full
/// SIGHUP→SIGTERM→SIGKILL escalation (graceful escalation is deferred to a later
/// phase). It DOES reap: `wait()` after `kill()` prevents a zombie if the daemon
/// ever outlives the child. The PTY reader thread separately signals
/// `PaneOutput::Exited` when the master EOFs, which is how the server learns the
/// pane ended; this `Drop` is the teardown path when the pane is closed from above.
struct ChildKiller {
    child: Mutex<Box<dyn Child + Send>>,
}

impl Drop for ChildKiller {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait(); // reap so we never leave a zombie
        }
    }
}

/// Owns one PTY-backed child process.
///
/// CAUTION: dropping a `PaneRuntime` runs `ChildKiller::Drop`, which calls the
/// blocking `wait()` syscall. Do not drop a `PaneRuntime` on a hot tokio worker
/// where a multi-second `SIGKILL` delivery would stall the runtime. In Phase 1 the
/// only drop happens at daemon teardown, which is acceptable. Use
/// `PaneRuntime::close(self)` to tear a pane down from async code: it moves this
/// blocking drop into `spawn_blocking` (and so requires an active Tokio runtime
/// context, which `run()` and `#[tokio::test]` both provide). Dropping a
/// `PaneRuntime` directly still runs the blocking teardown inline, so only do
/// that off the runtime (e.g. at process exit).
pub struct PaneRuntime {
    master: Box<dyn MasterPty + Send>,
    writer: Mutex<Box<dyn Write + Send>>,
    _child_killer: ChildKiller,
}

impl PaneRuntime {
    /// Spawn `command` (argv) under a new PTY of the given size.
    /// Returns the runtime plus a receiver of the child's output.
    pub fn spawn(
        command: &[String],
        cwd: &str,
        cols: u16,
        rows: u16,
        session_name: &str,
    ) -> io::Result<(PaneRuntime, mpsc::UnboundedReceiver<PaneOutput>)> {
        let argv = if command.is_empty() {
            default_shell()
        } else {
            command.to_vec()
        };

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;

        let mut cmd = CommandBuilder::new(&argv[0]);
        for arg in &argv[1..] {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        // Advertise color support to the child. The daemon may have been launched with a
        // bare or color-less TERM (e.g. detached at boot), so set a known color-capable
        // TERM and COLORTERM=truecolor rather than inheriting whatever it happens to have.
        // Our renderer preserves the cell styling these produce and re-emits it as SGR.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        // Mark this child as living inside a dvlpr session, named for the session.
        // The CLI reads $DVLPR at startup to refuse opening a nested session.
        cmd.env("DVLPR", session_name);
        cmd.env_remove("DVLPR_SESSION_CWD");
        // The daemon may carry DVLPR_SESSION_CWD (the client's launch dir, used to
        // seed the session cwd). It is an internal handoff, not meant for user
        // shells, so strip it from every child's environment.
        cmd.env_remove("DVLPR_SESSION_CWD");

        let child = pair.slave.spawn_command(cmd).map_err(to_io)?;
        // Slave fd no longer needed in this process once the child holds it.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(to_io)?;
        let writer = pair.master.take_writer().map_err(to_io)?;

        let (tx, rx) = mpsc::unbounded_channel();

        // Blocking reader thread -> async channel.
        // No JoinHandle is kept: the thread exits when the master fd closes
        // (read returns Ok(0)/Err) or when the receiver is dropped (send fails).
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if tx.send(PaneOutput::Bytes(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(PaneOutput::Exited);
        });

        let child_killer = ChildKiller {
            child: Mutex::new(child),
        };

        Ok((
            PaneRuntime {
                master: pair.master,
                writer: Mutex::new(writer),
                _child_killer: child_killer,
            },
            rx,
        ))
    }

    /// Write user input to the PTY.
    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// The pid of the PTY's foreground process group (`tcgetpgrp` on the master),
    /// or `None` when unavailable. This is what tmux reads for automatic-rename.
    pub fn foreground_pid(&self) -> Option<i32> {
        self.master.process_group_leader()
    }

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        // Clamp to at least 1x1: a 0-row/0-col TIOCSWINSZ makes ncurses/vim/less
        // divide by the terminal size on SIGWINCH and crash.
        let cols = cols.max(1);
        let rows = rows.max(1);
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    /// Tear the pane down off the async runtime. `ChildKiller::Drop` performs a
    /// blocking `kill()`+`wait()`, which must not run on a tokio worker, so move
    /// the runtime into `spawn_blocking` and let it drop there.
    pub fn close(self) {
        tokio::task::spawn_blocking(move || drop(self));
    }
}

fn default_shell() -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    vec![shell]
}

fn to_io<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn pane_runs_command_and_streams_output() {
        let (pane, mut rx) = PaneRuntime::spawn(
            &["sh".into(), "-c".into(), "printf READY".into()],
            ".",
            80,
            24,
            "test",
        )
        .expect("spawn pane");

        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(out)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match out {
                PaneOutput::Bytes(b) => collected.extend_from_slice(&b),
                PaneOutput::Exited => break,
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("READY"), "got: {text:?}");
        drop(pane);
    }

    #[tokio::test]
    async fn foreground_pid_is_some_for_a_live_pane() {
        let (pane, _rx) = PaneRuntime::spawn(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            ".",
            80,
            24,
            "test",
        )
        .expect("spawn pane");
        // The PTY has a foreground process group as soon as the child is running.
        assert!(pane.foreground_pid().is_some());
    }

    #[tokio::test]
    async fn close_tears_down_without_blocking_the_runtime() {
        let (pane, _rx) = PaneRuntime::spawn(
            &["sh".into(), "-c".into(), "sleep 30".into()],
            ".",
            80,
            24,
            "test",
        )
        .expect("spawn pane");
        // close() must move the blocking kill()+wait() off the async runtime and
        // return promptly (it does not block on the child).
        pane.close();
        // Give the spawned blocking task a moment; the test completing without a
        // hang/panic is the assertion.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn spawn_sets_dvlpr_env_to_session_name() {
        let (pane, mut rx) = PaneRuntime::spawn(
            &["sh".into(), "-c".into(), "printf %s \"$DVLPR\"".into()],
            ".",
            80,
            24,
            "mysess",
        )
        .expect("spawn pane");

        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(out)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match out {
                PaneOutput::Bytes(b) => collected.extend_from_slice(&b),
                PaneOutput::Exited => break,
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("mysess"), "got: {text:?}");
        drop(pane);
    }

    #[tokio::test]
    async fn spawn_uses_given_cwd() {
        // The child must start in the directory we pass. Use `pwd -P` (physical
        // path) and canonicalize the expected dir so the macOS /tmp ->
        // /private/tmp symlink does not cause a spurious mismatch.
        let want = std::env::temp_dir()
            .canonicalize()
            .expect("canonicalize tmp");
        let (pane, mut rx) = PaneRuntime::spawn(
            &["sh".into(), "-c".into(), "pwd -P".into()],
            want.to_str().unwrap(),
            80,
            24,
            "test",
        )
        .expect("spawn pane");

        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(out)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            match out {
                PaneOutput::Bytes(b) => collected.extend_from_slice(&b),
                PaneOutput::Exited => break,
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert_eq!(text.trim(), want.to_string_lossy());
        drop(pane);
    }
}
