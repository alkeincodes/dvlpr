//! Per-client input decoding. The thin client forwards raw stdin bytes; this
//! state machine turns them into pane-bound bytes, structural `Command`s, or
//! `MouseEvent`s. It is the only place the prefix keymap and SGR mouse encoding
//! are interpreted. Pure transform — no I/O, unit-tested without PTYs/sockets.
//!
//! Escape ambiguity (only the genuinely-ambiguous lone-ESC cases get a timer): a
//! lone trailing `ESC` in normal mode (standalone Escape vs. the start of a
//! CSI/SS3 sequence), or `prefix ESC` in command mode (cancel the prefix vs. an
//! arrow key whose continuation hasn't arrived). These are reported by
//! `pending_escape()`; the run loop arms a ~50ms timer and, if no continuation
//! arrives, calls `flush_escape_timeout()` — sending the lone `ESC` to the focused
//! pane (normal mode) or cancelling the prefix (command mode). Once a sequence is
//! *committed* (`ESC [`, `ESC [ <`, `ESC O`) it is NOT ambiguous: the parser just
//! buffers the remaining bytes across `feed` calls until the sequence completes,
//! with no timer — so fragmented arrows/mouse reports are never leaked to the pane
//! by a timeout. Continuation bytes that resolve a committed sequence clear any
//! prior deadline (the state is no longer `pending_escape()`).

use crate::config::{Config, Key, NamedKey};

const ESC: u8 = 0x1b;
/// Upper bound on a buffered SGR mouse param run (`ESC [ < b ; x ; y`). A
/// well-formed report is far shorter; this caps a malformed/runaway sequence so it
/// can never buffer unbounded client input.
const MAX_MOUSE_SEQ: usize = 32;
/// Upper bound on a buffered non-mouse CSI sequence, so an unterminated `ESC [ …`
/// can't buffer unbounded input before being flushed to the pane.
const MAX_CSI_SEQ: usize = 32;

/// A mouse button action decoded from an SGR report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    Press,
    Drag,
    Release,
}

/// A decoded SGR mouse event. `col`/`row` are 1-based (the wire encoding), matching
/// what `layout::hit_test` expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseEvent {
    pub button: u8,
    pub col: u16,
    pub row: u16,
    pub kind: MouseKind,
}

/// One decoded input event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEvent {
    /// Bytes to write to the focused pane.
    Pane(Vec<u8>),
    /// A structural command from the prefix keymap.
    Command(crate::config::Command),
    /// A mouse event to hit-test against the active window.
    Mouse(MouseEvent),
    /// The client's host terminal just gained focus. Carries no payload —
    /// the server already knows which client emitted this from connection
    /// identity. FocusOut is intentionally not represented; the parser
    /// consumes the corresponding `ESC [ O` sequence and emits nothing.
    FocusIn,
    /// A decoded key while copy mode is active. Routed to the session's
    /// copy-mode handler; never reaches the pane PTY or the prefix keymap.
    CopyKey(crate::copymode::CopyKey),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    NEsc,   // normal mode, saw ESC
    NCsi,   // normal mode, saw ESC [
    NMouse, // normal mode, saw ESC [ < — collecting mouse params
    Armed,  // prefix pressed, awaiting a command key
    AEsc,   // armed, saw ESC
    ACsi,   // armed, saw ESC [
    ASs3,   // armed, saw ESC O
}

/// Per-client parser. One instance per connected client; holds the in-progress
/// escape/mouse buffer and the prefix state across `feed` calls.
pub struct InputParser {
    state: State,
    pending: Vec<u8>,
    /// Accumulator for Ground-mode pass-through bytes. Flushed as a single
    /// `Pane` event whenever the parser leaves Ground state (or at end of
    /// `feed`), so consecutive plain bytes arrive as one chunk.
    ground_buf: Vec<u8>,
}

impl Default for InputParser {
    fn default() -> Self {
        Self::new()
    }
}

impl InputParser {
    pub fn new() -> Self {
        InputParser {
            state: State::Ground,
            pending: Vec::new(),
            ground_buf: Vec::new(),
        }
    }

