//! The `dvlpr <noun> <verb>` control CLI: parse, resolve the target session,
//! send one command to its daemon, map the reply to an exit code.

use crate::protocol::{ControlCommand, SplitDir};

/// Outcome of parsing the noun-verb args (session selector already stripped).
#[derive(Debug, PartialEq, Eq)]
enum Parsed {
    Cmd(ControlCommand),
    Usage(String),
}

const USAGE: &str = "\
control commands:
  dvlpr window new [--name NAME]      dvlpr pane split right|down
  dvlpr window rename <NAME>          dvlpr pane close
  dvlpr window close                  dvlpr pane zoom
  dvlpr window next | prev            dvlpr sidebar toggle
  dvlpr window select <N>
target a session with a leading @name or a trailing --session NAME";

/// Resolve the target session and return `(name, remaining_args)` with the
/// selector removed. Precedence: leading `@name` wins over `--session NAME`,
/// which wins over `$DVLPR`, which wins over `"default"`. Both selectors are
/// stripped from the args passed to `parse_command`, so
/// `@work pane zoom --session other` correctly targets `work` and leaves only
/// `["pane", "zoom"]`.
fn strip_session(args: &[String], dvlpr_env: Option<&str>) -> (String, Vec<String>) {
    // Strip any `--session NAME` pair from the tail (records it as the explicit
    // selector); everything else accumulates into `rest`.
    let mut rest = Vec::new();
    let mut explicit: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--session" && i + 1 < args.len() {
            explicit = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    // A leading `@name` (now possibly the first element of `rest`) wins over the
    // explicit `--session`, which wins over `$DVLPR`, which wins over "default".
    if let Some(first) = rest.first() {
        if let Some(name) = first.strip_prefix('@') {
            let name = name.to_string();
            let tail = rest[1..].to_vec();
            return (name, tail);
        }
    }
    let name = explicit
        .or_else(|| dvlpr_env.map(|s| s.to_string()))
        .unwrap_or_else(|| "default".to_string());
    (name, rest)
}

/// Parse the noun-verb args (selector already stripped) into a `ControlCommand`.
fn parse_command(args: &[String]) -> Parsed {
    let usage = |m: &str| Parsed::Usage(format!("dvlpr: {m}\n{USAGE}"));
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        ["window", "new", rest @ ..] => match rest {
            [] => Parsed::Cmd(ControlCommand::WindowNew { name: None }),
            ["--name", name] => Parsed::Cmd(ControlCommand::WindowNew {
                name: Some((*name).to_string()),
            }),
            _ => usage("usage: dvlpr window new [--name NAME]"),
        },
        ["window", "rename", name] => {
            Parsed::Cmd(ControlCommand::WindowRename((*name).to_string()))
        }
        ["window", "close"] => Parsed::Cmd(ControlCommand::WindowClose),
        ["window", "next"] => Parsed::Cmd(ControlCommand::WindowNext),
        ["window", "prev"] => Parsed::Cmd(ControlCommand::WindowPrev),
        ["window", "select", n] => match n.parse::<u8>() {
            Ok(v) if v >= 1 => Parsed::Cmd(ControlCommand::WindowSelect(v)),
            _ => usage("window select expects a positive number (1-based)"),
        },
        ["pane", "split", "right"] => Parsed::Cmd(ControlCommand::PaneSplit(SplitDir::Right)),
        ["pane", "split", "down"] => Parsed::Cmd(ControlCommand::PaneSplit(SplitDir::Down)),
        ["pane", "close"] => Parsed::Cmd(ControlCommand::PaneClose),
        ["pane", "zoom"] => Parsed::Cmd(ControlCommand::PaneZoom),
        ["sidebar", "toggle"] => Parsed::Cmd(ControlCommand::SidebarToggle),
        _ => usage("unknown command"),
    }
}

