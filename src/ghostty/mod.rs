//! FFI bindings to the vendored MIT libghostty-vt C API.
//!
//! Phase 2a exposes the raw bindgen-generated symbols only (build plumbing).
//! A safe wrapper (`GhosttyScreen`) lands in Phase 2b.

#[allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/ghostty_bindings.rs"));
}

#[cfg(test)]
mod smoke {
    use super::sys;
    use std::mem;
    use std::ptr;

    /// End-to-end: create a terminal, feed "hi", read back that cell (0,0) is 'h'.
    /// Proves the vendored lib compiles, links, and is callable over FFI.
    #[test]
    fn feed_hi_reads_back_h() {
        unsafe {
            // 1. Create the terminal (80x24, no scrollback).
            // `GhosttyTerminal` is itself a pointer typedef (`*mut GhosttyTerminalImpl`),
            // so the out-var is a single pointer and `ghostty_terminal_new` takes `&mut term`.
            let mut term: sys::GhosttyTerminal = ptr::null_mut();
            let opts = sys::GhosttyTerminalOptions {
                cols: 80,
                rows: 24,
                max_scrollback: 0,
            };
            let rc = sys::ghostty_terminal_new(ptr::null(), &mut term, opts);
            assert_eq!(rc, 0, "ghostty_terminal_new failed: {rc}");
            assert!(!term.is_null());

            // 2. Feed bytes (vt_write never fails).
            let data = b"hi";
            sys::ghostty_terminal_vt_write(term, data.as_ptr(), data.len());

            // 3. Read cell (0,0).
            // A zeroed GhosttyPoint = tag ACTIVE (0) at coordinate (0,0).
            // `GhosttyPoint` has no direct x/y fields — coords live under `value`
            // (a GhosttyPointValue union) — so do NOT try to set them; zeroed is correct.
            let point: sys::GhosttyPoint = mem::zeroed();

            // GhosttyGridRef is a "sized struct": the `size` field must be set to
            // sizeof(GhosttyGridRef) before passing to any API that writes into it.
            // This mirrors the C macro `GHOSTTY_INIT_SIZED(GhosttyGridRef)`.
            let mut gref: sys::GhosttyGridRef = mem::zeroed();
            gref.size = mem::size_of::<sys::GhosttyGridRef>();

            let rc = sys::ghostty_terminal_grid_ref(term, point, &mut gref);
            assert_eq!(rc, 0, "grid_ref failed: {rc}");

            // ghostty_grid_ref_cell writes the opaque u64 cell value into out_cell.
            let mut cell: sys::GhosttyCell = 0;
            let rc = sys::ghostty_grid_ref_cell(&gref, &mut cell);
            assert_eq!(rc, 0, "grid_ref_cell failed: {rc}");

            // 4. Decode the codepoint from the opaque cell.
            // The prefixed constant name is required (bindgen-generated).
            let mut codepoint: u32 = 0;
            let rc = sys::ghostty_cell_get(
                cell,
                sys::GhosttyCellData_GHOSTTY_CELL_DATA_CODEPOINT,
                (&raw mut codepoint).cast(),
            );
            assert_eq!(rc, 0, "cell_get failed: {rc}");
            assert_eq!(
                char::from_u32(codepoint),
                Some('h'),
                "expected 'h', got U+{codepoint:04X}"
            );

            // 5. Free.
            sys::ghostty_terminal_free(term);
        }
    }
}
