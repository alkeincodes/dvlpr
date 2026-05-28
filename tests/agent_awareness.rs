//! End-to-end test for the agent-awareness sidebar.
//!
//! Drives `Session` directly without `server::run`, using only public
//! APIs: feed / refresh_agent_states / compose / toggle_sidebar /
//! apply_command. Observation is through `Session::compose()`'s public
//! Grid output and `Session::active_window_index()` / `focused_pane()`.
//!
//! Per the spec, the resolver injection here is what enables testing
//! without a real Claude binary: the closure |_pid| Some("claude".into())
//! makes any spawned shell process classify as Claude for the duration
//! of the test.

use std::time::Duration;

use dvlpr::config::Command;
use dvlpr::input::{MouseEvent, MouseKind};
use dvlpr::layout::SplitPath;
use dvlpr::session::Session;
use dvlpr::theme::Theme;

#[tokio::test]
async fn sidebar_renders_claude_pane_with_state_colors_and_responds_to_click() {
    // 1. Build a Session with one shell pane. Session::new takes 6 args
    //    (the 6th is theme), and returns Result<(Self, PaneId, rx)>.
    let (mut session, pane_id, _rx) = Session::new(
        "test".to_string(),
        vec!["sh".to_string(), "-c".to_string(), "sleep 30".to_string()],
        std::env::current_dir()
            .unwrap()
            .to_string_lossy()
            .to_string(),
        80,
        24,
        Theme::default(),
    )
    .expect("Session::new");

    // 2. Toggle sidebar visible.
    let _ = session.apply_command(Command::ToggleSidebar);

    let grid_before_classify = session.compose();
    assert_eq!(grid_before_classify.cols, 80);

    // 3. Feed busy marker positioned so tail_text(20) can see it.
    //    After toggle_sidebar the pane is resized to 64×23.
    //    tail_text(20) reads the last 20 rows (rows 3..22, 0-based).
    //    \x1b[20;1H moves the cursor to row 20, col 1 (1-based) = row 19 (0-based),
    //    which is within the tail window.
    session.feed(pane_id, b"\x1b[2J\x1b[20;1Hesc to interrupt\n");
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string()));

    let theme = Theme::default();
    let grid = session.compose();
    // Sidebar rect: cols (80-16)=64..79, rows 0..22. Dot at column 64+13 = 77,
    // row 2 (first entry, after AGENTS header + separator).
    // grid.cols = 80 (full viewport width).
    let dot_idx = (2 * grid.cols as usize) + 77;
    assert_eq!(
        grid.cells[dot_idx].style.fg, theme.agent_working_fg,
        "dot should be theme.agent_working_fg (the default flavor's yellow)"
    );

    // 4. Feed blocked marker; immediate transition.
    session.feed(
        pane_id,
        b"\x1b[2J\x1b[20;1HDo you want to proceed?\nYes / No\n",
    );
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string()));
    let grid = session.compose();
    assert_eq!(
        grid.cells[dot_idx].style.fg, theme.agent_blocked_fg,
        "blocked should flip immediately"
    );

    // 5. Clear; two ticks for the idle stabilizer.
    session.feed(pane_id, b"\x1b[2J\x1b[H");
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string())); // streak=1
    let grid = session.compose();
    assert_eq!(
        grid.cells[dot_idx].style.fg, theme.agent_blocked_fg,
        "single idle sample should NOT flip"
    );
    let _ = session.refresh_agent_states(|_pid| Some("claude".to_string())); // streak=2 → idle
    let grid = session.compose();
    assert_eq!(
        grid.cells[dot_idx].style.fg, theme.agent_idle_fg,
        "two consecutive idle samples → Idle"
    );

    // 6. Click on the sidebar entry via the PUBLIC handle_mouse API.
    let mut drag: Option<(usize, SplitPath)> = None;
    session.handle_mouse(
        MouseEvent {
            button: 0,
            col: 70,
            row: 3,
            kind: MouseKind::Press,
        },
        &mut drag,
    );
    assert!(drag.is_none(), "sidebar click should not initiate a drag");
    assert_eq!(session.active_window_index(), 0);
    assert_eq!(session.focused_pane(), pane_id);

    // 7. Toggle off; content area returns to full width.
    let _ = session.apply_command(Command::ToggleSidebar);
    let grid = session.compose();
    let cell = grid.cells[(2 * grid.cols as usize) + 77];
    assert_ne!(
        cell.style.fg, theme.agent_idle_fg,
        "sidebar should be gone after toggle off"
    );

    // Cleanup
    drop(session);
    tokio::time::sleep(Duration::from_millis(50)).await;
}
