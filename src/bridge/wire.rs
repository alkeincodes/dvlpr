//! Bridge NDJSON contract (spec §2.2): one JSON object per line. Events go
//! bridge -> consumer; commands come consumer -> bridge, each carrying a
//! client-chosen `id` echoed in the `reply`.

use serde::{Deserialize, Serialize};

use crate::protocol::{AgentInfo, NamedKey};

/// Bumped when the NDJSON contract changes (independent of the socket
/// PROTOCOL_VERSION). Consumers refuse unknown majors.
pub const BRIDGE_PROTOCOL: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum BridgeEvent {
    Hello {
        bridge_protocol: u32,
        dvlpr_version: String,
    },
    /// Full roster snapshot for one dvlpr session (snapshot-on-change).
    Agents {
        session: String,
        epoch: String,
        agents: Vec<AgentInfo>,
    },
    /// A whole dvlpr session disappeared (daemon exit). Spec addendum: the
    /// per-session `agents` snapshots cannot express this themselves.
    SessionGone {
        session: String,
    },
    Transcript {
        key: String,
        line: serde_json::Value,
    },
    TranscriptReset {
        key: String,
    },
    Reply {
        id: String,
        ok: bool,
        code: Option<String>,
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum BridgeCmd {
    Watch {
        id: String,
        key: String,
        epoch: Option<String>,
        replay: Replay,
    },
    Unwatch {
        id: String,
        key: String,
    },
    Send {
        id: String,
        key: String,
        epoch: Option<String>,
        text: String,
        submit: bool,
    },
    Key {
        id: String,
        key: String,
        epoch: Option<String>,
        name: String,
    },
    WindowClose {
        id: String,
        session: String,
        epoch: Option<String>,
        window: usize, // 0-based (spec §2.2)
    },
    WindowRename {
        id: String,
        session: String,
        epoch: Option<String>,
        window: usize,
        name: String,
    },
    WindowSelect {
        id: String,
        session: String,
        epoch: Option<String>,
        window: usize,
    },
    PaneSplit {
        id: String,
        session: String,
        epoch: Option<String>,
        window: usize,
        dir: String, // "right" | "down"
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Replay {
    Full,
    None,
}

/// `"<session>/<pane_id>"` -> (session, pane). None on malformed input.
pub fn parse_key(key: &str) -> Option<(String, u64)> {
    let (session, pane) = key.rsplit_once('/')?;
    if session.is_empty() {
        return None;
    }
    Some((session.to_string(), pane.parse().ok()?))
}

/// Spec key names: enter|esc|up|down|tab|digit_N|char:<c>.
pub fn parse_key_name(name: &str) -> Option<NamedKey> {
    match name {
        "enter" => Some(NamedKey::Enter),
        "esc" => Some(NamedKey::Esc),
        "up" => Some(NamedKey::Up),
        "down" => Some(NamedKey::Down),
        "tab" => Some(NamedKey::Tab),
        _ => {
            if let Some(d) = name.strip_prefix("digit_") {
                return d
                    .parse::<u8>()
                    .ok()
                    .filter(|d| *d <= 9)
                    .map(NamedKey::Digit);
            }
            if let Some(c) = name.strip_prefix("char:") {
                let mut chars = c.chars();
                let ch = chars.next()?;
                return chars.next().is_none().then_some(NamedKey::Char(ch));
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_serialize_to_spec_shapes() {
        let hello = BridgeEvent::Hello {
            bridge_protocol: 1,
            dvlpr_version: "0.4.3".into(),
        };
        assert_eq!(
            serde_json::to_string(&hello).unwrap(),
            r#"{"event":"hello","bridge_protocol":1,"dvlpr_version":"0.4.3"}"#
        );
        let reply = BridgeEvent::Reply {
            id: "1".into(),
            ok: false,
            code: Some("stale_target".into()),
            message: None,
        };
        let v: serde_json::Value = serde_json::to_value(&reply).unwrap();
        assert_eq!(v["event"], "reply");
        assert_eq!(v["code"], "stale_target");
    }

    #[test]
    fn commands_deserialize_from_spec_shapes() {
        let cmd: BridgeCmd = serde_json::from_str(
            r#"{"id":"3","cmd":"send","key":"default/3","epoch":"e","text":"hi","submit":true}"#,
        )
        .unwrap();
        match cmd {
            BridgeCmd::Send {
                id,
                key,
                epoch,
                text,
                submit,
            } => {
                assert_eq!(id, "3");
                assert_eq!(key, "default/3");
                assert_eq!(epoch.as_deref(), Some("e"));
                assert_eq!(text, "hi");
                assert!(submit);
            }
            other => panic!("wrong variant: {other:?}"),
        }
        let cmd: BridgeCmd = serde_json::from_str(
            r#"{"id":"5","cmd":"window_close","session":"default","epoch":"e","window":2}"#,
        )
        .unwrap();
        assert!(matches!(cmd, BridgeCmd::WindowClose { window: 2, .. }));
    }

    #[test]
    fn key_string_parses_to_session_and_pane() {
        assert_eq!(parse_key("default/3").unwrap(), ("default".into(), 3));
        assert_eq!(parse_key("my.sess/12").unwrap(), ("my.sess".into(), 12));
        assert!(parse_key("nopane").is_none());
        assert!(parse_key("s/notanum").is_none());
    }

    #[test]
    fn key_names_parse_to_named_keys() {
        use crate::protocol::NamedKey;
        assert_eq!(parse_key_name("enter"), Some(NamedKey::Enter));
        assert_eq!(parse_key_name("esc"), Some(NamedKey::Esc));
        assert_eq!(parse_key_name("up"), Some(NamedKey::Up));
        assert_eq!(parse_key_name("down"), Some(NamedKey::Down));
        assert_eq!(parse_key_name("tab"), Some(NamedKey::Tab));
        assert_eq!(parse_key_name("digit_1"), Some(NamedKey::Digit(1)));
        assert_eq!(parse_key_name("char:y"), Some(NamedKey::Char('y')));
        assert_eq!(parse_key_name("bogus"), None);
    }
}
