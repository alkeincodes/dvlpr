//! Thin client: raw-mode terminal with SGR mouse tracking, forward stdin bytes +
//! SIGWINCH resizes, paint server frames to stdout. Prefix/mouse interpretation
//! lives entirely on the server now; the client forwards raw bytes and exits when
//! the server sends `Detach` or `Closed`.

use std::io;
use std::io::Write as _;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};

use crate::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};

/// Enables raw mode on construction, restores cooked mode on drop (including on a
/// panic or early return).
struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Enables SGR mouse tracking (button-event tracking `1002` + SGR coords `1006`)
/// on construction and disables it on drop, so the user's terminal never stays in
/// mouse-reporting mode after the client exits. Writes synchronously to stdout
/// (teardown must not depend on the async runtime still running).
struct MouseGuard;

impl MouseGuard {
    fn enable() -> io::Result<Self> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?1002;1006h")?;
        out.flush()?;
        Ok(MouseGuard)
    }
}

impl Drop for MouseGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1002;1006l");
        let _ = out.flush();
    }
}

/// Attach the current terminal to the daemon at `socket_path`.
pub async fn attach(socket_path: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(socket_path).await?;
    let (mut read_half, mut write_half) = stream.into_split();

    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    write_msg(
        &mut write_half,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            cols,
            rows,
        },
    )
    .await?;

    match read_msg::<_, ServerHello>(&mut read_half).await? {
        Some(ServerHello::Ok { .. }) => {}
        Some(ServerHello::Reject { reason }) => {
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, reason));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed",
            ))
        }
    }

    // Guards restore cooked mode AND disable mouse tracking on every exit path.
    let _raw = RawModeGuard::enable()?;
    let _mouse = MouseGuard::enable()?;
    let result = run_loop(read_half, write_half).await;
    if result.is_ok() {
        let mut out = tokio::io::stdout();
        let _ = out.write_all(b"\x1b[2J\x1b[H").await;
        let _ = out.flush().await;
    }
    result
}

async fn run_loop(
    read_half: tokio::net::unix::OwnedReadHalf,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
) -> io::Result<()> {
    let mut winch = signal(SignalKind::window_change())?;

    // Frame-reader task: paints frames, exits on Detach/Closed/EOF. read_msg is
    // not cancel-safe so it must own read_half outside any select! arm.
    let mut reader = tokio::spawn(async move {
        let mut read_half = read_half;
        let mut stdout = tokio::io::stdout();
        loop {
            match read_msg::<_, ServerMsg>(&mut read_half).await {
                Ok(Some(ServerMsg::Frame { data, .. })) => {
                    if stdout.write_all(&data).await.is_err() {
                        break;
                    }
                    let _ = stdout.flush().await;
                }
                Ok(Some(ServerMsg::Detach)) | Ok(Some(ServerMsg::Closed { .. })) | Ok(None) => {
                    break
                }
                Err(_) => break,
            }
        }
    });

    let mut stdin = tokio::io::stdin();
    let mut in_buf = [0u8; 4096];

    loop {
        tokio::select! {
            _ = &mut reader => break,
            _ = winch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    if write_msg(&mut write_half, &ClientMsg::Resize { cols, rows }).await.is_err() {
                        break;
                    }
                }
            }
            n = stdin.read(&mut in_buf) => {
                let n = match n {
                    Ok(n) => n,
                    Err(e) => { reader.abort(); return Err(e); }
                };
                if n == 0 { break; }
                if write_msg(&mut write_half, &ClientMsg::Input(in_buf[..n].to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }

    reader.abort();
    Ok(())
}