/// Entry point from `main`. Returns the process exit code.
pub async fn run(args: &[String]) -> i32 {
    let dvlpr_env = std::env::var("DVLPR").ok();
    let (session, rest) = strip_session(args, dvlpr_env.as_deref());

    let cmd = match parse_command(&rest) {
        Parsed::Cmd(c) => c,
        Parsed::Usage(msg) => {
            eprintln!("{msg}");
            return 2;
        }
    };

    // Validate the name first (clean exit 2), then resolve the socket path the
    // same way main.rs's session_socket helper does.
    if let Err(e) = crate::server::socket::validate_session_name(&session) {
        eprintln!("dvlpr: invalid session name '{session}': {e}");
        return 2;
    }
    let dir = crate::server::socket::runtime_dir();
    let socket_path = crate::server::socket::socket_path_in(&dir, &session);

    match crate::client::send_command(&socket_path, cmd).await {
        Ok(reply) if reply.ok => 0,
        Ok(reply) => {
            eprintln!(
                "dvlpr: {}",
                reply.message.unwrap_or_else(|| "command failed".into())
            );
            1
        }
        Err(e)
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused =>
        {
            eprintln!("dvlpr: no running session '{session}'");
            1
        }
        Err(e) => {
            eprintln!("dvlpr: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }
    fn parse(args: &[&str]) -> Parsed {
        parse_command(&v(args))
    }

    #[test]
    fn parses_every_verb() {
        assert_eq!(
            parse(&["window", "new"]),
            Parsed::Cmd(ControlCommand::WindowNew { name: None })
        );
        assert_eq!(
            parse(&["window", "new", "--name", "api"]),
            Parsed::Cmd(ControlCommand::WindowNew {
                name: Some("api".into())
            })
        );
        assert_eq!(
            parse(&["window", "rename", "db"]),
            Parsed::Cmd(ControlCommand::WindowRename("db".into()))
        );
        assert_eq!(
            parse(&["window", "close"]),
            Parsed::Cmd(ControlCommand::WindowClose)
        );
        assert_eq!(
            parse(&["window", "next"]),
            Parsed::Cmd(ControlCommand::WindowNext)
        );
        assert_eq!(
            parse(&["window", "prev"]),
            Parsed::Cmd(ControlCommand::WindowPrev)
        );
        assert_eq!(
            parse(&["window", "select", "2"]),
            Parsed::Cmd(ControlCommand::WindowSelect(2))
        );
        assert_eq!(
            parse(&["pane", "split", "right"]),
            Parsed::Cmd(ControlCommand::PaneSplit(SplitDir::Right))
        );
        assert_eq!(
            parse(&["pane", "split", "down"]),
            Parsed::Cmd(ControlCommand::PaneSplit(SplitDir::Down))
        );
        assert_eq!(
            parse(&["pane", "close"]),
            Parsed::Cmd(ControlCommand::PaneClose)
        );
        assert_eq!(
            parse(&["pane", "zoom"]),
            Parsed::Cmd(ControlCommand::PaneZoom)
        );
        assert_eq!(
            parse(&["sidebar", "toggle"]),
            Parsed::Cmd(ControlCommand::SidebarToggle)
        );
    }

    #[test]
    fn rejects_bad_input_as_usage() {
        assert!(matches!(parse(&["window"]), Parsed::Usage(_)));
        assert!(matches!(parse(&["window", "bogus"]), Parsed::Usage(_)));
        assert!(matches!(parse(&["window", "rename"]), Parsed::Usage(_)));
        assert!(matches!(
            parse(&["window", "select", "x"]),
            Parsed::Usage(_)
        ));
        assert!(matches!(
            parse(&["window", "select", "0"]),
            Parsed::Usage(_)
        ));
        assert!(matches!(parse(&["window", "select"]), Parsed::Usage(_)));
        assert!(matches!(
            parse(&["pane", "split", "sideways"]),
            Parsed::Usage(_)
        ));
        assert!(matches!(parse(&["pane", "split"]), Parsed::Usage(_)));
        assert!(matches!(parse(&["bogus", "verb"]), Parsed::Usage(_)));
        assert!(matches!(
            parse(&["window", "new", "--name"]),
            Parsed::Usage(_)
        ));
    }

    #[test]
    fn resolves_session_precedence() {
        let (name, rest) = strip_session(&v(&["@work", "pane", "split", "right"]), Some("envsess"));
        assert_eq!(name, "work");
        assert_eq!(rest, v(&["pane", "split", "right"]));

        let (name, rest) = strip_session(
            &v(&["pane", "split", "right", "--session", "api"]),
            Some("envsess"),
        );
        assert_eq!(name, "api");
        assert_eq!(rest, v(&["pane", "split", "right"]));

        let (name, _rest) = strip_session(&v(&["pane", "split", "right"]), Some("envsess"));
        assert_eq!(name, "envsess");

        let (name, _rest) = strip_session(&v(&["pane", "split", "right"]), None);
        assert_eq!(name, "default");
    }

    #[test]
    fn at_name_wins_over_trailing_session_and_both_are_stripped() {
        let (name, rest) = strip_session(
            &v(&["@work", "pane", "zoom", "--session", "other"]),
            Some("envsess"),
        );
        assert_eq!(name, "work");
        assert_eq!(rest, v(&["pane", "zoom"]));
    }
}
