//! `dvlpr update` restart-and-restore orchestration.
//!
//! After the binary swap, restart eligible *detached* sessions running a
//! compatible protocol and let each respawned daemon restore its layout via
//! `DVLPR_RESTORE=1`. The stop uses the keep-snapshot shutdown so the layout
//! survives. See docs/superpowers/specs/2026-06-01-update-restart-restore-design.md.

use std::io;
use std::path::Path;
use std::time::Duration;

use crate::protocol::StatusInfo;

/// How `dvlpr update` decides whether to restart running sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPref {
    /// Default: prompt if interactive, decline if not.
    Prompt,
    /// `--restart`: always restart (even non-interactive).
    Force,
    /// `--no-restart`: never restart (swap only).
    Skip,
}

/// Why a live session was not restarted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    AttachedElsewhere,
    SelfSession,
    StaleIncompatible,
}

impl SkipReason {
    pub fn message(&self) -> &'static str {
        match self {
            SkipReason::AttachedElsewhere => "attached in another terminal",
            SkipReason::SelfSession => "you're attached to it now",
            SkipReason::StaleIncompatible => {
                "running an older dvlpr — pkill -f 'dvlpr server' then start it fresh"
            }
        }
    }
}

/// A live session as seen during enumeration. `status == None` means the
/// `Intent::Status` probe failed → a stale/incompatible (older-protocol) daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub name: String,
    pub status: Option<StatusInfo>,
}

/// Pure split of live sessions into restart-eligible names and skip list.
/// Eligible = compatible (Status replied) ∧ detached (`clients == 0`) ∧ not the
/// `$DVLPR` self-session.
pub fn classify_sessions(
    sessions: &[LiveSession],
    self_session: Option<&str>,
) -> (Vec<String>, Vec<(String, SkipReason)>) {
    let mut eligible = Vec::new();
    let mut skipped = Vec::new();
    for s in sessions {
        if self_session == Some(s.name.as_str()) {
            skipped.push((s.name.clone(), SkipReason::SelfSession));
            continue;
        }
        match &s.status {
            None => skipped.push((s.name.clone(), SkipReason::StaleIncompatible)),
            Some(info) if info.clients > 0 => {
                skipped.push((s.name.clone(), SkipReason::AttachedElsewhere))
            }
            Some(_) => eligible.push(s.name.clone()),
        }
    }
    (eligible, skipped)
}

/// Pure: should we restart, given the preference and whether stdin is a TTY?
pub fn should_restart(pref: RestartPref, is_tty: bool, answered_yes: bool) -> bool {
    match pref {
        RestartPref::Force => true,
        RestartPref::Skip => false,
        RestartPref::Prompt => is_tty && answered_yes,
    }
}

/// Injected session operations — real impl talks to sockets/processes; tests
/// use a fake. Generic (static dispatch), so async fns in the trait are fine.
#[allow(async_fn_in_trait)]
pub trait SessionOps {
    /// Names of sessions with a live socket under the runtime dir.
    async fn list_live(&self) -> Vec<String>;
    /// `Intent::Status` for a session; `None` if the probe fails (stale/incompatible).
    async fn status(&self, name: &str) -> Option<StatusInfo>;
    /// Send the keep-snapshot shutdown to a session.
    async fn keep_snapshot_stop(&self, name: &str) -> io::Result<()>;
    /// Is the session's socket currently live?
    async fn is_live(&self, name: &str) -> bool;
    /// Respawn the daemon from `exe` with `DVLPR_RESTORE=1`.
    fn respawn(&self, exe: &Path, name: &str) -> io::Result<()>;
}

