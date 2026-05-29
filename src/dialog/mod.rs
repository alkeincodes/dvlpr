//! Pure model for the window-name input dialog (New Window / Rename Window).
//! No I/O, no async, no `Session` dependency. Driven by `Session`, which owns
//! `Option<WindowNameDialog>` and routes key events into it.

/// Which flow the dialog is driving.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogMode {
    /// Creating a window. Submit with an empty buffer → default/auto name.
    NewWindow,
    /// Renaming an existing window (0-based index). Submit empty → un-pin.
    RenameWindow { window: usize },
}

/// Open-dialog state. Minimal editing: append printable chars, backspace,
/// cursor pinned at end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowNameDialog {
    pub mode: DialogMode,
    pub buffer: String,
}

impl WindowNameDialog {
    /// New Window dialog, empty buffer.
    pub fn new_window() -> Self {
        Self { mode: DialogMode::NewWindow, buffer: String::new() }
    }

    /// Rename dialog for `window`, pre-filled with the current `name`.
    pub fn rename(window: usize, name: &str) -> Self {
        Self { mode: DialogMode::RenameWindow { window }, buffer: name.to_string() }
    }

    /// Append a printable character. Control characters are ignored by the
    /// caller, so this trusts its input.
    pub fn insert_char(&mut self, c: char) {
        self.buffer.push(c);
    }

    /// Delete the last character, if any.
    pub fn backspace(&mut self) {
        self.buffer.pop();
    }

    /// The trimmed submit value.
    pub fn value(&self) -> &str {
        self.buffer.trim()
    }

    /// Title shown in the dialog border.
    pub fn title(&self) -> &'static str {
        match self.mode {
            DialogMode::NewWindow => "New Window",
            DialogMode::RenameWindow { .. } => "Rename Window",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_window_starts_empty_with_title() {
        let d = WindowNameDialog::new_window();
        assert_eq!(d.value(), "");
        assert_eq!(d.title(), "New Window");
        assert_eq!(d.mode, DialogMode::NewWindow);
    }

    #[test]
    fn rename_prefills_buffer_and_carries_window() {
        let d = WindowNameDialog::rename(2, "zsh");
        assert_eq!(d.value(), "zsh");
        assert_eq!(d.title(), "Rename Window");
        assert_eq!(d.mode, DialogMode::RenameWindow { window: 2 });
    }

    #[test]
    fn insert_and_backspace_edit_the_buffer() {
        let mut d = WindowNameDialog::new_window();
        d.insert_char('a');
        d.insert_char('b');
        d.insert_char('c');
        d.backspace();
        assert_eq!(d.value(), "ab");
    }

    #[test]
    fn value_trims_surrounding_whitespace() {
        let mut d = WindowNameDialog::new_window();
        for c in "  hi  ".chars() { d.insert_char(c); }
        assert_eq!(d.value(), "hi");
    }
}
