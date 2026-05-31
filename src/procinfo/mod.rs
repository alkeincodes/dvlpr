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

/// Resolve `$HOME` for locating agent transcript dirs (env-driven so the daemon
/// honors the user's real home; `agent_transcript_in` takes it as a param for tests).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Claude derives its project-dir name from the cwd by replacing every `/` and `.`
/// with `-` (e.g. `/a/b/.c` -> `-a-b--c`). Verified against real
/// `~/.claude/projects/` directory names.
fn claude_project_dirname(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

/// Newest `*.jsonl` (by mtime) directly inside `dir`, if any.
fn newest_jsonl(dir: &std::path::Path) -> Option<PathBuf> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(m) = e.metadata().and_then(|md| md.modified()) else {
            continue;
        };
        if newest.as_ref().map(|(t, _)| m > *t).unwrap_or(true) {
            newest = Some((m, p));
        }
    }
    newest.map(|(_, p)| p)
}

/// Discover the LIVE transcript for an agent running in `cwd`, returning
/// `(full path, session uuid)`. Agents open-append-CLOSE their transcripts — they
/// do NOT hold the fd open — so we locate the file by directory + recency, not by
/// enumerating open file descriptors. (Verified empirically: a live `claude` never
/// holds its `.jsonl` open.) This is how `claude --resume` itself scopes by cwd.
pub fn agent_transcript(cwd: &str, kind: crate::detect::Agent) -> Option<(PathBuf, String)> {
    agent_transcript_in(&home_dir()?, cwd, kind)
}

/// Home-injected core of `agent_transcript` (testable without mutating `$HOME`).
fn agent_transcript_in(
    home: &std::path::Path,
    cwd: &str,
    kind: crate::detect::Agent,
) -> Option<(PathBuf, String)> {
    match kind {
        // Claude: `~/.claude/projects/<munged-cwd>/` → newest `<uuid>.jsonl`.
        crate::detect::Agent::Claude => {
            let dir = home
                .join(".claude/projects")
                .join(claude_project_dirname(cwd));
            let path = newest_jsonl(&dir)?;
            let id = transcript_id_for(&path.to_string_lossy(), kind)?;
            Some((path, id))
        }
        // Codex sessions are date-foldered, not cwd-foldered: find the most-recent
        // `rollout-*.jsonl` whose first-line `payload.cwd` matches `cwd`.
        crate::detect::Agent::Codex => codex_transcript_for_cwd(home, cwd),
    }
}

/// Most-recent codex rollout whose recorded cwd equals `cwd`. Bounded scan: the
/// newest ~64 rollout files by mtime.
fn codex_transcript_for_cwd(home: &std::path::Path, cwd: &str) -> Option<(PathBuf, String)> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    collect_rollouts(&home.join(".codex/sessions"), 0, &mut files);
    files.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
    files.into_iter().take(64).find_map(|(_, path)| {
        let (c, id) = codex_head_cwd_id(&path)?;
        (c == cwd).then_some((path, id))
    })
}

/// Recurse at most `YYYY/MM/DD` (3 levels) collecting `rollout-*.jsonl` + mtimes.
fn collect_rollouts(
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<(std::time::SystemTime, PathBuf)>,
) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if depth < 3 {
                collect_rollouts(&e.path(), depth + 1, out);
            }
        } else if let Some(name) = e.path().file_name().and_then(|n| n.to_str()) {
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                if let Ok(m) = e.metadata().and_then(|md| md.modified()) {
                    out.push((m, e.path()));
                }
            }
        }
    }
}

/// Pull `(payload.cwd, payload.id)` from a codex rollout's first JSONL line.
fn codex_head_cwd_id(path: &std::path::Path) -> Option<(String, String)> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(f).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let p = v.get("payload")?;
    let cwd = p.get("cwd")?.as_str()?.to_string();
    let id = p.get("id")?.as_str()?.to_string();
    crate::persist::is_canonical_uuid(&id).then_some((cwd, id))
}

/// File creation time, falling back to mtime where the FS/kernel doesn't record a
/// birth time (e.g. some Linux filesystems). Used to pair concurrent same-cwd
/// agents to distinct transcripts by recency-of-creation.
fn entry_birth(md: &std::fs::Metadata) -> Option<std::time::SystemTime> {
    md.created().or_else(|_| md.modified()).ok()
}

