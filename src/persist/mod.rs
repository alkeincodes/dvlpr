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
/// file, then rename it onto the target. The rename is atomic, so a reader (and a
/// power cut) never observes a torn or half-written target — only the old file or the
/// fully-written new one. Any failure during the temp write or the rename removes the
/// temp file before returning, so no stray `.tmp.<pid>` is left behind.
///
/// The follow-up parent-directory fsync is BEST-EFFORT: we do not fail the write if the
/// directory can't be opened/synced (failing here would be strictly worse — the data is
/// already renamed into place). The cost is that a power loss in the small window after
/// the rename but before the dir entry reaches stable storage may lose only the most
/// recent snapshot. That is acceptable: snapshots are rewritten ~once per second, so at
/// most ~1s of state is at risk, and the target is never left torn.
pub fn write_atomic(path: &Path, snap: &SessionSnapshot) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        set_mode(dir, 0o700);
    }
    let json = serde_json::to_vec_pretty(snap)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    // Create (0600 from the start, no chmod-after race window), write, fsync. Capture
    // any error so we can remove the temp file before returning — a `?` here would leak it.
    let write = (|| -> io::Result<()> {
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
        Ok(())
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // Best-effort parent-directory fsync (see doc comment): never fail the write on it.
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

/// Strict canonical UUID check: 8-4-4-4-12 hex with hyphens. Both Claude and Codex
/// session ids are canonical UUIDs. Anything else is rejected (never interpolated).
pub fn is_canonical_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != groups.len() {
        return false;
    }
    parts
        .iter()
        .zip(groups)
        .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// One pane's resolved restore outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct PaneRestore {
    pub cwd: String,
    pub cwd_exists: bool,
    pub agent: AgentResume,
    pub downgraded: Option<&'static str>,
}

/// Tree-ordered plan: one `PaneRestore` per leaf, in `all_panes` DFS order, plus
/// per-window structure carried implicitly by the caller re-walking the snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct RestorePlan {
    pub panes: Vec<PaneRestore>,
    pub window_count: usize,
}

impl RestorePlan {
    pub fn summary(&self) -> String {
        let downgrades = self.panes.iter().filter(|p| p.downgraded.is_some()).count();
        let w = self.window_count;
        let p = self.panes.len();
        let base = format!(
            "{w} window{}, {p} pane{}",
            if w == 1 { "" } else { "s" },
            if p == 1 { "" } else { "s" }
        );
        if downgrades == 0 {
            base
        } else {
            format!("{base} ({downgrades} downgraded)")
        }
    }
}

/// Pure: decide resume-vs-downgrade for each leaf by checking the persisted
/// transcript path and cwd existence. Shared by the CLI (summary) and the daemon
/// (spawn), so the message and behavior use identical logic.
pub fn plan_restore(snap: &SessionSnapshot) -> RestorePlan {
    let mut panes = Vec::new();
    for w in &snap.windows {
        collect_panes(&w.layout, &mut panes);
    }
    RestorePlan { panes, window_count: snap.windows.len() }
}

fn collect_panes(node: &NodeSnapshot, out: &mut Vec<PaneRestore>) {
    match node {
        NodeSnapshot::Leaf(p) => out.push(resolve_pane(p)),
        NodeSnapshot::Split { first, second, .. } => {
            collect_panes(first, out);
            collect_panes(second, out);
        }
    }
}