    /// Decode a chunk of raw client bytes into events.
    ///
    /// When `copy_mode` is `true`, the parser does NOT arm the prefix and does NOT
    /// accumulate pane bytes — control bytes → `CopyKey::Ctrl`, printable bytes →
    /// `CopyKey::Char`, and CSI sequences decode to navigation `CopyKey`s. When
    /// `copy_mode` is `false`, parsing is byte-identical to the pre-copy-mode
    /// behaviour (the regression test `normal_mode_prefix_still_arms_when_copy_mode_inactive_regression`
    /// pins this).
    pub fn feed(&mut self, config: &Config, bytes: &[u8], copy_mode: bool) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for &b in bytes {
            self.step(config, b, copy_mode, &mut out);
        }
        // Flush any accumulated Ground-mode bytes at the end of the feed.
        self.flush_ground(&mut out);
        out
    }

    /// True ONLY for the genuinely-ambiguous lone-ESC states (`NEsc`, `AEsc`) — so
    /// the run loop arms the ESCDELAY timer only when a standalone Escape can't yet
    /// be told apart from a sequence start. Committed sequences (`NCsi`/`NMouse`/
    /// `ACsi`/`ASs3`) are NOT pending: they just wait for completion, so a
    /// fragmented arrow or mouse report is never flushed to the pane by a timeout.
    pub fn pending_escape(&self) -> bool {
        matches!(self.state, State::NEsc | State::AEsc)
    }

    /// The ESCDELAY timer fired with no continuation. In normal mode (`NEsc`) flush
    /// the lone `ESC` to the pane as a standalone Escape; in command mode (`AEsc`)
    /// cancel the pending prefix. In copy mode (`copy_mode == true`), a pending lone
    /// ESC becomes `InputEvent::CopyKey(CopyKey::Ctrl(0x1b))` (Exit) instead of
    /// leaking to the pane. Resets to ground. Only ever called while
    /// `pending_escape()` holds, so committed sequences are untouched.
    pub fn flush_escape_timeout(&mut self, copy_mode: bool) -> Vec<InputEvent> {
        let mut out = Vec::new();
        match self.state {
            State::NEsc => {
                if copy_mode {
                    // In copy mode a standalone ESC exits; never leak it to the pane.
                    out.push(InputEvent::CopyKey(crate::copymode::CopyKey::Ctrl(0x1b)));
                    self.pending.clear();
                } else if !self.pending.is_empty() {
                    out.push(InputEvent::Pane(std::mem::take(&mut self.pending)));
                }
            }
            State::AEsc => {
                self.pending.clear();
            }
            _ => {}
        }
        self.state = State::Ground;
        out
    }

    /// Flush the Ground-mode accumulator as a Pane event if non-empty.
    fn flush_ground(&mut self, out: &mut Vec<InputEvent>) {
        if !self.ground_buf.is_empty() {
            out.push(InputEvent::Pane(std::mem::take(&mut self.ground_buf)));
        }
    }

    fn step(&mut self, config: &Config, b: u8, copy_mode: bool, out: &mut Vec<InputEvent>) {
        match self.state {
            State::Ground => {
                if copy_mode {
                    // In copy mode: never arm the prefix, never accumulate pane bytes.
                    // Control bytes → CopyKey::Ctrl; ESC → enter NEsc for CSI decoding;
                    // printable bytes → CopyKey::Char.
                    self.flush_ground(out);
                    if b == ESC {
                        // May begin a CSI (arrows / PageUp / Home/End). Buffer and
                        // let the NEsc/NCsi machinery decode it, but in copy mode
                        // the terminal final bytes resolve to CopyKey (see below).
                        self.pending.clear();
                        self.pending.push(ESC);
                        self.state = State::NEsc;
                    } else if b < 0x20 {
                        out.push(InputEvent::CopyKey(crate::copymode::CopyKey::Ctrl(b)));
                    } else {
                        out.push(InputEvent::CopyKey(crate::copymode::CopyKey::Char(b)));
                    }
                } else if config.prefix.matches(&Key::Char(b)) {
                    self.flush_ground(out);
                    self.state = State::Armed;
                } else if b == ESC {
                    self.flush_ground(out);
                    self.pending.clear();
                    self.pending.push(ESC);
                    self.state = State::NEsc;
                } else {
                    // Accumulate plain bytes; flushed as one Pane chunk.
                    self.ground_buf.push(b);
                }
            }
            State::NEsc => {
                if b == b'[' {
                    self.pending.push(b);
                    self.state = State::NCsi;
                } else if copy_mode {
                    // In copy mode, a lone ESC not followed by '[' is an Exit key.
                    // Emit the CopyKey for the standalone ESC, reset to Ground, and
                    // reprocess this byte normally.
                    out.push(InputEvent::CopyKey(crate::copymode::CopyKey::Char(0x1b)));
                    self.pending.clear();
                    self.state = State::Ground;
                    self.step(config, b, copy_mode, out);
                } else {
                    // ESC was standalone or began a non-mouse sequence: flush the
                    // buffered ESC to the pane, then reprocess this byte in ground.
                    out.push(InputEvent::Pane(std::mem::take(&mut self.pending)));
                    self.state = State::Ground;
                    self.step(config, b, copy_mode, out);
                }
            }
            State::NCsi => {
                // The SGR mouse introducer is `ESC [ <`; `<` counts only as the
                // first byte after `[` (pending is exactly `[ESC, '[']`, len 2).
                // This branch is mode-agnostic: copy-mode mouse drags (ESC[<…M/m)
                // must still reach NMouse → InputEvent::Mouse.
                if b == b'<' && self.pending.len() == 2 {
                    self.pending.push(b);
                    self.state = State::NMouse;
                } else if (b == b'I' || b == b'O') && self.pending.len() == 2 {
                    // DECSET 1004 focus report: bare `ESC [ I` (FocusIn) or
                    // `ESC [ O` (FocusOut), zero parameter bytes. Intercept
                    // — do NOT push the final byte and do NOT forward to the
                    // pane. A parameterized CSI with `I`/`O` terminator
                    // (pending.len() > 2) is unaffected and falls through below.
                    self.pending.clear();
                    self.state = State::Ground;
                    if b == b'I' {
                        out.push(InputEvent::FocusIn);
                    }
                    // FocusOut emits nothing.
                } else {
                    self.pending.push(b);
                    // A CSI final byte (0x40..=0x7e) ends the sequence.
                    if (0x40..=0x7e).contains(&b) || self.pending.len() >= MAX_CSI_SEQ {
                        if copy_mode {
                            // In copy mode: map to a CopyKey; never push pane bytes.
                            if let Some(ck) = csi_to_copykey(&self.pending) {
                                out.push(InputEvent::CopyKey(ck));
                            }
                            // Unmapped CSI sequences are silently dropped in copy mode.
                        } else {
                            // Normal mode: forward the whole sequence to the pane unchanged.
                            out.push(InputEvent::Pane(std::mem::take(&mut self.pending)));
                        }
                        self.pending.clear();
                        self.state = State::Ground;
                    }
                }
            }
            State::NMouse => {
                if b == b'M' || b == b'm' {
                    self.pending.push(b);
                    if let Some(ev) = parse_sgr(&self.pending) {
                        out.push(InputEvent::Mouse(ev));
                    }
                    self.pending.clear();
                    self.state = State::Ground;
                } else if (b.is_ascii_digit() || b == b';') && self.pending.len() < MAX_MOUSE_SEQ {
                    self.pending.push(b);
                } else {
                    // Malformed or runaway SGR report (a non-param byte, or an
                    // over-long sequence). Mouse bytes are server-consumed, not pane
                    // input, so discard the partial sequence and reprocess this byte
                    // from ground — a stray byte after a corrupt prefix is not lost.
                    self.pending.clear();
                    self.state = State::Ground;
                    self.step(config, b, copy_mode, out);
                }
            }
            State::Armed => {
                if b == ESC {
                    self.pending.clear();
                    self.pending.push(ESC);
                    self.state = State::AEsc;
                } else {
                    resolve_key(config, Key::Char(b), out);
                    self.state = State::Ground;
                }
            }
            State::AEsc => {
                if b == b'[' {
                    self.state = State::ACsi;
                } else if b == b'O' {
                    self.state = State::ASs3;
                } else {
                    // Prefix + ESC + non-arrow: cancel prefix, drop the ESC, and
                    // process this byte normally.
                    self.pending.clear();
                    self.state = State::Ground;
                    self.step(config, b, copy_mode, out);
                }
            }
            State::ACsi => {
                // Focus reports `ESC [ I` / `ESC [ O` can land here when the user
                // is mid-prefix. These are side-channel terminal metadata — they
                // must NOT cancel the user's armed prefix. Intercept before the
                // shared arrow-resolution path: emit FocusIn for `I` (nothing for
                // `O`), then RETURN to State::Armed so the user's next byte
                // continues command resolution normally.
                if b == b'I' || b == b'O' {
                    self.pending.clear();
                    self.state = State::Armed;
                    if b == b'I' {
                        out.push(InputEvent::FocusIn);
                    }
                    return;
                }
                let named = match b {
                    b'A' => Some(NamedKey::Up),
                    b'B' => Some(NamedKey::Down),
                    b'C' => Some(NamedKey::Right),
                    b'D' => Some(NamedKey::Left),
                    _ => None,
                };
                self.pending.clear();
                self.state = State::Ground;
                if let Some(n) = named {
                    resolve_key(config, Key::Named(n), out);
                }
                // Non-arrow non-focus final byte after the prefix: unknown
                // command, dropped (existing behavior).
            }
            State::ASs3 => {
                // SS3 arrows from the armed state (`ESC O A/B/C/D`). Focus
                // events are CSI, not SS3, so this path never sees them.
                let named = match b {
                    b'A' => Some(NamedKey::Up),
                    b'B' => Some(NamedKey::Down),
                    b'C' => Some(NamedKey::Right),
                    b'D' => Some(NamedKey::Left),
                    _ => None,
                };
                self.pending.clear();
                self.state = State::Ground;
                if let Some(n) = named {
                    resolve_key(config, Key::Named(n), out);
                }
            }
        }
    }
}

