//! Notification sound playback. Fire-and-forget OS-native player spawn
//! (afplay on macOS, paplay/pw-play/ffplay/mpg123/mpv on Linux) with
//! per-second debouncing. The embedded default sound bytes ship in
//! `assets/blocked.wav` and are extracted to a platform cache directory
//! on first play.

use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEFAULT_BLOCKED_WAV: &[u8] = include_bytes!("../assets/blocked.wav");
const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(1);

/// Pure transition predicate. The sound fires exactly when an agent
/// just flipped INTO `Blocked` from a non-Blocked state.
pub fn should_play_blocked(
    prev: crate::detect::AgentState,
    next: crate::detect::AgentState,
) -> bool {
    use crate::detect::AgentState::*;
    prev != Blocked && next == Blocked
}

#[derive(Default)]
pub struct SoundDebouncer {
    last_fire: Mutex<Option<Instant>>,
}

impl SoundDebouncer {
    pub const fn new() -> Self {
        SoundDebouncer {
            last_fire: Mutex::new(None),
        }
    }

    /// Returns `true` once per `DEBOUNCE_INTERVAL`. Subsequent calls
    /// within the window return `false`.
    pub fn try_fire(&self) -> bool {
        let now = Instant::now();
        let mut guard = self.last_fire.lock().expect("sound debouncer poisoned");
        match *guard {
            Some(last) if now.duration_since(last) < DEBOUNCE_INTERVAL => false,
            _ => {
                *guard = Some(now);
                true
            }
        }
    }

    /// Construct with an explicit `last_fire` value — used by tests to
    /// exercise the time window without a real `sleep`.
    #[cfg(test)]
    pub fn with_last_fire_for_test(last_fire: Option<Instant>) -> Self {
        SoundDebouncer {
            last_fire: Mutex::new(last_fire),
        }
    }
}

/// Fire-and-forget play of the configured "blocked" sound. Errors are
/// silently dropped — sound playback must never take down the daemon.
pub fn play_blocked(config: &crate::config::SoundConfig) {
    if !config.enabled {
        return;
    }
    let path = match resolve_sound_path(config) {
        Some(p) => p,
        None => return,
    };
    let Some(player) = pick_player() else { return };
    let _ = std::process::Command::new(player)
        .arg(&path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn resolve_sound_path(config: &crate::config::SoundConfig) -> Option<std::path::PathBuf> {
    if let Some(override_path) = &config.blocked {
        return Some(std::path::PathBuf::from(override_path));
    }
    extract_default_sound().ok()
}

fn extract_default_sound() -> std::io::Result<std::path::PathBuf> {
    let cache = cache_dir().ok_or_else(|| std::io::Error::other("no cache dir"))?;
    std::fs::create_dir_all(&cache)?;
    let path = cache.join("blocked.wav");
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() as usize == DEFAULT_BLOCKED_WAV.len() {
            return Ok(path);
        }
    }
    std::fs::write(&path, DEFAULT_BLOCKED_WAV)?;
    Ok(path)
}

#[cfg(target_os = "macos")]
fn cache_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join("Library/Caches/dvlpr"))
}

#[cfg(not(target_os = "macos"))]
fn cache_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("dvlpr"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache/dvlpr"))
}

#[cfg(target_os = "macos")]
fn pick_player() -> Option<&'static str> {
    Some("afplay")
}

#[cfg(not(target_os = "macos"))]
fn pick_player() -> Option<&'static str> {
    use std::sync::OnceLock;
    static CHOICE: OnceLock<Option<&'static str>> = OnceLock::new();
    *CHOICE.get_or_init(|| {
        for cand in ["paplay", "pw-play", "ffplay", "mpg123", "mpv"] {
            if which_exists(cand) {
                return Some(cand);
            }
        }
        None
    })
}

#[cfg(not(target_os = "macos"))]
fn which_exists(bin: &str) -> bool {
    if let Some(path) = std::env::var_os("PATH") {
        for p in std::env::split_paths(&path) {
            let candidate = p.join(bin);
            if candidate.is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::AgentState::*;
    use std::time::Duration;

    #[test]
    fn should_play_idle_to_blocked() {
        assert!(should_play_blocked(Idle, Blocked));
    }

    #[test]
    fn should_play_working_to_blocked() {
        assert!(should_play_blocked(Working, Blocked));
    }

    #[test]
    fn should_not_play_blocked_to_blocked() {
        assert!(!should_play_blocked(Blocked, Blocked));
    }

    #[test]
    fn should_not_play_idle_to_working() {
        assert!(!should_play_blocked(Idle, Working));
    }

    #[test]
    fn debouncer_allows_first_fire_then_blocks_second() {
        let d = SoundDebouncer::new();
        assert!(d.try_fire());
        assert!(!d.try_fire());
    }

    #[test]
    fn debouncer_allows_second_fire_after_interval_expires() {
        // Avoid a real-time sleep: backdate `last_fire` past the
        // debounce window via the test-only constructor.
        let d = SoundDebouncer::with_last_fire_for_test(
            Some(Instant::now() - (DEBOUNCE_INTERVAL + Duration::from_millis(50))),
        );
        assert!(d.try_fire());
    }
}