const STOP_TIMEOUT_POLLS: u32 = 100; // ×20ms = 2s
const RESPAWN_ATTEMPTS: u32 = 100; // re-spawn across the flock-release window (~2s total)
const UP_TIMEOUT_POLLS: u32 = 1; // one liveness probe per respawn, then re-spawn
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Restart one eligible session: keep-snapshot stop → wait dead → respawn with
/// retry → wait up. Returns true on success. The respawn retry covers the race
/// where the old daemon has removed its socket but not yet released the flock,
/// so a respawn lands on `WouldBlock` and exits 0 without binding.
async fn restart_one<O: SessionOps>(ops: &O, exe: &Path, name: &str) -> bool {
    if ops.keep_snapshot_stop(name).await.is_err() {
        return false;
    }
    let mut dead = false;
    for _ in 0..STOP_TIMEOUT_POLLS {
        if !ops.is_live(name).await {
            dead = true;
            break;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    if !dead {
        return false; // shutdown timeout — leave it; snapshot is preserved
    }
    for _ in 0..RESPAWN_ATTEMPTS {
        if ops.respawn(exe, name).is_err() {
            return false;
        }
        for _ in 0..UP_TIMEOUT_POLLS {
            if ops.is_live(name).await {
                return true;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        // Not up within the window: likely the flock race — re-spawn.
    }
    false
}

/// Production entry: build the real ops and run the orchestration.
pub async fn restart_running_sessions(install_exe: &Path, pref: RestartPref) {
    restart_with(
        &RealSessionOps,
        install_exe,
        pref,
        crate::update::stdin_is_tty(),
    )
    .await
}

/// Testable core. `is_tty` is injected so unit tests are deterministic.
pub async fn restart_with<O: SessionOps>(
    ops: &O,
    install_exe: &Path,
    pref: RestartPref,
    is_tty: bool,
) {
    if pref == RestartPref::Skip {
        return;
    }
    let mut sessions = Vec::new();
    for name in ops.list_live().await {
        let status = ops.status(&name).await;
        sessions.push(LiveSession { name, status });
    }
    let self_session = std::env::var("DVLPR").ok();
    let (eligible, skipped) = classify_sessions(&sessions, self_session.as_deref());

    if eligible.is_empty() {
        report_skips(&skipped);
        return;
    }

    let answered_yes = match pref {
        RestartPref::Prompt if is_tty => prompt_restart(&eligible),
        _ => false,
    };
    if !should_restart(pref, is_tty, answered_yes) {
        report_skips(&skipped);
        return;
    }

    for name in &eligible {
        if restart_one(ops, install_exe, name).await {
            match ops.status(name).await {
                Some(info) => {
                    println!("==> Restarted {name} … restored {} window(s)", info.windows)
                }
                None => println!("==> Restarted {name}"),
            }
        } else {
            eprintln!(
                "==> {name}: restart failed — its layout is saved; run `dvlpr {name}` to restore"
            );
        }
    }
    report_skips(&skipped);
    println!("Done. Reattach with `dvlpr <name>`.");
}

fn report_skips(skipped: &[(String, SkipReason)]) {
    for (name, reason) in skipped {
        println!("  (skipped '{name}' — {})", reason.message());
    }
}

/// Prompt `Restart N running session(s) … [Y/n]`. Default Yes on Enter.
fn prompt_restart(eligible: &[String]) -> bool {
    eprint!(
        "Restart {} running session(s) ({}) to apply the update? [Y/n] ",
        eligible.len(),
        eligible.join(", ")
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let ans = line.trim().to_ascii_lowercase();
    ans.is_empty() || ans == "y" || ans == "yes"
}

/// Production [`SessionOps`] — talks to session sockets and spawns daemons.
pub struct RealSessionOps;

impl SessionOps for RealSessionOps {
    async fn list_live(&self) -> Vec<String> {
        let dir = crate::server::socket::runtime_dir();
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                let Some(name) = fname.strip_suffix(".sock") else {
                    continue;
                };
                let path = crate::server::socket::socket_path_in(&dir, name);
                if crate::server::socket::is_live(&path).await {
                    names.push(name.to_string());
                }
            }
        }
        names
    }

    async fn status(&self, name: &str) -> Option<StatusInfo> {
        let path =
            crate::server::socket::socket_path_in(&crate::server::socket::runtime_dir(), name);
        crate::client::query_status(&path).await.ok()
    }

    async fn keep_snapshot_stop(&self, name: &str) -> io::Result<()> {
        let path =
            crate::server::socket::socket_path_in(&crate::server::socket::runtime_dir(), name);
        crate::client::send_kill(&path, true).await
    }

    async fn is_live(&self, name: &str) -> bool {
        let path =
            crate::server::socket::socket_path_in(&crate::server::socket::runtime_dir(), name);
        crate::server::socket::is_live(&path).await
    }

    fn respawn(&self, exe: &Path, name: &str) -> io::Result<()> {
        crate::daemon::spawn_detached_server_at(exe, name, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(name: &str, clients: u32) -> LiveSession {
        LiveSession {
            name: name.into(),
            status: Some(StatusInfo {
                windows: 1,
                clients,
            }),
        }
    }
    fn stale(name: &str) -> LiveSession {
        LiveSession {
            name: name.into(),
            status: None,
        }
    }

    #[test]
    fn classify_splits_eligible_attached_self_and_stale() {
        let sessions = vec![
            live("default", 0),
            live("work", 0),
            live("api", 2),
            stale("old"),
            live("devbox", 0),
        ];
        let (eligible, skipped) = classify_sessions(&sessions, Some("devbox"));
        assert_eq!(eligible, vec!["default".to_string(), "work".to_string()]);
        assert!(skipped.contains(&("api".to_string(), SkipReason::AttachedElsewhere)));
        assert!(skipped.contains(&("old".to_string(), SkipReason::StaleIncompatible)));
        assert!(skipped.contains(&("devbox".to_string(), SkipReason::SelfSession)));
    }

    #[test]
    fn should_restart_honors_pref_and_tty() {
        assert!(should_restart(RestartPref::Force, false, false));
        assert!(!should_restart(RestartPref::Skip, true, true));
        assert!(should_restart(RestartPref::Prompt, true, true));
        assert!(!should_restart(RestartPref::Prompt, true, false));
        assert!(!should_restart(RestartPref::Prompt, false, true));
    }

    use std::cell::RefCell;
    use std::path::PathBuf;

    /// Fake ops: scripted Status results + an is_live sequence that simulates the
    /// flock-release race (false a few times, then true) to exercise respawn retry.
    struct FakeOps {
        live_names: Vec<String>,
        statuses: std::collections::HashMap<String, Option<StatusInfo>>,
        is_live_script: RefCell<std::collections::VecDeque<bool>>,
        respawns: RefCell<u32>,
        stops: RefCell<Vec<String>>,
    }

    impl SessionOps for FakeOps {
        async fn list_live(&self) -> Vec<String> {
            self.live_names.clone()
        }
        async fn status(&self, name: &str) -> Option<StatusInfo> {
            self.statuses.get(name).cloned().flatten()
        }
        async fn keep_snapshot_stop(&self, name: &str) -> io::Result<()> {
            self.stops.borrow_mut().push(name.to_string());
            Ok(())
        }
        async fn is_live(&self, _name: &str) -> bool {
            self.is_live_script.borrow_mut().pop_front().unwrap_or(true)
        }
        fn respawn(&self, _exe: &Path, _name: &str) -> io::Result<()> {
            *self.respawns.borrow_mut() += 1;
            Ok(())
        }
    }

    #[tokio::test]
    async fn respawn_retries_across_flock_race_then_succeeds() {
        let mut script = std::collections::VecDeque::new();
        script.extend([false, false, false, true]);
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(
            "work".to_string(),
            Some(StatusInfo {
                windows: 2,
                clients: 0,
            }),
        );
        let ops = FakeOps {
            live_names: vec!["work".into()],
            statuses,
            is_live_script: RefCell::new(script),
            respawns: RefCell::new(0),
            stops: RefCell::new(Vec::new()),
        };
        let ok = restart_one(&ops, &PathBuf::from("/bin/dvlpr"), "work").await;
        assert!(
            ok,
            "restart should ultimately succeed after retrying respawn"
        );
        assert!(
            *ops.respawns.borrow() >= 2,
            "respawn must retry across the flock-release window (got {})",
            ops.respawns.borrow()
        );
    }

    #[tokio::test]
    async fn restart_with_skip_pref_does_nothing() {
        let ops = FakeOps {
            live_names: vec!["work".into()],
            statuses: std::collections::HashMap::new(),
            is_live_script: RefCell::new(std::collections::VecDeque::new()),
            respawns: RefCell::new(0),
            stops: RefCell::new(Vec::new()),
        };
        restart_with(&ops, &PathBuf::from("/bin/dvlpr"), RestartPref::Skip, true).await;
        assert_eq!(*ops.respawns.borrow(), 0);
        assert!(ops.stops.borrow().is_empty());
    }

    #[tokio::test]
    async fn restart_with_force_skips_stale_and_attached_restarts_detached() {
        let mut statuses = std::collections::HashMap::new();
        statuses.insert(
            "good".to_string(),
            Some(StatusInfo {
                windows: 1,
                clients: 0,
            }),
        );
        statuses.insert(
            "busy".to_string(),
            Some(StatusInfo {
                windows: 1,
                clients: 1,
            }),
        );
        statuses.insert("old".to_string(), None);
        let ops = FakeOps {
            live_names: vec!["good".into(), "busy".into(), "old".into()],
            statuses,
            is_live_script: RefCell::new(std::collections::VecDeque::from([false, true])),
            respawns: RefCell::new(0),
            stops: RefCell::new(Vec::new()),
        };
        restart_with(
            &ops,
            &PathBuf::from("/bin/dvlpr"),
            RestartPref::Force,
            false,
        )
        .await;
        assert_eq!(*ops.stops.borrow(), vec!["good".to_string()]);
        assert!(*ops.respawns.borrow() >= 1);
    }
}
