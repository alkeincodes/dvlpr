//! Thin client: raw-mode terminal, forward stdin bytes + SIGWINCH resizes,
//! paint server frames to stdout. Detach with the prefix `Ctrl-b d`.

use std::io;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};

use crate::protocol::{
    read_msg, write_msg, ClientHello, ClientMsg, ServerHello, ServerMsg, PROTOCOL_VERSION,
};

const PREFIX: u8 = 0x02; // Ctrl-b
const DETACH_KEY: u8 = b'd';

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
        None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "server closed")),
    }

    crossterm::terminal::enable_raw_mode()?;
    let result = run_loop(read_half, write_half).await;
    let _ = crossterm::terminal::disable_raw_mode();
    // Restore a sane screen on the way out.
    let mut out = tokio::io::stdout();
    let _ = out.write_all(b"\x1b[2J\x1b[H").await;
    let _ = out.flush().await;
    result
}

async fn run_loop(
    read_half: tokio::net::unix::OwnedReadHalf,
    mut write_half: tokio::net::unix::OwnedWriteHalf,
) -> io::Result<()> {
    // Dedicated frame-reader task: read_msg is NOT cancel-safe, so it must never
    // live inside a select! arm. It owns read_half and paints straight to stdout.
    let mut reader = tokio::spawn(async move {
        let mut read_half = read_half;
        let mut stdout = tokio::io::stdout();
        while let Ok(Some(ServerMsg::Frame { data, .. })) =
            read_msg::<_, ServerMsg>(&mut read_half).await
        {
            if stdout.write_all(&data).await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });

    let mut stdin = tokio::io::stdin();
    let mut winch = signal(SignalKind::window_change())?;
    let mut in_buf = [0u8; 4096];
    let mut prefix_armed = false;

    loop {
        tokio::select! {
            // The server closed (reader task finished): exit.
            _ = &mut reader => break,
            // Terminal resize. (signal recv is cancel-safe)
            _ = winch.recv() => {
                if let Ok((cols, rows)) = crossterm::terminal::size() {
                    if write_msg(&mut write_half, &ClientMsg::Resize { cols, rows }).await.is_err() {
                        break;
                    }
                }
            }
            // Keyboard -> input. (stdin.read is cancel-safe; the handler body below
            // runs to completion once this arm is selected, so its write_msg awaits
            // are not interrupted.)
            n = stdin.read(&mut in_buf) => {
                let n = n?;
                if n == 0 { break; }
                match forward_input(&in_buf[..n], &mut prefix_armed, &mut write_half).await {
                    Ok(Some(InputAction::Detach)) => {
                        let _ = write_msg(&mut write_half, &ClientMsg::Detach).await;
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
    }

    reader.abort();
    Ok(())
}

#[derive(PartialEq, Eq)]
enum InputAction {
    Detach,
}

/// Scan input for the `Ctrl-b d` detach sequence; forward everything else.
async fn forward_input(
    bytes: &[u8],
    prefix_armed: &mut bool,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> io::Result<Option<InputAction>> {
    let mut passthrough: Vec<u8> = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if *prefix_armed {
            *prefix_armed = false;
            if b == DETACH_KEY {
                if !passthrough.is_empty() {
                    write_msg(write_half, &ClientMsg::Input(std::mem::take(&mut passthrough))).await?;
                }
                return Ok(Some(InputAction::Detach));
            } else if b == PREFIX {
                // Ctrl-b Ctrl-b sends a literal Ctrl-b.
                passthrough.push(PREFIX);
            } else {
                // Not a recognized command: send the prefix, then this byte.
                passthrough.push(PREFIX);
                passthrough.push(b);
            }
        } else if b == PREFIX {
            *prefix_armed = true;
        } else {
            passthrough.push(b);
        }
    }
    if !passthrough.is_empty() {
        write_msg(write_half, &ClientMsg::Input(passthrough)).await?;
    }
    Ok(None)
}