fn resolve_pane(p: &PaneSnapshot) -> PaneRestore {
    let cwd_exists = Path::new(&p.cwd).is_dir();
    // Reason strings are checked in priority order; cwd loss downgrades any pane.
    let (agent, downgraded) = match &p.agent {
        AgentResume::None => {
            // A shell pane whose cwd vanished will silently relocate to $HOME on
            // restore; flag it so the summary counts it as a downgrade.
            (AgentResume::None, (!cwd_exists).then_some("cwd missing"))
        }
        ag @ (AgentResume::Claude { session_id, transcript }
        | AgentResume::Codex { session_id, transcript }) => {
            if !is_canonical_uuid(session_id) {
                (AgentResume::None, Some("invalid session id"))
            } else if !Path::new(transcript).exists() {
                (AgentResume::None, Some("transcript missing"))
            } else if !cwd_exists {
                (AgentResume::None, Some("cwd missing"))
            } else {
                (ag.clone(), None)
            }
        }
    };
    PaneRestore { cwd: p.cwd.clone(), cwd_exists, agent, downgraded }
}

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

    #[test]
    fn is_canonical_uuid_accepts_only_canonical_form() {
        assert!(is_canonical_uuid("019d30c1-5e55-74f1-84bc-e8a8c3b3024c"));
        assert!(is_canonical_uuid("11111111-2222-3333-4444-555555555555"));
        assert!(!is_canonical_uuid("------------------------------------"));
        assert!(!is_canonical_uuid("1111111122223333444455555555555")); // no hyphens
        assert!(!is_canonical_uuid("zzzzzzzz-2222-3333-4444-555555555555"));
        assert!(!is_canonical_uuid("11111111-2222-3333-4444-555555555555; rm -rf /"));
    }

    #[test]
    fn plan_restore_resumes_present_transcript_and_downgrades_missing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("present.jsonl");
        std::fs::write(&present, b"x").unwrap();
        let cwd_ok = dir.path().to_string_lossy().to_string();

        let snap = SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            session_name: "w".into(),
            sidebar_visible: true,
            sidebar_width: 26,
            active_window: 0,
            windows: vec![WindowSnapshot {
                name: "w".into(),
                name_pinned: false,
                zoomed: false,
                focused_leaf: 0,
                layout: NodeSnapshot::Split {
                    dir: SplitDirSnap::Vertical,
                    ratio: 0.5,
                    // Pane A: present transcript + existing cwd → resume.
                    first: Box::new(NodeSnapshot::Leaf(PaneSnapshot {
                        cwd: cwd_ok.clone(),
                        agent: AgentResume::Claude {
                            session_id: "11111111-2222-3333-4444-555555555555".into(),
                            transcript: present.to_string_lossy().to_string(),
                        },
                    })),
                    // Pane B: missing transcript → downgrade to shell.
                    second: Box::new(NodeSnapshot::Leaf(PaneSnapshot {
                        cwd: cwd_ok.clone(),
                        agent: AgentResume::Codex {
                            session_id: "99999999-2222-3333-4444-555555555555".into(),
                            transcript: dir.path().join("gone.jsonl").to_string_lossy().to_string(),
                        },
                    })),
                },
            }],
        };

        let plan = plan_restore(&snap);
        assert_eq!(plan.panes.len(), 2);
        assert!(matches!(plan.panes[0].agent, AgentResume::Claude { .. }));
        assert_eq!(plan.panes[0].downgraded, None);
        assert!(matches!(plan.panes[1].agent, AgentResume::None));
        assert_eq!(plan.panes[1].downgraded, Some("transcript missing"));
        assert!(plan.summary().contains("1 window"));
        assert!(plan.summary().contains("2 panes"));
        assert!(plan.summary().contains("downgraded"));
    }

    #[test]
    fn write_atomic_cleans_temp_and_preserves_target_on_rename_failure() {
        let dir = tempfile::tempdir().unwrap();
        // Make the target path a NON-EMPTY directory so rename(file -> dir) fails.
        let target = dir.path().join("work.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inner"), b"keep").unwrap();
        let err = write_atomic(&target, &sample());
        assert!(err.is_err(), "rename onto a non-empty dir must fail");
        // Target dir still intact.
        assert!(target.is_dir());
        assert!(target.join("inner").exists());
        // No leftover temp files.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp must be cleaned up on rename failure: {leftovers:?}");
    }

    #[test]
    fn plan_restore_downgrades_when_cwd_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("t.jsonl");
        std::fs::write(&present, b"x").unwrap();
        let snap = SessionSnapshot {
            schema_version: SCHEMA_VERSION,
            session_name: "w".into(),
            sidebar_visible: true,
            sidebar_width: 26,
            active_window: 0,
            windows: vec![WindowSnapshot {
                name: "w".into(),
                name_pinned: false,
                zoomed: false,
                focused_leaf: 0,
                layout: NodeSnapshot::Leaf(PaneSnapshot {
                    cwd: dir.path().join("vanished").to_string_lossy().to_string(),
                    agent: AgentResume::Claude {
                        session_id: "11111111-2222-3333-4444-555555555555".into(),
                        transcript: present.to_string_lossy().to_string(),
                    },
                }),
            }],
        };
        let plan = plan_restore(&snap);
        assert!(matches!(plan.panes[0].agent, AgentResume::None));
        assert_eq!(plan.panes[0].cwd_exists, false);
    }
}
