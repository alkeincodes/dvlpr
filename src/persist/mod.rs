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
}
