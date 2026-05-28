//! Agent state detection via terminal tail pattern matching.
//!
//! Each pane's bottom-of-buffer text is sampled periodically and matched
//! against known agent output patterns to classify state.

/// An agent dvlpr can classify the state of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agent {
    Claude,
}

/// The classified state of a known agent inside a pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AgentState {
    #[default]
    Idle,
    Working,
    Blocked,
}

/// Map a foreground process's friendly name to an Agent, or None if the
/// name doesn't identify any known agent.
pub fn agent_for(process_name: &str) -> Option<Agent> {
    match process_name {
        "claude" => Some(Agent::Claude),
        _ => None,
    }
}

/// Classify a pane's current state given the agent and the bottom rows of
/// its grid as a flat string. Pure. May allocate a short-lived buffer for
/// case-folding (bounded — ~20 rows × cols cells).
pub fn classify(agent: Agent, tail: &str) -> AgentState {
    match agent {
        Agent::Claude => classify_claude(tail),
    }
}

fn classify_claude(tail: &str) -> AgentState {
    let lower = tail.to_ascii_lowercase();

    // Idle pre-filters — avoid false-positive Working when the user is
    // browsing scrollback or using the search prompt.
    if tail.contains("⌕ Search…") {
        return AgentState::Idle;
    }
    if lower.contains("ctrl+r to toggle") {
        return AgentState::Idle;
    }

    // Blocked is checked BEFORE working: a permission prompt with
    // spinner overflow still correctly classifies as needing attention.
    if is_claude_blocked(tail, &lower) {
        return AgentState::Blocked;
    }

    if is_claude_working(tail) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn is_claude_blocked(tail: &str, lower: &str) -> bool {
    has_confirmation_prompt(lower)
        || lower.contains("do you want to proceed?")
        || lower.contains("would you like to proceed?")
        || lower.contains("waiting for permission")
        || lower.contains("do you want to allow this connection?")
        // AskUserQuestion always renders a "Chat about this" affordance —
        // a stable signal regardless of menu shape or wording.
        || lower.contains("chat about this")
        // Any `❯ N. <label>` selection menu means the agent is waiting for
        // user input — whether the labels are yes/no (permission prompts)
        // or open-ended (AskUserQuestion). `❯` (U+276F) is the menu cursor
        // glyph; Claude's regular input cursor is plain `>` so they don't
        // collide.
        || has_selection_prompt(tail)
}

/// "do you want" or "would you like", with "yes" or "❯" appearing later
/// in the tail (so a stray sentence like "do you want to keep this
/// open?" without an actual yes/no UI nearby doesn't trigger Blocked).
fn has_confirmation_prompt(lower: &str) -> bool {
    let pos = lower
        .find("do you want")
        .or_else(|| lower.find("would you like"));
    match pos {
        Some(p) => {
            let after = &lower[p..];
            after.contains("yes") || after.contains('❯')
        }
        None => false,
    }
}

/// A "❯ N." selection cursor appears anywhere on a line — the `❯` glyph may
/// be preceded by box-drawing chars (e.g., `│ ` when the menu is inside a
/// bordered dialog like AskUserQuestion's). We require digits followed
/// immediately by `.` after the cursor to keep `❯ Yes` (no digit) from
/// matching here — `has_confirmation_prompt` covers that path.
fn has_selection_prompt(tail: &str) -> bool {
    tail.lines().any(|line| {
        let Some(idx) = line.find('❯') else {
            return false;
        };
        let after = line[idx + '❯'.len_utf8()..].trim_start();
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

/// Match Claude's working hint on any line that isn't part of the user's
/// own input row.
///
/// Real layout: the input box sits ABOVE the spinner, so the working hint
/// lives BELOW the box's bottom border. We can't anchor "above the first
/// ─ border" — that excludes the very rows the spinner occupies. Instead,
/// look at every line and skip ones starting with `>` (the user's input
/// cursor inside the prompt box). Claude renders selection-menu cursors
/// as `❯`, never `>`, so the two don't collide.
fn is_claude_working(tail: &str) -> bool {
    tail.lines().any(|line| {
        let trimmed = line.trim_start();
        if trimmed.starts_with('>') {
            // User typed "esc to interrupt" into their own prompt — ignore.
            return false;
        }
        let lower = trimmed.to_ascii_lowercase();
        lower.contains("esc to interrupt") || lower.contains("ctrl+c to interrupt")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_for_known_names() {
        assert_eq!(agent_for("claude"), Some(Agent::Claude));
        assert_eq!(agent_for("zsh"), None);
        assert_eq!(agent_for(""), None);
        assert_eq!(agent_for("Claude"), None); // case-sensitive
    }

    #[test]
    fn classify_claude_empty_tail_is_idle() {
        assert_eq!(classify(Agent::Claude, ""), AgentState::Idle);
    }

    #[test]
    fn classify_claude_default_prompt_is_idle() {
        let tail = "\n────────\n>\n────────\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_search_mode_is_idle_even_with_working_marker_in_scrollback() {
        let tail = "esc to interrupt\n⌕ Search…\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_ctrl_r_toggle_is_idle() {
        let tail = "esc to interrupt\nctrl+r to toggle\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_working_esc_to_interrupt() {
        let tail = "✻ Generating…\nesc to interrupt\n────────\n>\n────────\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_working_ctrl_c_to_interrupt() {
        let tail = "running tool…\nctrl+c to interrupt\n────────\n>\n────────\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_marker_below_prompt_box_is_idle() {
        // User typed "esc to interrupt" into their own prompt — should NOT
        // false-positive Working because the marker is BELOW the prompt box.
        let tail = "────────\n> esc to interrupt\n────────\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_blocked_do_you_want_to_proceed() {
        let tail = "Do you want to proceed?\n❯ 1. Yes\n  2. No\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_would_you_like_to_proceed() {
        let tail = "Would you like to proceed?\n❯ Yes\n  No\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_waiting_for_permission() {
        let tail = "Waiting for permission to write file…\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_allow_this_connection() {
        let tail = "Do you want to allow this connection?\nYes / No\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_structural_selection_plus_yes_no() {
        // No "do you want" / "would you like" — only the structural fallback.
        let tail = "Pick one:\n❯ 1. Yes\n  2. No, and tell claude what to do\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_takes_precedence_over_working() {
        // Tail containing BOTH "esc to interrupt" above prompt AND
        // "do you want to proceed?" — should be Blocked, not Working.
        let tail = "esc to interrupt\nDo you want to proceed?\n❯ 1. Yes\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_confirmation_prompt_needs_yes_or_caret() {
        // "do you want" without yes/❯ nearby → NOT blocked.
        let tail = "I'll explain what I'm about to do. Do you want to keep going?\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_working_when_spinner_below_prompt_box() {
        // Real Claude Code layout: the input box is drawn ABOVE the spinner —
        // the working hint lives BELOW the box's bottom border. v1's
        // "above the prompt box only" logic missed this and reported Idle.
        let tail = "context above\n\
                    ╭──────────────────────────────────╮\n\
                    │ >                                │\n\
                    ╰──────────────────────────────────╯\n\
                    ✻ Generating… (5s · esc to interrupt)\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_blocked_open_ended_selection_prompt() {
        // AskUserQuestion renders a menu with arbitrary labels — never "yes"
        // / "no". The structural fallback must catch it regardless.
        let tail = "╭──────────────────────────────────╮\n\
                    │ Which library should we use?     │\n\
                    │                                  │\n\
                    │ ❯ 1. date-fns                    │\n\
                    │   2. moment                      │\n\
                    │   3. dayjs                       │\n\
                    ╰──────────────────────────────────╯\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_ask_user_question_chat_about_this_marker() {
        // "Chat about this" is a footer affordance that AskUserQuestion
        // consistently renders. Match it as a keyword so we catch the
        // tool even if the menu's rendering ever changes shape and the
        // structural `❯ N.` fallback misses.
        let tail = "Which approach do you want?\n\
                    1. Refactor first\n\
                    2. Add tests first\n\
                    Chat about this · ↑↓ to navigate · enter to select\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_blocked_trust_folder_safety_check() {
        // Regression: Claude Code's "Quick safety check / trust this folder"
        // screen (full-width, NOT inside a bordered dialog). Was blocked
        // pre-fix via the yes/no anchor; must remain blocked post-fix via
        // the now-yes/no-agnostic structural fallback.
        let tail = "Accessing workspace:\n\
                    /Users/alkein\n\
                    Quick safety check: Is this a project you created or one you trust?\n\
                    Claude Code'll be able to read, edit, and execute files here.\n\
                    Security guide\n\
                    ❯ 1. Yes, I trust this folder\n  2. No, exit\n\
                    Enter to confirm · Esc to cancel\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Blocked);
    }

    #[test]
    fn classify_claude_working_paren_spinner_format() {
        // The on-screen spinner is parenthesized: "(5s · esc to interrupt)".
        // Detection should match it the same as the bare form.
        let tail = "✻ Generating… (12s · ↓ 2.1k tokens · esc to interrupt)\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }
}
