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
        // Inline-code flags (`python -c`, `node -e`, `--eval`, …) take the NEXT arg as
        // source code, not a script path. Naming the window from that would render
        // arbitrary code — possibly secrets — into the status bar shown to every
        // attached client, so fall back to the runtime name when we see one. A bare
        // flag like `--inspect` is not an eval flag: skip it and keep scanning for the
        // script path (e.g. `node --inspect app.js` → `app`).
        const EVAL_FLAGS: &[&str] = &["-c", "-e", "--eval", "-p", "--print"];
        for a in &argv[1..] {
            if EVAL_FLAGS.contains(&a.as_str()) {
                return Some(exe.to_string());
            }
            if !a.starts_with('-') {
                return Some(strip_js_ext(basename(a)).to_string());
            }
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
    if argc <= 0 || argc > 65_536 {
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

/// Resolve a pid's current working directory, or `None` on any failure
/// (EPERM, ESRCH, permission denied, unsupported platform).
#[cfg(target_os = "macos")]
pub fn pid_cwd(pid: i32) -> Option<std::path::PathBuf> {
    // Use proc_pidinfo(PROC_PIDVNODEPATHINFO) — the standard macOS API for
    // resolving a process's cwd. libc exposes the required types and function.
    use std::ffi::CStr;
    use std::mem;
    let mut info: libc::proc_vnodepathinfo = unsafe { mem::zeroed() };
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int,
        )
    };
    if ret <= 0 {
        return None;
    }
    // SAFETY: pvi_cdir.vip_path is [[c_char; 32]; 32] = 1024 bytes, laid out
    // contiguously in memory.  The kernel ABI for PROC_PIDVNODEPATHINFO
    // (Apple's sys/proc_info.h) guarantees the path is NUL-terminated, so
    // CStr::from_ptr below will always find a terminator within the array.
    let flat: &[libc::c_char; 1024] = unsafe {
        &*(&info.pvi_cdir.vip_path as *const [[libc::c_char; 32]; 32]
            as *const [libc::c_char; 1024])
    };
    let cstr = unsafe { CStr::from_ptr(flat.as_ptr()) };
    let path = std::path::PathBuf::from(cstr.to_string_lossy().into_owned());
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

#[cfg(target_os = "linux")]
pub fn pid_cwd(pid: i32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn pid_cwd(_pid: i32) -> Option<std::path::PathBuf> {
    None
}

use std::path::PathBuf;

/// If `path` is an agent transcript of `kind`, return its session uuid.
/// Claude: `**/.claude/projects/**/<uuid>.jsonl` (filename stem is the uuid).
/// Codex:  `**/.codex/sessions/**/rollout-*-<uuid>.jsonl` (trailing uuid).
pub fn transcript_id_for(path: &str, kind: crate::detect::Agent) -> Option<String> {
    if !path.ends_with(".jsonl") {
        return None;
    }
    let stem = std::path::Path::new(path).file_stem()?.to_str()?;
    match kind {
        crate::detect::Agent::Claude => {
            if !path.contains("/.claude/projects/") {
                return None;
            }
            crate::persist::is_canonical_uuid(stem).then(|| stem.to_string())
        }
        crate::detect::Agent::Codex => {
            if !path.contains("/.codex/sessions/") || !stem.starts_with("rollout-") {
                return None;
            }
            // The uuid is the trailing 5 hyphen-groups of the stem.
            let tail: Vec<&str> = stem.rsplit('-').take(5).collect();
            if tail.len() < 5 {
                return None;
            }
            let uuid: String = tail.into_iter().rev().collect::<Vec<_>>().join("-");
            crate::persist::is_canonical_uuid(&uuid).then_some(uuid)
        }
    }
}

/// Read a `/proc/<pid>/fd`-style directory, returning resolved symlink targets.
#[cfg(any(target_os = "linux", test))]
pub fn scan_fd_dir(fd_dir: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(fd_dir) {
        for e in entries.flatten() {
            if let Ok(target) = std::fs::read_link(e.path()) {
                out.push(target.to_string_lossy().to_string());
            }
        }
    }
    out
}

/// Discover the agent's open transcript: returns (full path, uuid). `None` if the
/// process holds no matching transcript or discovery is unavailable.
pub fn agent_transcript(pid: i32, kind: crate::detect::Agent) -> Option<(PathBuf, String)> {
    let paths = open_files(pid);
    paths.into_iter().find_map(|p| {
        transcript_id_for(&p, kind).map(|id| (PathBuf::from(p), id))
    })
}

#[cfg(target_os = "linux")]
fn open_files(pid: i32) -> Vec<String> {
    scan_fd_dir(std::path::Path::new(&format!("/proc/{pid}/fd")))
}

#[cfg(target_os = "macos")]
fn open_files(pid: i32) -> Vec<String> {
    // Net-new libproc FFI: PROC_PIDLISTFDS to enumerate fds, then
    // proc_pidfdinfo(PROC_PIDFDVNODEPATHINFO) per vnode fd to resolve its path.
    use std::os::raw::c_void;
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;
    const PROX_FDTYPE_VNODE: u32 = 1;
    // proc_fdinfo: { proc_fd: i32, proc_fdtype: u32 }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ProcFdInfo {
        proc_fd: i32,
        proc_fdtype: u32,
    }
    // vnode_fdinfowithpath = { proc_fileinfo pfi; vnode_info_path pvip; } where
    // vnode_info_path = { vnode_info vip_vi; char vip_path[MAXPATHLEN]; }. The path
    // is the TRAILING char[MAXPATHLEN] field. Sizes verified against
    // <sys/proc_info.h> on this macOS (aarch64) host:
    //   sizeof(proc_fileinfo)=24, sizeof(vnode_info)=152, MAXPATHLEN=1024
    //   => sizeof(vnode_fdinfowithpath) = 24 + 152 + 1024 = 1200
    //   => vip_path offset = 24 + 152 = 176 = 1200 - 1024
    const VNODE_INFO_SIZE: usize = 1200; // sizeof(vnode_fdinfowithpath) on macOS
    const PATH_OFFSET: usize = VNODE_INFO_SIZE - libc::PATH_MAX as usize; // trailing char[MAXPATHLEN]

    let mut out = Vec::new();
    unsafe {
        let needed = libc::proc_pidinfo(pid, PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
        if needed <= 0 {
            return out;
        }
        let count = needed as usize / std::mem::size_of::<ProcFdInfo>();
        let mut fds = vec![ProcFdInfo { proc_fd: 0, proc_fdtype: 0 }; count];
        let got = libc::proc_pidinfo(
            pid,
            PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr() as *mut c_void,
            needed,
        );
        if got <= 0 {
            return out;
        }
        let n = got as usize / std::mem::size_of::<ProcFdInfo>();
        for fd in fds.iter().take(n) {
            if fd.proc_fdtype != PROX_FDTYPE_VNODE {
                continue;
            }
            let mut buf = vec![0u8; VNODE_INFO_SIZE];
            let r = libc::proc_pidfdinfo(
                pid,
                fd.proc_fd,
                PROC_PIDFDVNODEPATHINFO,
                buf.as_mut_ptr() as *mut c_void,
                VNODE_INFO_SIZE as libc::c_int,
            );
            if r <= 0 {
                continue;
            }
            // Path is a NUL-terminated C string in the trailing field.
            let path_bytes = &buf[PATH_OFFSET..];
            let end = path_bytes.iter().position(|&b| b == 0).unwrap_or(0);
            if end > 0 {
                if let Ok(s) = std::str::from_utf8(&path_bytes[..end]) {
                    out.push(s.to_string());
                }
            }
        }
    }
    out
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_files(_pid: i32) -> Vec<String> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::{friendly_name, parse_procargs2};

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn transcript_id_extracts_claude_uuid_from_path() {
        let p = "/home/u/.claude/projects/-home-u-proj/abcdef01-2345-6789-abcd-ef0123456789.jsonl";
        assert_eq!(
            transcript_id_for(p, crate::detect::Agent::Claude),
            Some("abcdef01-2345-6789-abcd-ef0123456789".to_string())
        );
        // Wrong agent shape → None.
        assert_eq!(transcript_id_for(p, crate::detect::Agent::Codex), None);
    }

    #[test]
    fn transcript_id_extracts_codex_uuid_from_rollout_path() {
        let p = "/home/u/.codex/sessions/2026/03/28/rollout-2026-03-28T03-24-32-019d30c1-5e55-74f1-84bc-e8a8c3b3024c.jsonl";
        assert_eq!(
            transcript_id_for(p, crate::detect::Agent::Codex),
            Some("019d30c1-5e55-74f1-84bc-e8a8c3b3024c".to_string())
        );
        assert_eq!(transcript_id_for(p, crate::detect::Agent::Claude), None);
    }

    #[test]
    fn transcript_id_rejects_non_transcript_paths() {
        assert_eq!(transcript_id_for("/dev/null", crate::detect::Agent::Claude), None);
        assert_eq!(transcript_id_for("/home/u/.claude/projects/x/notes.txt", crate::detect::Agent::Claude), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn agent_transcript_finds_open_jsonl_via_synthetic_fd_dir() {
        // We can't easily fake /proc, so exercise the fd-scan helper directly with a
        // directory of symlinks pointing at staged transcript files.
        let dir = tempfile::tempdir().unwrap();
        let fddir = dir.path().join("fd");
        std::fs::create_dir_all(&fddir).unwrap();
        let target = dir.path().join("abcdef01-2345-6789-abcd-ef0123456789.jsonl");
        std::fs::write(&target, b"x").unwrap();
        // Place it under a claude-shaped path via a parent symlink chain is overkill;
        // instead point the matcher at the resolved target path string directly.
        std::os::unix::fs::symlink(&target, fddir.join("7")).unwrap();
        let claude_path = format!(
            "/x/.claude/projects/p/abcdef01-2345-6789-abcd-ef0123456789.jsonl"
        );
        // scan_fd_dir returns resolved link targets; assert it reads the symlink.
        let links = scan_fd_dir(&fddir);
        assert!(links.iter().any(|l| l.ends_with("abcdef01-2345-6789-abcd-ef0123456789.jsonl")));
        // And the matcher turns a claude-shaped path into the uuid.
        assert_eq!(
            transcript_id_for(&claude_path, crate::detect::Agent::Claude),
            Some("abcdef01-2345-6789-abcd-ef0123456789".to_string())
        );
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
    fn inline_code_flags_fall_back_to_runtime_name_not_the_code() {
        // -c/-e/--eval/... take the next arg as SOURCE CODE; never display it (it may
        // contain secrets) and never let it leak into the status bar.
        assert_eq!(
            friendly_name(&s(&["python3", "-c", "print('token=hunter2')"])),
            Some("python3".into())
        );
        assert_eq!(
            friendly_name(&s(&["node", "-e", "console.log(SECRET)"])),
            Some("node".into())
        );
        assert_eq!(
            friendly_name(&s(&["ruby", "--eval", "puts ENV['KEY']"])),
            Some("ruby".into())
        );
        // A non-eval flag is still skipped on the way to the real script path.
        assert_eq!(
            friendly_name(&s(&["node", "--inspect", "/x/app.js"])),
            Some("app".into())
        );
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

    #[test]
    fn parse_procargs2_handles_argc_larger_than_present_args() {
        // argc claims 5, but only 2 argv strings are present.
        let mut buf = Vec::new();
        buf.extend_from_slice(&5i32.to_ne_bytes());
        buf.extend_from_slice(b"/usr/bin/node\0"); // exec_path
        buf.extend_from_slice(b"\0"); // padding
        buf.extend_from_slice(b"node\0");
        buf.extend_from_slice(b"app.js\0");
        let argv = parse_procargs2(&buf).unwrap();
        assert_eq!(argv, vec!["node".to_string(), "app.js".to_string()]);
    }

    #[test]
    fn agent_transcript_finds_a_file_this_process_holds_open() {
        // Stage a claude-shaped transcript path and keep it open in THIS process, then
        // ask agent_transcript to find it among our own open fds. This validates the
        // real per-platform open-fd enumeration (Linux /proc, macOS libproc offsets).
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join(".claude/projects/p");
        std::fs::create_dir_all(&proj).unwrap();
        let uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        let file = proj.join(format!("{uuid}.jsonl"));
        // Hold the file open for the duration of the call.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&file)
            .unwrap();
        let me = std::process::id() as i32;
        let found = agent_transcript(me, crate::detect::Agent::Claude)
            .expect("agent_transcript must discover the .jsonl this process holds open");
        // Compare canonicalized paths — macOS libproc may report /private/var while
        // tempfile gives /var (both resolve to the same file). UUID is checked exactly.
        assert_eq!(found.1, uuid, "extracted uuid must match exactly");
        assert_eq!(
            std::fs::canonicalize(&found.0).unwrap(),
            std::fs::canonicalize(&file).unwrap(),
            "discovered path must resolve to the held-open file"
        );
        // A non-matching kind finds nothing.
        assert_eq!(agent_transcript(me, crate::detect::Agent::Codex), None);
    }

    #[test]
    fn pid_cwd_resolves_to_the_process_working_directory() {
        use std::process::{Command, Stdio};
        use std::thread::sleep;
        use std::time::Duration;

        let tmp = tempfile::tempdir().expect("tempdir");
        let want = std::fs::canonicalize(tmp.path()).expect("canonicalize");

        // Spawn `sleep 5` with cwd = tempdir; capture its pid.
        let mut child = Command::new("sleep")
            .arg("5")
            .current_dir(&want)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;

        // Give the OS a moment to update the process table.
        sleep(Duration::from_millis(50));

        let got = super::pid_cwd(pid);

        // Cleanup BEFORE asserting so a failure doesn't leak the child.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait();

        assert_eq!(got, Some(want));
    }
}
