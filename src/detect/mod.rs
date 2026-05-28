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
        || (has_selection_prompt(tail) && has_yes_no_choice(tail))
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

/// A line starting with "❯" that also contains a digit-dot pair somewhere
/// on the same line (matches "❯ 1. Yes" patterns).
fn has_selection_prompt(tail: &str) -> bool {
    for line in tail.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('❯')
            && trimmed.chars().any(|c| c.is_ascii_digit())
            && trimmed.contains('.')
        {
            return true;
        }
    }
    false
}

/// At least one line whose trimmed content (with leading ❯ stripped) reads
/// as a yes/no choice. Anchors the structural fallback so a stray "❯"
/// without yes/no doesn't trigger Blocked.
fn has_yes_no_choice(tail: &str) -> bool {
    tail.lines().any(|line| {
        let t = line
            .trim()
            .trim_start_matches('❯')
            .trim_start()
            .to_lowercase();
        t == "yes"
            || t == "no"
            || t.starts_with("1. yes")
            || t.starts_with("2. no")
            || t.starts_with("yes, and ")
            || t.starts_with("no, and tell claude")
    })
}

fn is_claude_working(tail: &str) -> bool {
    let above = content_above_prompt_box(tail);
    let lower = above.to_ascii_lowercase();
    lower.contains("esc to interrupt") || lower.contains("ctrl+c to interrupt")
}

/// Claude's input UI is bracketed by two horizontal-rule lines with `❯`
/// between them. Return the tail content above the FIRST top-border found;
/// if no box, return the whole tail.
///
/// Heuristic match: a row consisting (after trim) of ≥4 consecutive '─'
/// plus only whitespace / box-corner glyphs. Brittle against titled
/// borders — accepted v1 limitation per the spec.
fn content_above_prompt_box(tail: &str) -> &str {
    let lines: Vec<&str> = tail.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.chars().filter(|c| *c == '─').count() >= 4
            && trimmed.chars().all(|c| {
                c == '─' || c.is_whitespace() || c == '┌' || c == '┐' || c == '╭' || c == '╮'
            })
        {
            let byte_offset: usize = lines[..i].iter().map(|l| l.len() + 1).sum();
            return &tail[..byte_offset.min(tail.len())];
        }
    }
    tail
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
}
