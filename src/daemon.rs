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

/// Spawn `<exe> server <name>` detached from the controlling terminal, using an
/// EXPLICIT executable path. The update-restart orchestration uses this with the
/// canonical install path captured BEFORE the binary swap — `current_exe()`
/// after a rename resolves to the deleted old inode on Linux (`…/dvlpr (deleted)`).
pub fn spawn_detached_server_at(exe: &Path, name: &str, restore: bool) -> io::Result<()> {
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
    // Make the restore handoff explicit and non-inheritable: only ever set
    // DVLPR_RESTORE when we are deliberately restoring, and otherwise strip it
    // from the child's inherited environment. Without the env_remove, a user
    // whose own environment already had DVLPR_RESTORE=1 would cause a normal
    // (restore=false) launch to wrongly restore, since Command inherits the
    // parent env by default.
    if restore {
        cmd.env("DVLPR_RESTORE", "1");
    } else {
        cmd.env_remove("DVLPR_RESTORE");
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

/// Spawn `dvlpr server <name>` detached, using this process's own executable.
pub fn spawn_detached_server(name: &str, restore: bool) -> io::Result<()> {
    spawn_detached_server_at(&std::env::current_exe()?, name, restore)
}

#[cfg(test)]
mod tests {
    /// Source-guard: the non-restore branch of `spawn_detached_server` must
    /// strip DVLPR_RESTORE from the child's inherited environment, so an
    /// inherited DVLPR_RESTORE=1 in the user's own environment can never cause
    /// a normal launch to restore.
    #[test]
    fn daemon_clears_restore_flag_when_not_restoring() {
        let src = include_str!("daemon.rs");
        assert!(
            src.contains("env_remove(\"DVLPR_RESTORE\")"),
            "spawn_detached_server must env_remove(\"DVLPR_RESTORE\") on the non-restore path"
        );
    }

    /// Source-guard: the explicit-path entry point exists and the no-arg wrapper
    /// delegates to it with `current_exe()`. Protects the update-restart respawn,
    /// which must NOT call `current_exe()` after the binary swap (Linux resolves
    /// it to the deleted old inode).
    #[test]
    fn spawn_at_takes_explicit_exe_path() {
        let src = include_str!("daemon.rs");
        assert!(
            src.contains("pub fn spawn_detached_server_at(exe: &Path"),
            "spawn_detached_server_at(exe, name, restore) must exist"
        );
        assert!(
            src.contains("spawn_detached_server_at(&std::env::current_exe()?"),
            "spawn_detached_server must delegate to spawn_detached_server_at"
        );
    }
}
