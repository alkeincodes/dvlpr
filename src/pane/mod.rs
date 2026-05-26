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
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl Drop for ChildKiller {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait(); // reap so we never leave a zombie
        }
    }
}

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

        let child = pair.slave.spawn_command(cmd).map_err(to_io)?;
        // Slave fd no longer needed in this process once the child holds it.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(to_io)?;
        let writer = pair.master.take_writer().map_err(to_io)?;

        let (tx, rx) = mpsc::unbounded_channel();

        // Blocking reader thread -> async channel.
        let reader_tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if reader_tx
                            .send(PaneOutput::Bytes(buf[..n].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = reader_tx.send(PaneOutput::Exited);
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

    /// Resize the PTY.
    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
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
        let (pane, mut rx) =
            PaneRuntime::spawn(&["sh".into(), "-c".into(), "printf READY".into()], ".", 80, 24)
                .expect("spawn pane");

        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while let Ok(Some(out)) =
            tokio::time::timeout_at(deadline, rx.recv()).await
        {
            match out {
                PaneOutput::Bytes(b) => collected.extend_from_slice(&b),
                PaneOutput::Exited => break,
            }
        }
        let text = String::from_utf8_lossy(&collected);
        assert!(text.contains("READY"), "got: {text:?}");
        drop(pane);
    }
}
