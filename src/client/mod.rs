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
    read_msg, write_msg, ClientHello, ClientMsg, CommandReply, ControlCommand, Intent, ServerHello,
    ServerMsg, StatusInfo, PROTOCOL_VERSION,
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

/// Enables DECSET 1004 (focus reporting) on construction and disables it on drop,
/// so the user's host terminal sends `\x1b[I` / `\x1b[O` while the client is
/// attached and stops on detach. The bytes flow over the wire inside the existing
/// `ClientMsg::Input` and are interpreted server-side by `InputParser` —
/// FocusIn promotes this client to foreground (interaction-driven promote-most-recent),
/// FocusOut is consumed and dropped. Writes synchronously to stdout (teardown
/// must not depend on the async runtime still running).
struct FocusGuard;

impl FocusGuard {
    fn enable() -> io::Result<Self> {
        let mut out = io::stdout();
        out.write_all(b"\x1b[?1004h")?;
        out.flush()?;
        Ok(FocusGuard)
    }
}

impl Drop for FocusGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1004l");
        let _ = out.flush();
    }
}

/// Switches the terminal to its alternate screen buffer on construction and back to the
/// primary screen (restoring the user's prior content + scrollback) on drop. This is the
/// standard mechanism full-screen apps (vim, tmux, less) use to claim the whole screen;
/// terminals that overlay their own chrome on the primary screen (e.g. Warp's block UI /
/// input bar) hand the app a dedicated full-screen surface once it enters the alt screen,
/// so the server-painted frames are no longer clobbered. Writes synchronously to stdout
/// (teardown must not depend on the async runtime still running). Entered BEFORE the raw
/// and mouse guards so its restore runs LAST on teardown.
struct AltScreenGuard;

impl AltScreenGuard {
    fn enter() -> io::Result<Self> {
        let mut out = io::stdout();
        // Enter the alt screen, then clear it so the first server frame paints onto a
        // blank surface even before it arrives.
        out.write_all(b"\x1b[?1049h\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(AltScreenGuard)
    }
}

impl Drop for AltScreenGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = out.write_all(b"\x1b[?1049l");
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
            intent: Intent::Attach { cols, rows },
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

    // Guards restore terminal state on every exit path (including panic/early return).
    // Declared outermost-first so Drop runs in reverse: mouse off, focus off, raw off,
    // then leave the alt screen LAST — which restores the user's original screen +
    // scrollback, so no manual clear is needed here.
    let _alt = AltScreenGuard::enter()?;
    let _raw = RawModeGuard::enable()?;
    let _focus = FocusGuard::enable()?;
    let _mouse = MouseGuard::enable()?;
    run_loop(read_half, write_half).await
}

/// Connect to a session socket, ask for its status, return it. Does not attach.
pub async fn query_status(socket_path: &Path) -> io::Result<StatusInfo> {
    let stream = UnixStream::connect(socket_path).await?;
    let (mut read_half, mut write_half) = stream.into_split();
    write_msg(
        &mut write_half,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Status,
        },
    )
    .await?;
    read_msg::<_, StatusInfo>(&mut read_half)
        .await?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no status reply"))
}

/// Connect to a session socket and ask the daemon to shut down (best-effort).
pub async fn send_kill(socket_path: &Path) -> io::Result<()> {
    let stream = UnixStream::connect(socket_path).await?;
    let (_read_half, mut write_half) = stream.into_split();
    write_msg(
        &mut write_half,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Kill,
        },
    )
    .await
}

/// Send a single control command to a running session and return its reply.
/// Connection failure (no daemon) surfaces as the underlying `io::Error`; a
/// clean EOF before a reply means the daemon predates the control protocol.
pub async fn send_command(socket_path: &Path, cmd: ControlCommand) -> io::Result<CommandReply> {
    let stream = UnixStream::connect(socket_path).await?;
    let (mut read_half, mut write_half) = stream.into_split();
    write_msg(
        &mut write_half,
        &ClientHello {
            protocol_version: PROTOCOL_VERSION,
            intent: Intent::Command,
        },
    )
    .await?;
    write_msg(&mut write_half, &ClientMsg::Command(cmd)).await?;
    read_msg::<_, CommandReply>(&mut read_half)
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "session predates the control CLI; restart it (dvlpr stop -t <name>, then reattach)",
            )
        })
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
    // Tracks whether the loop exited because the reader task already finished
    // (so we must NOT await its JoinHandle again — that would double-poll).
    let mut reader_finished = false;

    loop {
        tokio::select! {
            _ = &mut reader => { reader_finished = true; break; }
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
                    Err(e) => {
                        reader.abort();
                        let _ = reader.await;
                        return Err(e);
                    }
                };
                if n == 0 { break; }
                if write_msg(&mut write_half, &ClientMsg::Input(in_buf[..n].to_vec())).await.is_err() {
                    break;
                }
            }
        }
    }

    if !reader_finished {
        reader.abort();
        let _ = reader.await; // wait for cancellation so MouseGuard::drop's fd-1 write can't interleave
    }
    Ok(())
}
