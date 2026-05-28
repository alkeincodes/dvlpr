//! Derive a display label for a Claude session, scraping
//! `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`. Priority: a
//! `custom-title` event from `/rename`, then the first user message,
//! then the JSONL filename's last 8 characters as a UUID-tail
//! fallback.

use std::path::{Path, PathBuf};

const JSONL_READ_CAP: usize = 256 * 1024; // 256 KB
const JSONL_HEAD_TAIL_KIB: usize = 32 * 1024; // 32 KB head + 32 KB tail
const UPSTREAM_LABEL_CAP: usize = 64;

/// Resolve a Claude session display label for `cwd`, or `None` when
/// the directory has no `~/.claude/projects/<encoded>/` entry. Reads
/// `$HOME` at call time.
pub fn session_label(cwd: &Path) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    session_label_with_home(cwd, &home)
}

/// Test-friendly variant taking an explicit home path. Avoids
/// mutating global `$HOME` across parallel tests.
pub fn session_label_with_home(cwd: &Path, home: &Path) -> Option<String> {
    let encoded = encode_cwd(cwd)?;
    let dir = home.join(".claude").join("projects").join(&encoded);
    if !dir.exists() {
        return None;
    }
    let jsonl = newest_jsonl(&dir)?;
    let text = read_capped(&jsonl, JSONL_READ_CAP, JSONL_HEAD_TAIL_KIB).ok()?;
    let label = scan_label(&text).or_else(|| uuid_tail_from_path(&jsonl))?;
    Some(truncate_chars(&label, UPSTREAM_LABEL_CAP))
}

/// Encode an absolute cwd to Claude's project directory name: leading
/// slash + every `/` → `-`. Example: `/Users/x/dvlpr` → `-Users-x-dvlpr`.
/// Public so tests and `session_label` share the encoder.
pub fn encode_cwd(cwd: &Path) -> Option<String> {
    let s = cwd.to_str()?;
    if !s.starts_with('/') {
        return None;
    }
    Some(s.replace('/', "-"))
}

fn newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        match &newest {
            Some((best, _)) if *best >= mtime => {}
            _ => newest = Some((mtime, path)),
        }
    }
    newest.map(|(_, p)| p)
}

fn read_capped(path: &Path, cap: usize, head_tail: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len() as usize;
    if len <= cap {
        let mut buf = String::with_capacity(len);
        f.read_to_string(&mut buf)?;
        return Ok(buf);
    }
    // Larger than cap → read head + tail.
    let mut head = vec![0u8; head_tail];
    let n = f.read(&mut head)?;
    head.truncate(n);
    f.seek(SeekFrom::End(-(head_tail as i64)))?;
    let mut tail = vec![0u8; head_tail];
    let n = f.read(&mut tail)?;
    tail.truncate(n);
    let mut out = String::from_utf8_lossy(&head).into_owned();
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(&tail));
    Ok(out)
}

fn scan_label(text: &str) -> Option<String> {
    // Pass 1: any custom-title event (highest priority).
    if let Some(title) = scan_custom_title(text) {
        return Some(title);
    }
    // Pass 2: first user message whose content is a non-empty string.
    scan_first_user_message(text)
}

fn scan_custom_title(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("custom-title") {
            if let Some(title) = v.get("title").and_then(|t| t.as_str()) {
                let trimmed = title.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    None
}

fn scan_first_user_message(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        // Look for `message.content` as a string. Skip when it's a
        // structured object (tool-call payloads etc.).
        let content = v
            .pointer("/message/content")
            .or_else(|| v.get("content"))?;
        if let Some(s) = content.as_str() {
            // Strip leading whitespace; collapse runs of whitespace
            // into single spaces.
            let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
            if !collapsed.is_empty() {
                return Some(collapsed);
            }
        }
    }
    None
}

fn uuid_tail_from_path(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    if stem.len() >= 8 {
        Some(format!("…{}", &stem[stem.len() - 8..]))
    } else {
        Some(format!("…{stem}"))
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write");
    }

    /// Build a fake `<home>/.claude/projects/<encoded-cwd>/<filename>` and
    /// return the absolute fake cwd that resolves to that dir.
    fn make_fixture(home: &Path, project: &Path, filename: &str, jsonl: &str) {
        let encoded = encode_cwd(project).expect("encode");
        let dir = home.join(".claude").join("projects").join(&encoded);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir.join(filename), jsonl);
    }

    // No HOME env mutation — Rust tests run in parallel so global env
    // state races. Tests call `session_label_with_home` directly with
    // a temp home path; the public `session_label` is a thin wrapper
    // that reads `$HOME` at call time.

    fn with_home<F: FnOnce(&Path)>(f: F) {
        let tmp = tempfile::tempdir().unwrap();
        f(tmp.path());
    }

    #[test]
    fn encode_cwd_replaces_slashes_with_dashes_and_keeps_leading_dash() {
        assert_eq!(
            encode_cwd(Path::new("/Users/x/Developments/dvlpr")).as_deref(),
            Some("-Users-x-Developments-dvlpr")
        );
    }

    #[test]
    fn session_label_returns_none_when_projects_dir_missing() {
        with_home(|home| {
            let cwd = Path::new("/nonexistent/project");
            assert_eq!(session_label_with_home(cwd, home), None);
        });
    }

    #[test]
    fn session_label_uses_custom_title_when_present() {
        with_home(|home| {
            let project = Path::new("/tmp/project-a");
            make_fixture(
                home,
                project,
                "abc12345.jsonl",
                concat!(
                    "{\"type\":\"user\",\"message\":{\"content\":\"hello world\"}}\n",
                    "{\"type\":\"custom-title\",\"title\":\"My Renamed Session\"}\n",
                ),
            );
            assert_eq!(
                session_label_with_home(project, home).as_deref(),
                Some("My Renamed Session")
            );
        });
    }

    #[test]
    fn session_label_falls_back_to_first_user_message_when_no_custom_title() {
        with_home(|home| {
            let project = Path::new("/tmp/project-b");
            make_fixture(
                home,
                project,
                "deadbeef.jsonl",
                concat!(
                    "{\"type\":\"system\",\"info\":\"setup\"}\n",
                    "{\"type\":\"user\",\"message\":{\"content\":\"  refactor the auth flow  \"}}\n",
                    "{\"type\":\"assistant\",\"message\":{\"content\":\"ok\"}}\n",
                ),
            );
            assert_eq!(
                session_label_with_home(project, home).as_deref(),
                Some("refactor the auth flow")
            );
        });
    }

    #[test]
    fn session_label_falls_back_to_uuid_tail_when_no_recognized_events() {
        with_home(|home| {
            let project = Path::new("/tmp/project-c");
            make_fixture(
                home,
                project,
                "0123456789abcdef.jsonl",
                "{\"type\":\"system\"}\n",
            );
            assert_eq!(
                session_label_with_home(project, home).as_deref(),
                Some("…89abcdef")
            );
        });
    }

    #[test]
    fn session_label_picks_most_recently_modified_jsonl() {
        use std::time::Duration;
        with_home(|home| {
            let project = Path::new("/tmp/project-d");
            make_fixture(
                home,
                project,
                "11111111.jsonl",
                "{\"type\":\"custom-title\",\"title\":\"OLD\"}\n",
            );
            std::thread::sleep(Duration::from_millis(20));
            make_fixture(
                home,
                project,
                "22222222.jsonl",
                "{\"type\":\"custom-title\",\"title\":\"NEW\"}\n",
            );
            assert_eq!(
                session_label_with_home(project, home).as_deref(),
                Some("NEW")
            );
        });
    }
}
