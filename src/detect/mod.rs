//! Agent state detection via terminal tail pattern matching.
//!
//! Each pane's bottom-of-buffer text is sampled periodically and matched
//! against known agent output patterns to classify state.

mod codex;
mod spinner_verbs;
use spinner_verbs::SPINNER_VERBS;

/// An agent dvlpr can classify the state of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agent {
    Claude,
    Codex,
}

/// The classified state of a known agent inside a pane.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AgentState {
    #[default]
    Idle,
    Working,
    Blocked,
    /// Finished a task while its window was unfocused — set only by the
    /// session stabilizer, never returned by `classify`. Clears to `Idle`
    /// when the window is focused.
    Done,
}

/// Map a foreground process's friendly name to an Agent, or None if the
/// name doesn't identify any known agent.
pub fn agent_for(process_name: &str) -> Option<Agent> {
    match process_name {
        "claude" => Some(Agent::Claude),
        "codex" => Some(Agent::Codex),
        _ => None,
    }
}

/// Classify a pane's current state given the agent and the bottom rows of
/// its grid as a flat string. Pure. May allocate a short-lived buffer for
/// case-folding (bounded — ~20 rows × cols cells).
pub fn classify(agent: Agent, tail: &str) -> AgentState {
    match agent {
        Agent::Claude => classify_claude(tail),
        Agent::Codex => codex::classify(tail),
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

    // Compute once — both detectors need it.
    let input_box = active_input_box(tail);

    // Blocked is checked BEFORE working: a permission prompt with
    // spinner overflow still correctly classifies as needing attention.
    if is_claude_blocked(tail, &lower, input_box.as_ref()) {
        return AgentState::Blocked;
    }

    if is_claude_working(tail, input_box.as_ref()) {
        return AgentState::Working;
    }

    AgentState::Idle
}

fn is_claude_blocked(tail: &str, lower: &str, input_box: Option<&InputBox>) -> bool {
    has_confirmation_prompt(lower)
        || lower.contains("do you want to proceed?")
        || lower.contains("would you like to proceed?")
        || lower.contains("waiting for permission")
        || lower.contains("do you want to allow this connection?")
        // AskUserQuestion always renders a "Chat about this" affordance —
        // a stable signal regardless of menu shape or wording.
        || lower.contains("chat about this")
        // Structural `❯ N. <label>` fallback. Only valid when there is NO
        // active input box: when the input box is visible at the bottom,
        // the agent is in normal interactive mode and any `❯ N.` higher
        // up is either (a) stale scrollback from a previously-dismissed
        // menu, or (b) a navigation menu (slash commands, settings) the
        // user is browsing — neither is "agent blocked on user". With no
        // input box, a full-screen dialog has taken over (trust folder,
        // AskUserQuestion without the keyword fallback) and the menu IS
        // the current state.
        || (input_box.is_none() && has_selection_prompt(tail))
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

/// Match Claude's working hint anywhere on screen EXCEPT inside the user's
/// own input box.
///
/// Two signals trip the Working state:
/// - the interrupt hint (`esc to interrupt` / `ctrl+c to interrupt`)
/// - a spinner verb framed with an ellipsis (`Combobulating…`, `Working…`,
///   `Pondering...`), drawn from Claude Code's own verb pool.
///
/// Real layout: the input box sits ABOVE the spinner, so the working hint
/// lives BELOW the box's bottom border. We can't anchor "above the first
/// ─ border" — that excludes the very rows the spinner occupies. Instead,
/// scan every line and skip those inside the active input box (which is
/// what would contain user-typed text matching either signal).
fn is_claude_working(tail: &str, input_box: Option<&InputBox>) -> bool {
    tail.lines().enumerate().any(|(i, line)| {
        if let Some(b) = input_box {
            if i >= b.content_start_line && i < b.content_end_line {
                return false; // line is inside the input box — ignore
            }
        }
        let lower = line.to_ascii_lowercase();
        if lower.contains("esc to interrupt") || lower.contains("ctrl+c to interrupt") {
            return true;
        }
        has_spinner_verb_line(line)
    })
}

/// True if `line` contains a Claude spinner verb framed with an ellipsis
/// (`<Verb>…` or `<Verb>...`). The ellipsis anchor is what separates a
/// spinner status from chat prose that happens to mention the same word.
///
/// Iterates whitespace-delimited words on the line, strips a trailing
/// `…` / `...`, and checks the remainder against the verb pool. The
/// `…` is U+2026 (3 bytes UTF-8); `...` is three ASCII dots — both shapes
/// appear in the wild.
fn has_spinner_verb_line(line: &str) -> bool {
    line.split_whitespace().any(|word| {
        for ellipsis in ["…", "..."] {
            if let Some(verb) = word.strip_suffix(ellipsis) {
                if SPINNER_VERBS.contains(&verb) {
                    return true;
                }
            }
        }
        false
    })
}

/// Line range (half-open: `[content_start_line, content_end_line)`) of the
/// content rows INSIDE the active input box at the bottom of the screen.
/// `content_end_line` is the index of the bottom border (the row AFTER the
/// last content row).
#[derive(Clone, Copy, Debug)]
struct InputBox {
    content_start_line: usize,
    content_end_line: usize,
}

/// Detect the bottom-of-screen Claude input box: a `❯`-cursored content
/// region bracketed by two horizontal-rule lines.
///
/// Real Claude layout:
/// ```text
/// ─────────────────────────       ← top border (HR)
/// ❯ <user input>                  ← cursor line (may span multiple rows)
/// ─────────────────────────       ← bottom border (HR)
/// <footer>                        ← status chips, hints
/// ```
///
/// We pick the LAST two HRs in the tail; the input box, if present, is
/// always the bottommost such bracketed region. The `❯` requirement
/// rejects accidental matches against unrelated double-rule blocks (e.g.,
/// banner separators) that don't contain a cursor.
fn active_input_box(tail: &str) -> Option<InputBox> {
    let lines: Vec<&str> = tail.lines().collect();
    let bot = lines.iter().rposition(|l| is_horizontal_rule(l))?;
    let top = lines[..bot].iter().rposition(|l| is_horizontal_rule(l))?;
    let has_cursor = lines[top + 1..bot]
        .iter()
        .any(|line| line.trim_start().starts_with('❯'));
    if !has_cursor {
        return None;
    }
    Some(InputBox {
        content_start_line: top + 1,
        content_end_line: bot,
    })
}

/// A row consisting only of `─` (and `╭╮╰╯` corners + whitespace), with at
/// least four `─` so a stray hyphen-like glyph doesn't false-match.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().filter(|c| *c == '─').count() < 4 {
        return false;
    }
    trimmed
        .chars()
        .all(|c| matches!(c, '─' | '╭' | '╮' | '╰' | '╯') || c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_for_known_names() {
        assert_eq!(agent_for("claude"), Some(Agent::Claude));
        assert_eq!(agent_for("codex"), Some(Agent::Codex));
        assert_eq!(agent_for("zsh"), None);
        assert_eq!(agent_for(""), None);
        assert_eq!(agent_for("Claude"), None); // case-sensitive
        assert_eq!(agent_for("Codex"), None); // case-sensitive
    }

    #[test]
    fn agent_for_codex_name_returns_codex_variant() {
        assert_eq!(agent_for("codex"), Some(Agent::Codex));
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
    fn classify_claude_marker_inside_input_box_is_idle() {
        // User typed "esc to interrupt" into their own input. The actual
        // Claude input cursor is `❯`, not `>` (the original spec was
        // wrong). The marker must be ignored because it sits INSIDE the
        // input box's content region (between top/bot ─ borders).
        let tail = "────────\n❯ esc to interrupt\n────────\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_fresh_session_ignores_stale_menu_in_scrollback() {
        // Repro of the live bug: user dismissed a trust-folder prompt and
        // re-ran `claude`. Claude Code does NOT clear the screen — the
        // dismissed menu's `❯ 2. No, exit` row persists in scrollback
        // above the new input box. Without input-box-aware gating, the
        // structural fallback keeps matching the stale row and reports
        // Blocked forever even though the user is idle.
        let tail = "Quick safety check: Is this a project you created?\n\
                    ❯ 2. No, exit\n\
                    Enter to confirm · Esc to cancel\n\
                    alkein@host ~ % claude\n\
                    Claude Code v2.1.153\n\
                    ─────────────────────────\n\
                    ❯\u{a0}\n\
                    ─────────────────────────\n\
                    Sonnet 4.6  high\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_working_with_active_input_box_and_spinner_below() {
        // Normal working state: input box visible at the bottom, spinner
        // line BELOW the bottom border. Working detection must look past
        // the input box to find the spinner.
        let tail = "...previous response...\n\
                    ─────────────────────────\n\
                    ❯ tell me a joke\n\
                    ─────────────────────────\n\
                    ✻ Generating… (5s · esc to interrupt)\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_navigation_menu_with_input_box_is_idle() {
        // Slash-command / settings menus reuse the same `❯ N.` selection
        // widget as permission prompts. When the input box is still
        // visible at the bottom, the user is navigating — NOT waiting on
        // the agent. Claude reuses the same generic Select widget for both
        // permission flows and ordinary slash/settings menus, so the visible
        // input box is the disambiguator.
        let tail = "Available commands:\n\
                    ❯ 1. /help\n\
                      2. /model\n\
                      3. /clear\n\
                    ─────────────────────────\n\
                    ❯ /\n\
                    ─────────────────────────\n";
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

    #[test]
    fn classify_claude_working_spinner_verb_alone_combobulating() {
        // Spinner verb anchored by `…` — the working hint should match
        // even when "esc to interrupt" isn't on screen (e.g., narrow pane
        // where the parens wrap off, or the verb-only frame Claude shows
        // between tool steps).
        let tail = "✻ Combobulating… (3s)\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_working_spinner_verb_working_with_ellipsis() {
        // "Working" is itself a spinner verb in Claude's pool. Must trip
        // detection ONLY when framed with the spinner ellipsis — bare
        // "Working" in chat prose stays Idle (covered in a separate test).
        let tail = "✻ Working…\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_working_spinner_verb_ascii_three_dots() {
        // Some Claude renderings (and some other agent TUIs) use ASCII
        // `...` rather than the Unicode `…`. Match both.
        let tail = "Working...\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Working);
    }

    #[test]
    fn classify_claude_bare_spinner_verb_in_prose_is_idle() {
        // Without the ellipsis anchor, a spinner-verb word in chat output
        // must NOT false-positive Working. This protects against Claude's
        // own responses mentioning these words conversationally.
        let tail = "I think we were working on something earlier.\n\
                    Just Pondering the next step here.\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_claude_spinner_verb_inside_input_box_is_idle() {
        // User typed "Combobulating…" into their own input box — should
        // be ignored by the same input-box-content skip as "esc to
        // interrupt".
        let tail = "─────────────────────────\n\
                    ❯ Combobulating…\n\
                    ─────────────────────────\n";
        assert_eq!(classify(Agent::Claude, tail), AgentState::Idle);
    }

    #[test]
    fn classify_dispatches_codex_to_codex_module() {
        assert_eq!(
            classify(Agent::Codex, "Working (1s • esc to interrupt)"),
            AgentState::Working
        );
        assert_eq!(
            classify(Agent::Codex, "Would you like to run the following command?"),
            AgentState::Blocked
        );
        assert_eq!(
            classify(Agent::Codex, "Ask Codex to do anything"),
            AgentState::Idle
        );
    }
}