/// EVERY candidate transcript for an agent of `kind` running in `cwd`, as
/// `(path, session uuid, birth time)`. Unlike `agent_transcript` (newest only),
/// this returns all matches so the caller can disambiguate two agents sharing a
/// cwd — pairing each pane to its OWN session by birth-time ↔ process-start-time.
pub fn agent_transcripts(
    cwd: &str,
    kind: crate::detect::Agent,
) -> Vec<(PathBuf, String, std::time::SystemTime)> {
    home_dir()
        .map(|h| agent_transcripts_in(&h, cwd, kind))
        .unwrap_or_default()
}

/// Home-injected core of `agent_transcripts` (testable without mutating `$HOME`).
fn agent_transcripts_in(
    home: &std::path::Path,
    cwd: &str,
    kind: crate::detect::Agent,
) -> Vec<(PathBuf, String, std::time::SystemTime)> {
    match kind {
        // Claude: every `<uuid>.jsonl` directly under `~/.claude/projects/<munged-cwd>/`.
        crate::detect::Agent::Claude => {
            let dir = home
                .join(".claude/projects")
                .join(claude_project_dirname(cwd));
            let mut out = Vec::new();
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    let Some(id) =
                        transcript_id_for(&p.to_string_lossy(), crate::detect::Agent::Claude)
                    else {
                        continue;
                    };
                    let Some(birth) = e.metadata().ok().as_ref().and_then(entry_birth) else {
                        continue;
                    };
                    out.push((p, id, birth));
                }
            }
            out
        }
        // Codex: every date-foldered rollout whose first-line `payload.cwd` matches.
        crate::detect::Agent::Codex => {
            let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
            collect_rollouts(&home.join(".codex/sessions"), 0, &mut files);
            let mut out = Vec::new();
            for (_, path) in files {
                if let Some((c, id)) = codex_head_cwd_id(&path) {
                    if c == cwd {
                        let birth = std::fs::metadata(&path)
                            .ok()
                            .as_ref()
                            .and_then(entry_birth)
                            .unwrap_or(std::time::UNIX_EPOCH);
                        out.push((path, id, birth));
                    }
                }
            }
            out
        }
    }
}

/// Wall-clock start time of process `pid`, or `None` on any failure. Used to
/// order concurrent same-cwd agents so each pairs to the transcript created
/// during its own lifetime.
#[cfg(target_os = "macos")]
pub fn proc_start_time(pid: i32) -> Option<std::time::SystemTime> {
    use std::mem;
    let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
    let ret = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            &mut info as *mut _ as *mut libc::c_void,
            mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
        )
    };
    if ret <= 0 {
        return None;
    }
    Some(
        std::time::UNIX_EPOCH
            + std::time::Duration::new(info.pbi_start_tvsec, (info.pbi_start_tvusec as u32) * 1000),
    )
}

/// Linux: derive wall-clock start from `/proc/<pid>/stat` field 22 (starttime, in
/// clock ticks since boot) + boot time from `/proc/stat`'s `btime`.
#[cfg(target_os = "linux")]
pub fn proc_start_time(pid: i32) -> Option<std::time::SystemTime> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm (field 2) is parenthesized and may itself contain spaces/parens; the
    // fields we want all follow the LAST ')'. After it, field 3 (state) is first,
    // so starttime (field 22) sits at index 22 - 3 = 19.
    let after = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ticks: u64 = fields.get(19)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if hz <= 0 {
        return None;
    }
    let hz = hz as u64;
    let btime = boot_time_secs()?;
    let secs = btime + ticks / hz;
    let nanos = ((ticks % hz) * 1_000_000_000 / hz) as u32;
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(secs, nanos))
}

