//! Daemon bootstrap helpers: single-instance lock and detached self-spawn.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Acquire an exclusive, non-blocking lock so only one daemon binds the socket.
/// Returns the held lock file (drop releases it). Errors if another daemon holds it.
pub fn acquire_instance_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "another dvlpr daemon already holds the instance lock",
        ));
    }
    Ok(file)
}

/// Spawn `dvlpr server <name>` detached from the controlling terminal.
pub fn spawn_detached_server(name: &str) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Hand the client's launch directory to the daemon so the session's shells
    // open where the user ran `dvlpr`, not in the daemon's inherited cwd.
    // ServerConfig::for_session reads it to seed the session cwd, and
    // PaneRuntime::spawn strips it from each child so it never reaches user shells.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.env("DVLPR_SESSION_CWD", cwd);
    }
    unsafe {
        cmd.pre_exec(|| {
            // New session so the daemon survives the parent terminal closing.
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()?;
    Ok(())
}