/// Map a buffered CSI (`pending` = ESC [ … final) to a copy-mode key. Returns
/// `None` for sequences copy mode does not bind.
fn csi_to_copykey(pending: &[u8]) -> Option<crate::copymode::CopyKey> {
    use crate::copymode::CopyKey;
    let final_byte = *pending.last()?;
    // params are between "ESC [" (2 bytes) and the final byte
    let params = pending.get(2..pending.len().saturating_sub(1))?;
    match final_byte {
        b'A' => Some(CopyKey::Up),
        b'B' => Some(CopyKey::Down),
        b'C' => Some(CopyKey::Right),
        b'D' => Some(CopyKey::Left),
        b'H' => Some(CopyKey::Home),
        b'F' => Some(CopyKey::End),
        b'~' => match params {
            b"5" => Some(CopyKey::PageUp),
            b"6" => Some(CopyKey::PageDown),
            b"1" | b"7" => Some(CopyKey::Home),
            b"4" | b"8" => Some(CopyKey::End),
            _ => None,
        },
        _ => None,
    }
}

/// Resolve a command-mode key into an event: a literal prefix, a bound command, a
/// window digit, or (unknown) nothing.
fn resolve_key(config: &Config, key: Key, out: &mut Vec<InputEvent>) {
    if config.prefix.matches(&key) {
        out.push(InputEvent::Pane(config.prefix.bytes()));
        return;
    }
    if let Some(cmd) = config.resolve(&key) {
        out.push(InputEvent::Command(cmd));
        return;
    }
    if let Key::Char(b) = key {
        if b == b'0' {
            out.push(InputEvent::Command(crate::config::Command::ToggleZoom));
        } else if (b'1'..=b'9').contains(&b) {
            out.push(InputEvent::Command(crate::config::Command::SelectWindow(
                (b - b'0') as usize,
            )));
        }
    }
}

