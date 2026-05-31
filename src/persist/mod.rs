//! On-disk session snapshot for crash resurrection. Pure: schema + atomic IO +
//! restore planning. No async, no `Session` dependency.
//! See docs/superpowers/specs/2026-05-31-session-resurrection-design.md.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub schema_version: u32,
    pub session_name: String,
    pub sidebar_visible: bool,
    pub sidebar_width: u16,
    pub active_window: usize,
    pub windows: Vec<WindowSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSnapshot {
    pub name: String,
    pub name_pinned: bool,
    pub zoomed: bool,
    /// Index of the focused leaf in tree order (first-before-second DFS), the
    /// same order as `layout::all_panes`.
    pub focused_leaf: usize,
    pub layout: NodeSnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeSnapshot {
    Leaf(PaneSnapshot),
    Split {
        dir: SplitDirSnap,
        ratio: f32,
        first: Box<NodeSnapshot>,
        second: Box<NodeSnapshot>,
    },
}

/// On-disk mirror of `layout::SplitDir`, kept snapshot-local so the runtime enum
/// needs no serde derives and the schema can evolve independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitDirSnap {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PaneSnapshot {
    pub cwd: String,
    pub agent: AgentResume,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AgentResume {
    None,
    /// `transcript` is the exact `.jsonl` path captured at discovery (used for the
    /// existence preflight); `session_id` is what the resume command needs.
    Claude { session_id: String, transcript: String },
    Codex { session_id: String, transcript: String },
}

use std::io::{self, Write};

/// `$XDG_STATE_HOME/dvlpr`, else `~/.local/state/dvlpr`. NOT the volatile runtime dir.
pub fn state_dir() -> PathBuf {
    if let Some(x) = std::env::var_os("XDG_STATE_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("dvlpr");
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/state/dvlpr")
}

pub fn snapshot_path(session: &str) -> PathBuf {
    state_dir().join(format!("{session}.json"))
}

/// Atomic write: ensure dir (0700), write a sibling temp file created 0600, fsync the
/// file, rename, then fsync the parent directory so the rename is durable. A power cut
/// mid-write leaves at most the temp file, never a torn target.
pub fn write_atomic(path: &Path, snap: &SessionSnapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        set_mode(dir, 0o700);
    }
    let json = serde_json::to_vec_pretty(snap)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        // Create with 0600 from the start (no chmod-after race window).
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // fsync the parent directory so the rename survives a power loss.
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Load + validate. `None` on absent file, parse error, or schema mismatch — i.e.
/// "nothing to restore". Never panics.
pub fn load(path: &Path) -> Option<SessionSnapshot> {
    let bytes = std::fs::read(path).ok()?;
    let snap: SessionSnapshot = serde_json::from_slice(&bytes).ok()?;
    (snap.schema_version == SCHEMA_VERSION).then_some(snap)
}

/// Best-effort unlink (graceful shutdown). Safe to call when the file is absent.
pub fn delete(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Move `<name>.json` to `<name>.json.bak` (one-slot safety net on decline).
pub fn archive(path: &Path) {
    let _ = std::fs::rename(path, path.with_extension("json.bak"));
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionSnapshot {
        SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            session_name: "work".into(),
            sidebar_visible: true,
            sidebar_width: 26,
            active_window: 1,
            windows: vec![WindowSnapshot {
                name: "edit".into(),
                name_pinned: true,
                zoomed: false,
                focused_leaf: 1,
                layout: NodeSnapshot::Split {
                    dir: SplitDirSnap::Vertical,
                    ratio: 0.5,
                    first: Box::new(NodeSnapshot::Leaf(PaneSnapshot {
                        cwd: "/a".into(),
                        agent: AgentResume::None,
                    })),
                    second: Box::new(NodeSnapshot::Leaf(PaneSnapshot {
                        cwd: "/b".into(),
                        agent: AgentResume::Claude {
                            session_id: "11111111-2222-3333-4444-555555555555".into(),
                            transcript: "/home/u/.claude/projects/x/11111111-2222-3333-4444-555555555555.jsonl".into(),
                        },
                    })),
                },
            }],
        }
    }

    #[test]
    fn snapshot_round_trips_through_json() {
        let snap = sample();
        let json = serde_json::to_string(&snap).unwrap();
        let back: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, back);
    }

    #[test]
    fn snapshot_path_lives_under_state_dir() {
        let p = snapshot_path("work");
        assert!(p.ends_with("dvlpr/work.json"));
    }

    #[test]
    fn write_then_load_is_identity_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.json");
        let snap = sample();
        write_atomic(&path, &snap).unwrap();
        assert_eq!(load(&path), Some(snap));
        // No leftover temp files in the dir.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    }

    #[test]
    fn load_returns_none_on_missing_corrupt_and_wrong_version() {
        let dir = tempfile::tempdir().unwrap();
        // Missing.
        assert_eq!(load(&dir.path().join("nope.json")), None);
        // Corrupt JSON.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"{not json").unwrap();
        assert_eq!(load(&bad), None);
        // Wrong schema version.
        let mut snap = sample();
        snap.schema_version = 999;
        let wrong = dir.path().join("wrong.json");
        std::fs::write(&wrong, serde_json::to_string(&snap).unwrap()).unwrap();
        assert_eq!(load(&wrong), None);
    }

    #[test]
    fn delete_removes_and_archive_renames_to_bak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.json");
        write_atomic(&path, &sample()).unwrap();
        archive(&path);
        assert!(!path.exists());
        assert!(path.with_extension("json.bak").exists());
        // delete is a no-op-safe unlink.
        write_atomic(&path, &sample()).unwrap();
        delete(&path);
        assert!(!path.exists());
        delete(&path); // second call must not panic
    }
}
