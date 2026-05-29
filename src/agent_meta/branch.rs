//! Stdlib-only git branch detection from a cwd. Walks ancestors to find
//! `.git`, follows worktree gitfiles, reads `HEAD`, returns the branch
//! name or a short detached-HEAD marker.

use std::path::{Path, PathBuf};

const HEAD_READ_CAP: usize = 4096;
const ANCESTOR_LIMIT: usize = 64;

/// Return the current branch name for the git repo containing `cwd`,
/// or a short detached-HEAD marker `(<7chars>)`. `None` when `cwd` is
/// not inside a git repo or HEAD can't be read.
pub fn detect_branch(cwd: &Path) -> Option<String> {
    let canon = cwd.canonicalize().ok()?;
    let dot_git = find_dot_git(&canon)?;
    let gitdir = resolve_gitdir(&dot_git)?;
    let head_path = gitdir.join("HEAD");
    let head = read_cap(&head_path, HEAD_READ_CAP).ok()?;
    parse_head(&head)
}

fn find_dot_git(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    for _ in 0..ANCESTOR_LIMIT {
        let dir = cur?;
        let candidate = dir.join(".git");
        if candidate.exists() {
            return Some(candidate);
        }
        cur = dir.parent();
    }
    None
}

fn resolve_gitdir(dot_git: &Path) -> Option<PathBuf> {
    let md = std::fs::symlink_metadata(dot_git).ok()?;
    if md.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    if md.is_file() {
        let body = read_cap(dot_git, HEAD_READ_CAP).ok()?;
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("gitdir:") {
                let p = Path::new(rest.trim());
                if p.is_absolute() {
                    return Some(p.to_path_buf());
                }
                // Relative gitdir: resolve against the directory holding `.git`.
                let parent = dot_git.parent()?;
                return Some(parent.join(p));
            }
        }
    }
    None
}

fn parse_head(head: &str) -> Option<String> {
    let first = head.lines().next()?.trim();
    if let Some(rest) = first.strip_prefix("ref:") {
        let r = rest.trim();
        let name = r.strip_prefix("refs/heads/").unwrap_or(r);
        if name.is_empty() {
            return None;
        }
        return Some(name.to_string());
    }
    if first.len() >= 40 && first[..40].chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(format!("({})", &first[..7]));
    }
    None
}

fn read_cap(path: &Path, cap: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; cap];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo(dir: &Path, branch: &str) {
        Command::new("git")
            .args(["init", "-q", "-b", branch])
            .current_dir(dir)
            .status()
            .expect("git init");
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn detect_branch_normal_repo_returns_named_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        init_repo(path, "main");
        assert_eq!(detect_branch(path), Some("main".to_string()));
    }

    #[test]
    fn detect_branch_walks_up_to_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo(root, "main");
        let nested = root.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(detect_branch(&nested), Some("main".to_string()));
    }

    #[test]
    fn detect_branch_outside_repo_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(detect_branch(tmp.path()), None);
    }

    #[test]
    fn detect_branch_worktree_dot_git_file_resolves_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo(root, "main");
        run_git(root, &["commit", "--allow-empty", "-q", "-m", "init"]);
        let wt_dir = tmp.path().join("wt");
        run_git(
            root,
            &[
                "worktree",
                "add",
                wt_dir.to_str().unwrap(),
                "-b",
                "feature-x",
            ],
        );
        assert_eq!(detect_branch(&wt_dir), Some("feature-x".to_string()));
    }

    #[test]
    fn detect_branch_detached_head_returns_short_sha_in_parens() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_repo(root, "main");
        run_git(root, &["commit", "--allow-empty", "-q", "-m", "c1"]);
        // Read HEAD's SHA, then checkout that SHA to detach.
        let head = std::fs::read_to_string(root.join(".git/HEAD")).unwrap();
        let sha_ref = head.trim().strip_prefix("ref: ").unwrap();
        let sha = std::fs::read_to_string(root.join(".git").join(sha_ref)).unwrap();
        let sha = sha.trim().to_string();
        run_git(root, &["checkout", "-q", &sha]);
        let got = detect_branch(root).expect("some branch");
        assert!(
            got.starts_with('(') && got.ends_with(')') && got.len() == 9,
            "expected `(<7chars>)`, got {got:?}"
        );
        assert_eq!(&got[1..8], &sha[..7]);
    }
}
