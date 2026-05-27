//! Resolve a process id to a short, human-friendly name for the status bar.
//! `friendly_name`/`parse_procargs2` are pure and unit-tested; `argv_of` is the
//! platform-specific argv fetch (macOS sysctl, Linux /proc, else None). Every
//! failure path returns `None` — callers keep the previous name.

/// Language runtimes whose first non-flag argument is the "real" program.
const RUNTIMES: &[&str] = &["node", "python", "python3", "ruby", "deno", "bun"];
/// Extensions stripped from a JS entrypoint so `claude.js` reads as `claude`.
const JS_EXTS: &[&str] = &[".js", ".mjs", ".cjs"];

/// Substring after the last `/` (the whole string if there is none).
fn basename(s: &str) -> &str {
    match s.rfind('/') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

fn strip_js_ext(name: &str) -> &str {
    for ext in JS_EXTS {
        if let Some(stripped) = name.strip_suffix(ext) {
            return stripped;
        }
    }
    name
}

/// Pure: pick a display name from an argv vector. See the spec's "Proc name
/// resolution" for the rules. Returns `None` for an empty/degenerate argv.
pub fn friendly_name(argv: &[String]) -> Option<String> {
    let arg0 = argv.first()?;
    // A login shell reports argv[0] like "-zsh"; strip the leading dash(es).
    let exe = basename(arg0).trim_start_matches('-');
    if exe.is_empty() {
        return None;
    }
    if RUNTIMES.contains(&exe) {
        if let Some(script) = argv[1..].iter().find(|a| !a.starts_with('-')) {
            return Some(strip_js_ext(basename(script)).to_string());
        }
    }
    Some(exe.to_string())
}

/// Pure: parse a macOS `KERN_PROCARGS2` buffer into argv. Layout is
/// `[argc: i32][exec_path\0][\0 padding][argv0\0]..[argv{argc-1}\0][env..]`.
/// Returns `None` for a short/garbled buffer or `argc <= 0`.
pub fn parse_procargs2(buf: &[u8]) -> Option<Vec<String>> {
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if argc <= 0 {
        return None;
    }
    let mut i = 4usize;
    while i < buf.len() && buf[i] != 0 {
        i += 1; // skip exec_path
    }
    while i < buf.len() && buf[i] == 0 {
        i += 1; // skip NUL padding
    }
    let mut argv = Vec::with_capacity(argc as usize);
    for _ in 0..argc {
        if i >= buf.len() {
            break;
        }
        let start = i;
        while i < buf.len() && buf[i] != 0 {
            i += 1;
        }
        argv.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
        i += 1; // skip the terminating NUL
    }
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// macOS: fetch argv via `sysctl([CTL_KERN, KERN_PROCARGS2, pid])`. Any error
/// (EPERM for another user's process, ESRCH if it exited, truncation) → None.
#[cfg(target_os = "macos")]
pub fn argv_of(pid: i32) -> Option<Vec<String>> {
    use std::ptr;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // First call: ask for the required buffer size.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            ptr::null_mut(),
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size == 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(size);
    parse_procargs2(&buf)
}

/// Linux: argv is the NUL-separated `/proc/<pid>/cmdline`.
#[cfg(target_os = "linux")]
pub fn argv_of(pid: i32) -> Option<Vec<String>> {
    let data = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> = data
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Other platforms: no argv source.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn argv_of(_pid: i32) -> Option<Vec<String>> {
    None
}

/// Resolve a pid to a friendly name, or `None` on any failure.
pub fn process_name(pid: i32) -> Option<String> {
    friendly_name(&argv_of(pid)?)
}

#[cfg(test)]
mod tests {
    use super::{friendly_name, parse_procargs2};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn friendly_name_cases() {
        assert_eq!(friendly_name(&s(&["zsh"])), Some("zsh".into()));
        assert_eq!(friendly_name(&s(&["/bin/bash"])), Some("bash".into()));
        assert_eq!(friendly_name(&s(&["-zsh"])), Some("zsh".into())); // login shell
        assert_eq!(
            friendly_name(&s(&["node", "/Users/x/lib/claude.js"])),
            Some("claude".into())
        );
        assert_eq!(
            friendly_name(&s(&["node", "--inspect", "/x/codex.js"])),
            Some("codex".into())
        );
        assert_eq!(
            friendly_name(&s(&["node", "/x/bin/claude"])),
            Some("claude".into())
        );
        assert_eq!(
            friendly_name(&s(&["python3", "app.py"])),
            Some("app.py".into())
        );
        assert_eq!(friendly_name(&s(&["node"])), Some("node".into()));
        assert_eq!(friendly_name(&[]), None);
        assert_eq!(friendly_name(&s(&["-"])), None); // basename empty after dash strip
    }

    #[test]
    fn parse_procargs2_skips_exec_path_and_nul_padding() {
        // Layout: [argc=2][exec_path\0][\0 padding][argv0\0][argv1\0][env...]
        let mut buf = Vec::new();
        buf.extend_from_slice(&2i32.to_ne_bytes());
        buf.extend_from_slice(b"/usr/bin/node\0"); // exec_path (skipped)
        buf.extend_from_slice(b"\0\0"); // NUL padding
        buf.extend_from_slice(b"node\0"); // argv[0]
        buf.extend_from_slice(b"/x/claude.js\0"); // argv[1]
        buf.extend_from_slice(b"PATH=/usr/bin\0"); // env (must NOT be collected)
        let argv = parse_procargs2(&buf).unwrap();
        assert_eq!(argv, vec!["node".to_string(), "/x/claude.js".to_string()]);
    }

    #[test]
    fn parse_procargs2_rejects_short_buffer() {
        assert_eq!(parse_procargs2(&[1, 2]), None);
        assert_eq!(parse_procargs2(&0i32.to_ne_bytes()), None); // argc 0
    }
}
