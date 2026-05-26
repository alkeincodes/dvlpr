//! Unix socket path resolution, directory setup, and stale/live detection.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SUN_PATH_MAX: usize = 100; // conservative; macOS sun_path is 104.

/// Resolve the directory that holds dvlpr sockets for this user.
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DVLPR_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("dvlpr");
    }
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("dvlpr-{uid}"))
}

/// Compute the socket path for a named session inside `dir`.
pub fn socket_path_in(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.sock"))
}

/// Default socket path for the default session.
pub fn default_socket_path() -> PathBuf {
    socket_path_in(&runtime_dir(), "default")
}

/// Create the runtime directory with `0700` perms if needed.
pub fn ensure_runtime_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Reject socket paths too long for `sockaddr_un.sun_path`.
pub fn check_sun_path_len(path: &Path) -> io::Result<()> {
    if path.as_os_str().len() > SUN_PATH_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("socket path too long ({} bytes)", path.as_os_str().len()),
        ));
    }
    Ok(())
}

/// Is a live daemon currently listening on this socket?
pub async fn is_live(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    tokio::net::UnixStream::connect(path).await.is_ok()
}

/// Set owner-only permissions on a bound socket file.
pub fn lock_down_socket(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_lives_under_the_runtime_dir() {
        let dir = std::env::temp_dir().join("dvlpr-sockettest");
        let path = socket_path_in(&dir, "default");
        assert_eq!(path, dir.join("default.sock"));
    }

    #[test]
    fn rejects_paths_that_exceed_sun_path_limit() {
        let long = PathBuf::from("/".to_string() + &"x".repeat(200));
        let err = check_sun_path_len(&long).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn stale_socket_file_is_detected_as_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        // A regular file at the socket path is not a live server.
        std::fs::write(&path, b"").unwrap();
        assert!(!is_live(&path).await);
    }

    #[tokio::test]
    async fn missing_socket_is_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.sock");
        assert!(!is_live(&path).await);
    }
}
