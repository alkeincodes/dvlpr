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

/// Spawn `dvlpr server` detached from the controlling terminal.
pub fn spawn_detached_server() -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
