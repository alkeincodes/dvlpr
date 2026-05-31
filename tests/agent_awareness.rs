//! End-to-end test for the agent-awareness sidebar.
//!
//! Drives `Session` directly without `server::run`, using only public APIs:
//! feed / refresh_agent_states / compose. Observation is through
//! `Session::compose()`'s public Grid output.
//!
//! The resolver injection here is what enables testing without a real Claude
//! binary: the closure `|_pid| Some("claude".into())` makes any spawned shell
//! process classify as Claude for the duration of the test.

use dvlpr::compositor::{Color, Grid};
use dvlpr::session::Session;
use dvlpr::theme::Theme;

fn build_session() -> (
    Session,
    dvlpr::layout::PaneId,
    tokio::sync::mpsc::UnboundedReceiver<dvlpr::pane::PaneOutput>,
) {
    Session::new(
        "test".to_string(),
        vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
        std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string(),
        80,
        24,
        Theme::default(),
        dvlpr::config::KeySpec::Ctrl('b'),
        dvlpr::config::KeyMap::default(),
        dvlpr::layout::SIDEBAR_WIDTH_DEFAULT,
        0,
    )
    .expect("Session::new")
}

/// Foreground color of the first sidebar agent-entry dot. The sidebar is open by
/// default, and the first entry sits at a fixed cell: row 3 (header, divider,
/// blank) and column `sidebar_x + 2`. Only the glyph and color change with state
/// (`●` working / `○` idle / `!` blocked / `✓` done), so we read the cell by
/// position rather than by glyph.
fn dot_fg(grid: &Grid) -> Color {
    let cols = grid.cols as usize;
    let sidebar_x = cols - dvlpr::layout::SIDEBAR_WIDTH_DEFAULT as usize;
    let dot_col = sidebar_x + 2;
    let dot_row = 3;
    grid.cells[dot_row * cols + dot_col].style.fg
}

#[tokio::test]
async fn sidebar_renders_claude_pane_with_state_colors() {
    let (mut session, pane_id, _rx) = build_session();
    let theme = Theme::default();

    // Working: the "esc to interrupt" marker classifies Claude as working.
    session.feed(pane_id, b"\x1b[2J\x1b[20;1Hesc to interrupt\n");
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string()));
    assert_eq!(
        dot_fg(&session.compose()),
        theme.agent_working_fg,
        "the working marker should paint the dot with agent_working_fg"
    );

    // Blocked: a confirmation prompt flips it immediately.
    session.feed(
        pane_id,
        b"\x1b[2J\x1b[20;1HDo you want to proceed?\nYes / No\n",
    );
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string()));
    assert_eq!(
        dot_fg(&session.compose()),
        theme.agent_blocked_fg,
        "a prompt should flip the dot to agent_blocked_fg immediately"
    );

    // Idle: clear the marker; two ticks satisfy the idle stabilizer.
    session.feed(pane_id, b"\x1b[2J\x1b[H");
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string()));
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string()));
    assert_eq!(
        dot_fg(&session.compose()),
        theme.agent_idle_fg,
        "after clearing the marker and two idle ticks, the dot should be idle"
    );
}
