//! Codex CLI state detection via terminal-tail pattern matching.
//!
//! Markers are source-verified against `openai/codex` v0.135 (`codex-rs/tui/`).
//! dvlpr matches decoded grid glyph text (no ANSI escapes), so Codex's
//! shimmer-animated status header is not fragmented and substrings are
//! contiguous. This module is the single place to tune Codex markers when the
//! TUI wording drifts across versions.

use super::AgentState;

/// Approval / question overlay markers. Any present (case-insensitive) ⇒
/// Blocked. The approval titles are one per Codex approval kind; the
/// trust-directory prompt is the onboarding gate Codex shows on first run
/// in an untrusted cwd; the footer rule (`to confirm` AND `to cancel`,
/// both present) catches overlay variants whose title wording drifts.
const BLOCKED_MARKERS: &[&str] = &[
    "would you like to run the following command?",
    "would you like to make the following edits?",
    "would you like to grant these permissions?",
    "do you want to approve network access to",
    "needs your approval",
    // Onboarding trust gate: "Do you trust the contents of this directory?"
    // with a `1. Yes, continue / 2. No, quit` menu. None of the approval
    // titles nor the confirm/cancel footer appear, so match it directly.
    "do you trust the contents of this directory",
];

/// Live status-line marker ⇒ Working. Codex renders `Working (Ns • <key> to
/// interrupt)`. Matching the bare `to interrupt` (no surrounding paren or key
/// prefix) survives both rebinding the interrupt key away from Esc AND the
/// closing paren being truncated on a narrow terminal.
const WORKING_MARKERS: &[&str] = &["to interrupt"];

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
    if lower.contains("to confirm") && lower.contains("to cancel") {
        return true;
    }
    // Structural fallback: a `› N.` selection cursor on a numbered option.
    // Every Codex select widget (approval, trust gate, custom "ask the user"
    // prompts) renders this, so it catches question shapes whose wording
    // isn't enumerated above. We match ONLY Codex's `›` cursor, never
    // Claude's `❯` — a Claude-style prompt leaking into a Codex pane must
    // not block (see `refresh_agent_states_codex_ignores_claude_only_prompt`).
    has_numbered_selection(lower)
}

/// A `›` selection cursor immediately followed (after optional spaces) by
/// one or more digits and a `.` — i.e. the highlighted row of a Codex
/// numbered menu. `to_ascii_lowercase` leaves `›`, digits, and `.`
/// untouched, so matching on the lowercased tail is fine.
fn has_numbered_selection(tail: &str) -> bool {
    const CURSOR: char = '›';
    tail.lines().any(|line| {
        let Some(idx) = line.find(CURSOR) else {
            return false;
        };
        let after = line[idx + CURSOR.len_utf8()..].trim_start();
        let mut saw_digit = false;
        for c in after.chars() {
            if c.is_ascii_digit() {
                saw_digit = true;
            } else {
                return saw_digit && c == '.';
            }
        }
        false
    })
}

fn is_working(lower: &str) -> bool {
    WORKING_MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn working_on_interrupt_hint() {
        assert_eq!(
            classify("Working (3s • esc to interrupt)"),
            AgentState::Working
        );
    }

    #[test]
    fn working_survives_key_rebind_via_to_interrupt_suffix() {
        // Interrupt key rebound away from Esc; only `to interrupt)` remains.
        assert_eq!(
            classify("Working (12s • ctrl-x to interrupt)"),
            AgentState::Working
        );
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
    fn blocked_on_trust_directory_prompt() {
        // Codex onboarding trust gate — no approval title, no confirm/cancel
        // footer. Regression for the sidebar showing Idle while Codex waits.
        let tail = "You are in /Users/alkein\n\
                    Do you trust the contents of this directory? Working with \
                    untrusted contents comes with higher risk of prompt injection.\n\
                    › 1. Yes, continue\n  2. No, quit\n\nPress enter to continue\n";
        assert_eq!(classify(tail), AgentState::Blocked);
    }

    #[test]
    fn blocked_on_numbered_selection_cursor_alone() {
        // Structural fallback: a `› N.` menu with arbitrary wording (a custom
        // "ask the user" prompt) must still block.
        let tail = "Which option do you prefer?\n› 1. Option A\n  2. Option B\n";
        assert_eq!(classify(tail), AgentState::Blocked);
    }

    #[test]
    fn numbered_selection_requires_digit_then_dot() {
        // A `›` cursor without a numbered option (e.g. the empty composer
        // prompt), or a digit not followed by `.`, must NOT block.
        assert_eq!(classify("› Ask Codex to do anything"), AgentState::Idle);
        assert_eq!(classify("› 1 banana"), AgentState::Idle);
    }

    #[test]
    fn claude_caret_cursor_does_not_block_codex() {
        // Claude's `❯ N.` cursor leaking into a Codex pane must stay Idle —
        // the structural fallback is `›`-only.
        assert_eq!(classify("Pick:\n❯ 1. Yes\n  2. No\n"), AgentState::Idle);
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
        assert_eq!(
            classify("press enter to confirm your choice"),
            AgentState::Idle
        );
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

    #[test]
    fn working_survives_truncated_status_line() {
        // Narrow terminal clipped the closing paren AND the key was rebound.
        assert_eq!(
            classify("Working (12s • ctrl-x to interrupt"),
            AgentState::Working
        );
    }
}