/// Linux boot time (seconds since epoch) from `/proc/stat`'s `btime` line.
#[cfg(target_os = "linux")]
fn boot_time_secs() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    stat.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn proc_start_time(_pid: i32) -> Option<std::time::SystemTime> {
    None
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
        assert_eq!(
            transcript_id_for("/dev/null", crate::detect::Agent::Claude),
            None
        );
        assert_eq!(
            transcript_id_for(
                "/home/u/.claude/projects/x/notes.txt",
                crate::detect::Agent::Claude
            ),
            None
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
    fn agent_transcript_finds_newest_claude_jsonl_by_cwd_without_open_fd() {
        // Claude open-appends-closes its transcript (no held-open fd), so discovery is
        // by cwd → munged project dir → newest *.jsonl. Stage two transcripts with
        // different mtimes; the NEWER one wins, and NOTHING is held open here.
        let home = tempfile::tempdir().unwrap();
        let cwd = "/work/proj.x"; // note the dot — must munge to a dash
        let dir = home.path().join(".claude/projects/-work-proj-x");
        std::fs::create_dir_all(&dir).unwrap();
        let older = dir.join("11111111-1111-1111-1111-111111111111.jsonl");
        let newer = dir.join("22222222-2222-2222-2222-222222222222.jsonl");
        std::fs::write(&older, b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newer, b"x").unwrap();
        let got = agent_transcript_in(home.path(), cwd, crate::detect::Agent::Claude)
            .expect("must discover the newest claude transcript for this cwd");
        assert_eq!(got.1, "22222222-2222-2222-2222-222222222222");
        assert_eq!(
            std::fs::canonicalize(&got.0).unwrap(),
            std::fs::canonicalize(&newer).unwrap()
        );
        // Unknown cwd → no project dir → None.
        assert_eq!(
            agent_transcript_in(home.path(), "/nope", crate::detect::Agent::Claude),
            None
        );
    }

    #[test]
    fn agent_transcript_finds_codex_rollout_matching_cwd() {
        // Codex sessions are date-foldered; match by the first line's payload.cwd.
        let home = tempfile::tempdir().unwrap();
        let cwd = "/work/codeproj";
        let dir = home.path().join(".codex/sessions/2026/05/31");
        std::fs::create_dir_all(&dir).unwrap();
        let uuid = "019e7e69-6ead-74a0-92ed-50d94369ff5b";
        let mine = dir.join(format!("rollout-2026-05-31T22-21-39-{uuid}.jsonl"));
        std::fs::write(
            &mine,
            format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":uuid,"cwd":cwd}})
            ),
        )
        .unwrap();
        // A newer rollout for a DIFFERENT cwd must be ignored.
        let other_uuid = "019e0000-6ead-74a0-92ed-50d94369ffff";
        let other = dir.join(format!("rollout-2026-05-31T23-00-00-{other_uuid}.jsonl"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &other,
            format!(
                "{}\n",
                serde_json::json!({"payload":{"id":other_uuid,"cwd":"/somewhere/else"}})
            ),
        )
        .unwrap();
        let got = agent_transcript_in(home.path(), cwd, crate::detect::Agent::Codex)
            .expect("must find the codex rollout whose payload.cwd matches");
        assert_eq!(got.1, uuid);
        assert_eq!(
            std::fs::canonicalize(&got.0).unwrap(),
            std::fs::canonicalize(&mine).unwrap()
        );
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

    #[test]
    fn agent_transcripts_returns_all_claude_jsonl_for_cwd_with_births() {
        // Two concurrent sessions in one cwd → BOTH transcripts must surface (the
        // singular `agent_transcript` only returns one, which collapsed two panes
        // onto the same id in production).
        let home = tempfile::tempdir().unwrap();
        let cwd = "/work/proj";
        let dir = home.path().join(".claude/projects/-work-proj");
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("11111111-1111-1111-1111-111111111111.jsonl");
        let b = dir.join("22222222-2222-2222-2222-222222222222.jsonl");
        std::fs::write(&a, b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&b, b"x").unwrap();
        let mut got = agent_transcripts_in(home.path(), cwd, crate::detect::Agent::Claude);
        got.sort_by(|x, y| x.1.cmp(&y.1));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, "11111111-1111-1111-1111-111111111111");
        assert_eq!(got[1].1, "22222222-2222-2222-2222-222222222222");
        // birth(b) must be later than birth(a) — the ordering the pairing relies on.
        let birth = |id: &str| got.iter().find(|t| t.1 == id).unwrap().2;
        assert!(
            birth("22222222-2222-2222-2222-222222222222")
                > birth("11111111-1111-1111-1111-111111111111")
        );
        // Unknown cwd → no project dir → empty.
        assert!(agent_transcripts_in(home.path(), "/nope", crate::detect::Agent::Claude).is_empty());
    }

    #[test]
    fn proc_start_time_is_recent_for_a_freshly_spawned_child() {
        use std::process::{Command, Stdio};
        let mut child = Command::new("sleep")
            .arg("5")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let got = super::proc_start_time(pid);
        // Clean up BEFORE asserting so a failure doesn't leak the child.
        let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        let _ = child.wait();
        let started = got.expect("must resolve a start time for a live child");
        let age = std::time::SystemTime::now()
            .duration_since(started)
            .expect("start time must precede now");
        assert!(
            age < std::time::Duration::from_secs(120),
            "a just-spawned child's start time should be recent, got age {age:?}"
        );
    }
}
