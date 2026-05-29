//! Per-pane metadata extracted from the foreground process's working
//! directory: git branch and (for Claude panes) the JSONL-derived
//! session label. Each submodule is independently testable.

pub mod branch;
pub mod claude;
