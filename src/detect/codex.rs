//! Codex CLI state detection via terminal-tail pattern matching.
//!
//! Markers are source-verified against `openai/codex` v0.135 (`codex-rs/tui/`).
//! dvlpr matches decoded grid glyph text (no ANSI escapes), so Codex's
//! shimmer-animated status header is not fragmented and substrings are
//! contiguous. This module is the single place to tune Codex markers when the
//! TUI wording drifts across versions.

use super::AgentState;

/// Approval / question overlay markers. Any present (case-insensitive) ⇒
/// Blocked. The five titles are one per Codex approval kind; the footer rule
/// (`to confirm` AND `to cancel`, both present) catches overlay variants whose
/// title wording drifts.
const BLOCKED_MARKERS: &[&str] = &[
    "would you like to run the following command?",
    "would you like to make the following edits?",
    "would you like to grant these permissions?",
    "do you want to approve network access to",
    "needs your approval.",
];

/// Live status-line markers ⇒ Working. `to interrupt)` is primary: it survives
/// the user rebinding the interrupt key away from Esc (only the suffix is
/// constant). `esc to interrupt` is a redundant safety net.
const WORKING_MARKERS: &[&str] = &["to interrupt)", "esc to interrupt"];

/// Classify a Codex pane's state from its screen tail.
///
/// Blocked is checked before Working: while an approval overlay is up, Codex
/// replaces the `Working (… esc to interrupt)` status line, but a stale
/// "Working" line may remain in the transcript above — the live state is
/// Blocked.
pub(super) fn classify(tail: &str) -> AgentState {
    let lower = tail.to_ascii_lowercase();
    if is_blocked(&lower) {
        return AgentState::Blocked;
    }
    if is_working(&lower) {
        return AgentState::Working;
    }
    AgentState::Idle
}

fn is_blocked(lower: &str) -> bool {
    if BLOCKED_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // Footer present on every approval overlay regardless of title wording.
    lower.contains("to confirm") && lower.contains("to cancel")
}

fn is_working(lower: &str) -> bool {
    WORKING_MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_on_interrupt_hint() {
        assert_eq!(classify("Working (3s • esc to interrupt)"), AgentState::Working);
    }

    #[test]
    fn working_survives_key_rebind_via_to_interrupt_suffix() {
        // Interrupt key rebound away from Esc; only `to interrupt)` remains.
        assert_eq!(classify("Working (12s • ctrl-x to interrupt)"), AgentState::Working);
    }

    #[test]
    fn blocked_on_each_approval_title() {
        for title in [
            "Would you like to run the following command?",
            "Would you like to make the following edits?",
            "Would you like to grant these permissions?",
            "Do you want to approve network access to \"example.com\"?",
            "filesystem needs your approval.",
        ] {
            assert_eq!(classify(title), AgentState::Blocked, "title: {title}");
        }
    }

    #[test]
    fn blocked_on_footer_pair() {
        assert_eq!(
            classify("Press Enter to confirm    Esc to cancel"),
            AgentState::Blocked
        );
    }

    #[test]
    fn footer_word_alone_does_not_block() {
        assert_eq!(classify("press enter to confirm your choice"), AgentState::Idle);
        assert_eq!(classify("nothing to cancel here"), AgentState::Idle);
    }

    #[test]
    fn numbered_option_block_with_title_is_blocked() {
        let tail = "Would you like to run the following command?\n\
                    › 1. Yes, proceed\n  2. No, and tell Codex what to do differently\n";
        assert_eq!(classify(tail), AgentState::Blocked);
    }

    #[test]
    fn idle_composer_only() {
        // Empty composer placeholder + no live status line ⇒ Idle.
        assert_eq!(classify("Ask Codex to do anything"), AgentState::Idle);
    }

    #[test]
    fn idle_transcript_without_live_status_line() {
        // Prior finished output + composer, no `to interrupt` status line.
        let tail = "Done. Updated 3 files.\n\nAsk Codex to do anything\n";
        assert_eq!(classify(tail), AgentState::Idle);
    }

    #[test]
    fn blocked_wins_over_stale_working_line() {
        // Stale working line in transcript, live approval overlay below.
        let tail = "Working (2s • esc to interrupt)\n...\n\
                    Would you like to run the following command?\n› 1. Yes, proceed\n";
        assert_eq!(classify(tail), AgentState::Blocked);
    }
}