/// Parse a complete SGR mouse buffer `ESC [ < b ; x ; y (M|m)`.
fn parse_sgr(buf: &[u8]) -> Option<MouseEvent> {
    if buf.len() < 4 {
        return None;
    }
    let final_byte = *buf.last().unwrap();
    let body = &buf[3..buf.len() - 1]; // strip ESC [ < ... final
    let s = std::str::from_utf8(body).ok()?;
    let mut it = s.split(';');
    let b: u16 = it.next()?.parse().ok()?;
    let x: u16 = it.next()?.parse().ok()?;
    let y: u16 = it.next()?.parse().ok()?;
    if it.next().is_some() {
        return None;
    }
    // Ignore wheel/scroll events (Pb bit 6): out of scope this phase. Decoding them
    // as button-0/1 presses would inject spurious clicks into focus/resize handling.
    if b & 64 != 0 {
        return None;
    }
    let kind = if b & 32 != 0 {
        MouseKind::Drag
    } else if final_byte == b'M' {
        MouseKind::Press
    } else {
        MouseKind::Release
    };
    Some(MouseEvent {
        button: (b & 0b11) as u8,
        col: x,
        row: y,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Command, Config};

    fn parse_all(bytes: &[u8]) -> Vec<InputEvent> {
        let cfg = Config::default();
        let mut p = InputParser::new();
        p.feed(&cfg, bytes, false)
    }

    #[test]
    fn plain_bytes_pass_through_to_the_pane() {
        assert_eq!(parse_all(b"ls\n"), vec![InputEvent::Pane(b"ls\n".to_vec())]);
    }

    #[test]
    fn ctrl_a_now_passes_through_to_the_pane() {
        // Pins the user-facing contract: under the default config, Ctrl-A
        // (0x01) reaches the foreground pane as a plain byte (where the inner
        // CLI's readline binds it to "beginning of line"). Regression guard
        // against any future flip-back of DEFAULT_PREFIX.
        assert_eq!(parse_all(&[0x01]), vec![InputEvent::Pane(vec![0x01])]);
    }

    #[test]
    fn prefix_then_close_pane_key_yields_the_command() {
        // Ctrl-b x => ClosePane (no pane bytes).
        assert_eq!(
            parse_all(&[0x02, b'x']),
            vec![InputEvent::Command(Command::ClosePane)]
        );
    }

    #[test]
    fn prefix_then_prefix_sends_a_literal_prefix_byte() {
        assert_eq!(parse_all(&[0x02, 0x02]), vec![InputEvent::Pane(vec![0x02])]);
    }

    #[test]
    fn prefix_then_digit_selects_a_window() {
        assert_eq!(
            parse_all(&[0x02, b'2']),
            vec![InputEvent::Command(Command::SelectWindow(2))]
        );
    }

    #[test]
    fn prefix_then_zero_toggles_zoom() {
        let mut out = Vec::new();
        resolve_key(&Config::default(), Key::Char(b'0'), &mut out);
        assert_eq!(out, vec![InputEvent::Command(Command::ToggleZoom)]);
    }

    // Pins the end-to-end default path: the default keymap binds toggle_sidebar to
    // 's', so prefix + 's' routes through config.resolve (not a hardwired branch).
    #[test]
    fn prefix_then_s_toggles_sidebar() {
        assert_eq!(
            parse_all(&[0x02, b's']),
            vec![InputEvent::Command(Command::ToggleSidebar)]
        );
    }

    #[test]
    fn prefix_then_custom_key_toggles_sidebar() {
        let cfg = Config {
            keys: crate::config::KeyMap {
                toggle_sidebar: crate::config::KeySpec::Char('v'),
                ..crate::config::KeyMap::default()
            },
            ..Config::default()
        };
        let mut out = Vec::new();
        resolve_key(&cfg, Key::Char(b'v'), &mut out);
        assert_eq!(out, vec![InputEvent::Command(Command::ToggleSidebar)]);
    }

    #[test]
    fn prefix_then_unknown_key_is_dropped() {
        assert_eq!(parse_all(&[0x02, b'Z']), vec![]);
    }

    #[test]
    fn prefix_then_down_arrow_splits_horizontal() {
        // Ctrl-b ESC [ B => SplitHorizontal (default binding Down).
        assert_eq!(
            parse_all(&[0x02, 0x1b, b'[', b'B']),
            vec![InputEvent::Command(Command::SplitHorizontal)]
        );
    }

    #[test]
    fn prefix_then_ss3_right_arrow_splits_vertical() {
        // Application-cursor encoding: Ctrl-b ESC O C => SplitVertical (Right).
        assert_eq!(
            parse_all(&[0x02, 0x1b, b'O', b'C']),
            vec![InputEvent::Command(Command::SplitVertical)]
        );
    }

    #[test]
    fn normal_mode_arrow_passes_through_to_the_pane() {
        // ESC [ A (Up) with no prefix is just pane input, byte-preserved.
        let evs = parse_all(b"\x1b[A");
        let bytes: Vec<u8> = evs
            .into_iter()
            .flat_map(|e| match e {
                InputEvent::Pane(b) => b,
                _ => vec![],
            })
            .collect();
        assert_eq!(bytes, b"\x1b[A");
    }

    #[test]
    fn fragmented_prefix_arrow_across_feeds() {
        let cfg = Config::default();
        let mut p = InputParser::new();
        assert_eq!(p.feed(&cfg, &[0x02, 0x1b], false), vec![]); // Ctrl-b ESC, waiting
        assert_eq!(p.feed(&cfg, b"[", false), vec![]); // CSI, waiting
        assert_eq!(
            p.feed(&cfg, b"B", false),
            vec![InputEvent::Command(Command::SplitHorizontal)]
        );
    }

    #[test]
    fn lone_trailing_escape_is_pending_then_flushes_to_pane() {
        let cfg = Config::default();
        let mut p = InputParser::new();
        assert_eq!(p.feed(&cfg, &[0x1b], false), vec![]); // ESC alone, buffered
        assert!(p.pending_escape());
        // Timer fires: standalone Escape goes to the pane.
        assert_eq!(p.flush_escape_timeout(false), vec![InputEvent::Pane(vec![0x1b])]);
        assert!(!p.pending_escape());
    }

    #[test]
    fn sgr_mouse_press_decodes() {
        // ESC [ < 0 ; 5 ; 7 M => left press at col 5, row 7.
        let evs = parse_all(b"\x1b[<0;5;7M");
        assert_eq!(
            evs,
            vec![InputEvent::Mouse(MouseEvent {
                button: 0,
                col: 5,
                row: 7,
                kind: MouseKind::Press,
            })]
        );
    }

    #[test]
    fn sgr_mouse_drag_and_release_decode() {
        // Drag: motion bit (32) set, final 'M'.
        assert_eq!(
            parse_all(b"\x1b[<32;9;3M"),
            vec![InputEvent::Mouse(MouseEvent {
                button: 0,
                col: 9,
                row: 3,
                kind: MouseKind::Drag
            })]
        );
        // Release: final 'm'.
        assert_eq!(
            parse_all(b"\x1b[<0;9;3m"),
            vec![InputEvent::Mouse(MouseEvent {
                button: 0,
                col: 9,
                row: 3,
                kind: MouseKind::Release
            })]
        );
    }

    #[test]
    fn fragmented_mouse_across_feeds() {
        let cfg = Config::default();
        let mut p = InputParser::new();
        assert_eq!(p.feed(&cfg, b"\x1b[<0;5", false), vec![]); // partial, buffered
                                                        // A committed mouse sequence is NOT pending_escape: it waits for completion
                                                        // without a timer, so a fragmented report is never flushed to the pane.
        assert!(!p.pending_escape());
        assert_eq!(
            p.feed(&cfg, b";7M", false),
            vec![InputEvent::Mouse(MouseEvent {
                button: 0,
                col: 5,
                row: 7,
                kind: MouseKind::Press
            })]
        );
        assert!(!p.pending_escape());
    }

    #[test]
    fn esc_then_non_bracket_byte_flushes_both_to_pane() {
        // ESC then 'a' (e.g. Alt-a): both reach the pane, ESC first.
        assert_eq!(
            parse_all(b"\x1ba"),
            vec![InputEvent::Pane(vec![0x1b]), InputEvent::Pane(vec![b'a'])]
        );
    }

    #[test]
    fn option_backspace_passes_through_to_the_pane() {
        // ESC \x7f is the macOS Option+Backspace "kill word backward"
        // sequence (\x7f is DEL, the byte the terminal sends for the
        // Delete/Backspace key). Walks the same NEsc -> non-bracket path
        // as ESC + arbitrary byte (covered by
        // esc_then_non_bracket_byte_flushes_both_to_pane), but pins the
        // exact byte the user-facing contract depends on rather than
        // inferring from the generic case.
        assert_eq!(
            parse_all(b"\x1b\x7f"),
            vec![InputEvent::Pane(vec![0x1b]), InputEvent::Pane(vec![0x7f]),]
        );
    }

    #[test]
    fn malformed_mouse_sequence_is_discarded_and_recovers() {
        // ESC [ < 0 ; 5 then a bogus 'Z' aborts the mouse parse; 'Z' becomes pane input.
        let cfg = Config::default();
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(&cfg, b"\x1b[<0;5Z", false),
            vec![InputEvent::Pane(vec![b'Z'])]
        );
        assert!(!p.pending_escape());
    }

    #[test]
    fn prefix_then_csi_right_arrow_splits_vertical() {
        // CSI (non-application-cursor) encoding: Ctrl-b ESC [ C => Right => SplitVertical.
        assert_eq!(
            parse_all(&[0x02, 0x1b, b'[', b'C']),
            vec![InputEvent::Command(Command::SplitVertical)]
        );
    }

    #[test]
    fn normal_mode_delete_key_passes_through() {
        // ESC [ 3 ~ (Delete) must reach the pane as one intact sequence.
        assert_eq!(
            parse_all(b"\x1b[3~"),
            vec![InputEvent::Pane(b"\x1b[3~".to_vec())]
        );
    }

    #[test]
    fn normal_mode_modified_arrow_passes_through() {
        // ESC [ 1 ; 5 C (Ctrl+Right) — parameterized CSI, intact.
        assert_eq!(
            parse_all(b"\x1b[1;5C"),
            vec![InputEvent::Pane(b"\x1b[1;5C".to_vec())]
        );
    }

    #[test]
    fn option_arrow_left_passes_through_to_the_pane() {
        // ESC [ 1 ; 3 D is the macOS Option+Left "previous word" sequence
        // (modifier=3 means Alt/Option in xterm modifyOtherKeys semantics).
        // The generic parameterized-CSI path handles this — the modifier
        // value doesn't change the parser's behavior — but we pin the
        // exact Option-modifier bytes here so the user-facing contract is
        // asserted directly, not just inferred from the Ctrl-modifier test.
        assert_eq!(
            parse_all(b"\x1b[1;3D"),
            vec![InputEvent::Pane(b"\x1b[1;3D".to_vec())]
        );
    }

    #[test]
    fn scroll_wheel_is_ignored() {
        // ESC [ < 64 ; 5 ; 7 M (scroll up) produces no event this phase.
        assert_eq!(parse_all(b"\x1b[<64;5;7M"), vec![]);
    }

    #[test]
    fn normal_mode_focus_in_emits_focusin_no_pane_bytes() {
        // Bare ESC [ I (zero parameter bytes) is a DECSET 1004 FocusIn report,
        // not pane input. The parser intercepts and emits InputEvent::FocusIn.
        assert_eq!(parse_all(b"\x1b[I"), vec![InputEvent::FocusIn]);
    }

    #[test]
    fn normal_mode_focus_out_is_dropped() {
        // FocusOut: parser consumes the sequence and emits nothing (no promote,
        // no demote, no pane forwarding).
        assert_eq!(parse_all(b"\x1b[O"), vec![]);
    }

    #[test]
    fn normal_mode_focus_between_pane_bytes_does_not_leak() {
        // The focus bytes must not appear in any Pane event; surrounding pane
        // input (x and y) is still forwarded.
        let evs = parse_all(b"x\x1b[Iy");
        // Flatten all Pane bytes; assert no ESC byte slipped through.
        let pane_bytes: Vec<u8> = evs
            .iter()
            .flat_map(|e| match e {
                InputEvent::Pane(b) => b.clone(),
                _ => vec![],
            })
            .collect();
        assert!(
            !pane_bytes.contains(&0x1b),
            "focus bytes must not reach a Pane event; got {pane_bytes:?}"
        );
        // And we got exactly one FocusIn from the embedded sequence.
        let focus_count = evs
            .iter()
            .filter(|e| matches!(e, InputEvent::FocusIn))
            .count();
        assert_eq!(focus_count, 1);
        // Both x and y still made it through as pane bytes.
        assert!(pane_bytes.contains(&b'x'));
        assert!(pane_bytes.contains(&b'y'));
    }

    #[test]
    fn normal_mode_parameterized_csi_ending_in_i_is_not_focus() {
        // ESC [ 1 ; 2 I has parameter bytes — it's NOT a focus event, so it
        // must pass through to the pane unchanged (as a normal CSI sequence).
        let evs = parse_all(b"\x1b[1;2I");
        // No FocusIn should be emitted.
        assert!(!evs.iter().any(|e| matches!(e, InputEvent::FocusIn)));
        // The full sequence reaches the pane verbatim.
        let pane_bytes: Vec<u8> = evs
            .into_iter()
            .flat_map(|e| match e {
                InputEvent::Pane(b) => b,
                _ => vec![],
            })
            .collect();
        assert_eq!(pane_bytes, b"\x1b[1;2I");
    }

    #[test]
    fn armed_mode_focus_in_emits_focusin_and_preserves_prefix() {
        // After the user presses the prefix (Ctrl-b = 0x02), the parser is in the
        // Armed state. A focus event arriving here (ESC [ I) must:
        //   - emit one FocusIn event (so promotion happens),
        //   - preserve the Armed state (focus is side-channel terminal metadata,
        //     not user input — must not cancel the user's command intent).
        // Verified by feeding `prefix ESC [ I x` in one shot: we expect a FocusIn,
        // then the next byte `x` resolves to the bound ClosePane command, NOT a
        // literal pane byte.
        assert_eq!(
            parse_all(&[0x02, 0x1b, b'[', b'I', b'x']),
            vec![
                InputEvent::FocusIn,
                InputEvent::Command(crate::config::Command::ClosePane),
            ]
        );
    }

    #[test]
    fn armed_mode_focus_out_emits_nothing_and_preserves_prefix() {
        // FocusOut after prefix: zero focus events, prefix preserved, next `x`
        // still resolves to ClosePane.
        assert_eq!(
            parse_all(&[0x02, 0x1b, b'[', b'O', b'x']),
            vec![InputEvent::Command(crate::config::Command::ClosePane)]
        );
    }

    #[test]
    fn armed_mode_arrow_still_resolves_as_command_regression() {
        // Regression guard: the existing `prefix ESC [ B = SplitHorizontal`
        // behavior must not change. (Already covered by
        // `prefix_then_down_arrow_splits_horizontal`, but re-asserted explicitly
        // alongside the focus tests so a future ACsi refactor can't silently
        // break this in isolation.)
        assert_eq!(
            parse_all(&[0x02, 0x1b, b'[', b'B']),
            vec![InputEvent::Command(crate::config::Command::SplitHorizontal)]
        );
    }

    #[test]
    fn sgr_right_button_press_decodes_button_2() {
        // SGR mouse press, right-button (bit 0..1 == 10): b=2, col=10, row=5.
        assert_eq!(
            parse_all(b"\x1b[<2;10;5M"),
            vec![InputEvent::Mouse(MouseEvent {
                button: 2,
                col: 10,
                row: 5,
                kind: MouseKind::Press,
            })]
        );
    }

    #[test]
    fn sgr_motion_under_1003_is_decoded_as_drag_kind() {
        // SGR motion report under any-event tracking: b = 32 + base. With
        // no button held, base = 35 (specifically the "no button" sentinel
        // some terminals use). The parser already treats `b & 32 != 0` as
        // MouseKind::Drag regardless of which low bits are set.
        assert_eq!(
            parse_all(b"\x1b[<35;10;5M"),
            vec![InputEvent::Mouse(MouseEvent {
                button: 3,
                col: 10,
                row: 5,
                kind: MouseKind::Drag,
            })]
        );
    }

    #[test]
    fn parse_sgr_motion_does_not_reject_button_released_motion() {
        // The same bytes as above must NOT be rejected (return Vec::new()) by
        // parse_sgr — they must reach the run loop as a Mouse event.
        let events = parse_all(b"\x1b[<35;10;5M");
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::Mouse(_)));
    }

    // ── Copy-mode tests ──────────────────────────────────────────────────────

    fn parse_copy(bytes: &[u8]) -> Vec<InputEvent> {
        let cfg = Config::default();
        let mut p = InputParser::new();
        p.feed(&cfg, bytes, /*copy_mode*/ true)
    }

    #[test]
    fn copy_mode_ctrl_b_does_not_arm_prefix_and_surfaces_as_copykey() {
        use crate::copymode::CopyKey;
        assert_eq!(parse_copy(&[0x02]), vec![InputEvent::CopyKey(CopyKey::Ctrl(0x02))]);
    }

    #[test]
    fn copy_mode_plain_char_is_a_copykey_not_pane_bytes() {
        use crate::copymode::CopyKey;
        assert_eq!(parse_copy(b"j"), vec![InputEvent::CopyKey(CopyKey::Char(b'j'))]);
    }

    #[test]
    fn copy_mode_arrow_and_pageup_decode() {
        use crate::copymode::CopyKey;
        assert_eq!(parse_copy(b"\x1b[A"), vec![InputEvent::CopyKey(CopyKey::Up)]);
        assert_eq!(parse_copy(b"\x1b[5~"), vec![InputEvent::CopyKey(CopyKey::PageUp)]);
        assert_eq!(parse_copy(b"\x1b[6~"), vec![InputEvent::CopyKey(CopyKey::PageDown)]);
        assert_eq!(parse_copy(b"\x1b[H"), vec![InputEvent::CopyKey(CopyKey::Home)]);
        assert_eq!(parse_copy(b"\x1b[F"), vec![InputEvent::CopyKey(CopyKey::End)]);
    }

    #[test]
    fn normal_mode_prefix_still_arms_when_copy_mode_inactive_regression() {
        // copy_mode = false: the existing prefix behavior is byte-identical.
        let cfg = Config::default();
        let mut p = InputParser::new();
        assert_eq!(
            p.feed(&cfg, &[0x02, b'x'], false),
            vec![InputEvent::Command(crate::config::Command::ClosePane)]
        );
    }

    #[test]
    fn copy_mode_lone_esc_at_chunk_end_flushes_as_exit_not_pane() {
        use crate::copymode::CopyKey;
        let cfg = Config::default();
        let mut p = InputParser::new();
        // ESC as the final byte: parser parks in NEsc, emitting nothing yet.
        assert!(p.feed(&cfg, b"\x1b", true).is_empty());
        // The timer fires: in copy mode this must be an Exit CopyKey, NOT Pane(ESC).
        assert_eq!(p.flush_escape_timeout(true), vec![InputEvent::CopyKey(CopyKey::Ctrl(0x1b))]);
    }

    #[test]
    fn copy_mode_sgr_mouse_still_reaches_nmouse() {
        // SGR mouse drags in copy mode (ESC[<…M/m) must still decode as Mouse events,
        // not be eaten or mapped to CopyKey — so the copy-mode mouse-drag handler
        // (Task 12) can receive them.
        let evs = parse_copy(b"\x1b[<0;3;2M");
        assert_eq!(evs.len(), 1, "expected exactly one event, got {evs:?}");
        assert!(
            matches!(evs[0], InputEvent::Mouse(_)),
            "expected Mouse event, got {evs:?}"
        );
    }
}
