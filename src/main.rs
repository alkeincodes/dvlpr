use std::time::Duration;

use dvlpr::client;
use dvlpr::daemon;
use dvlpr::server::{self, socket, ServerConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match parse_args(&args) {
        Cmd::Server { session } => run_server(&session).await,
        Cmd::Run { session } => run_or_attach(&session).await,
        Cmd::Attach { session } => attach_existing(&session).await,
        Cmd::Ls => list_sessions().await,
        Cmd::Kill { session } => kill_session(&session).await,
        Cmd::Usage(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("dvlpr: {e}");
        std::process::exit(1);
    }
}

/// A parsed CLI command.
#[derive(Debug, PartialEq, Eq)]
enum Cmd {
    /// create-or-attach (bare `dvlpr`, `dvlpr <name>`, `dvlpr new -s <name>`)
    Run { session: String },
    /// `dvlpr attach -t <name>` / `dvlpr a -t <name>` — attach; error if missing
    Attach { session: String },
    /// `dvlpr ls`
    Ls,
    /// `dvlpr kill -t <name>`
    Kill { session: String },
    /// `dvlpr server [name]` — internal daemon entrypoint
    Server { session: String },
    /// A usage error to print on stderr (exit 2)
    Usage(String),
}

/// Pure routing of argv (already stripped of argv[0]) to a `Cmd`. Name *validation*
/// happens in the command handlers, not here.
fn parse_args(args: &[String]) -> Cmd {
    let flag = |args: &[String], flag: &str| -> Option<String> {
        // expects args == [flag, value]
        match args {
            [f, value] if f == flag => Some(value.clone()),
            _ => None,
        }
    };
    const USAGE: &str =
        "usage: dvlpr [<name>] | new -s <name> | attach -t <name> | ls | kill -t <name>";
    match args.first().map(String::as_str) {
        None => Cmd::Run {
            session: "default".into(),
        },
        Some("server") => match args.len() {
            1 => Cmd::Server {
                session: "default".into(),
            },
            2 => Cmd::Server {
                session: args[1].clone(),
            },
            _ => Cmd::Usage("usage: dvlpr server [name]".into()),
        },
        Some("ls") => {
            if args.len() == 1 {
                Cmd::Ls
            } else {
                Cmd::Usage("usage: dvlpr ls".into())
            }
        }
        Some("new") => match flag(&args[1..], "-s") {
            Some(name) => Cmd::Run { session: name },
            None => Cmd::Usage("usage: dvlpr new -s <name>".into()),
        },
        Some("attach") | Some("a") => match flag(&args[1..], "-t") {
            Some(name) => Cmd::Attach { session: name },
            None => Cmd::Usage("usage: dvlpr attach -t <name>".into()),
        },
        Some("kill") => match flag(&args[1..], "-t") {
            Some(name) => Cmd::Kill { session: name },
            None => Cmd::Usage("usage: dvlpr kill -t <name>".into()),
        },
        // A single bare token is a session name (create-or-attach shorthand); extra
        // positional junk is a usage error rather than a silently-ignored typo.
        Some(name) => {
            if args.len() == 1 {
                Cmd::Run {
                    session: name.to_string(),
                }
            } else {
                Cmd::Usage(USAGE.into())
            }
        }
    }
}

/// Foreground daemon (the process spawned by `spawn_detached_server`).
async fn run_server(session: &str) -> std::io::Result<()> {
    let config = ServerConfig::for_session(session)?;
    let lock_path = socket::lock_path_in(&socket::runtime_dir(), session);
    let _lock = match daemon::acquire_instance_lock(&lock_path) {
        Ok(lock) => lock,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
        Err(e) => return Err(e),
    };
    server::run(config).await
}

/// Resolve a validated session's socket path under the runtime dir.
fn session_socket(session: &str) -> std::io::Result<std::path::PathBuf> {
    socket::validate_session_name(session)?;
    let dir = socket::runtime_dir();
    socket::ensure_runtime_dir(&dir)?;
    Ok(socket::socket_path_in(&dir, session))
}

/// create-or-attach: attach if live, else spawn the daemon and attach.
async fn run_or_attach(session: &str) -> std::io::Result<()> {
    let path = session_socket(session)?;
    if !socket::is_live(&path).await {
        daemon::spawn_detached_server(session)?;
        let mut up = false;
        for _ in 0..100 {
            if socket::is_live(&path).await {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !up {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "daemon did not start within timeout",
            ));
        }
    }
    client::attach(&path).await
}

/// attach an existing session; error if it is not live.
async fn attach_existing(session: &str) -> std::io::Result<()> {
    let path = session_socket(session)?;
    if !socket::is_live(&path).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session named '{session}'"),
        ));
    }
    client::attach(&path).await
}

/// kill a session by name.
async fn kill_session(session: &str) -> std::io::Result<()> {
    let path = session_socket(session)?;
    if !socket::is_live(&path).await {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("no session named '{session}'"),
        ));
    }
    client::send_kill(&path).await
}

/// list live sessions with window counts. Skips (never unlinks) non-live `.sock` files.
async fn list_sessions() -> std::io::Result<()> {
    let dir = socket::runtime_dir();
    socket::ensure_runtime_dir(&dir)?;
    let mut rows: Vec<(String, dvlpr::protocol::StatusInfo)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Some(session) = name.strip_suffix(".sock") else {
                continue;
            };
            let path = socket::socket_path_in(&dir, session);
            if !socket::is_live(&path).await {
                continue; // skip; do NOT unlink (would race a starting daemon)
            }
            if let Ok(info) = client::query_status(&path).await {
                rows.push((session.to_string(), info));
            }
        }
    }
    if rows.is_empty() {
        println!("no sessions");
        return Ok(());
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, info) in rows {
        let attached = if info.clients > 0 { "(attached)" } else { "" };
        println!("{name}   {} window(s)   {attached}", info.windows);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_args, Cmd};

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_args(&v(&[])),
            Cmd::Run {
                session: "default".into()
            }
        );
        assert_eq!(
            parse_args(&v(&["work"])),
            Cmd::Run {
                session: "work".into()
            }
        );
        assert_eq!(
            parse_args(&v(&["new", "-s", "work"])),
            Cmd::Run {
                session: "work".into()
            }
        );
        assert_eq!(
            parse_args(&v(&["attach", "-t", "work"])),
            Cmd::Attach {
                session: "work".into()
            }
        );
        assert_eq!(
            parse_args(&v(&["a", "-t", "work"])),
            Cmd::Attach {
                session: "work".into()
            }
        );
        assert_eq!(parse_args(&v(&["ls"])), Cmd::Ls);
        assert_eq!(
            parse_args(&v(&["kill", "-t", "work"])),
            Cmd::Kill {
                session: "work".into()
            }
        );
        assert_eq!(
            parse_args(&v(&["server"])),
            Cmd::Server {
                session: "default".into()
            }
        );
        assert_eq!(
            parse_args(&v(&["server", "work"])),
            Cmd::Server {
                session: "work".into()
            }
        );
    }

    #[test]
    fn missing_flag_values_are_usage_errors() {
        assert!(matches!(parse_args(&v(&["new"])), Cmd::Usage(_)));
        assert!(matches!(parse_args(&v(&["new", "-s"])), Cmd::Usage(_)));
        assert!(matches!(parse_args(&v(&["attach"])), Cmd::Usage(_)));
        assert!(matches!(parse_args(&v(&["attach", "-t"])), Cmd::Usage(_)));
        assert!(matches!(parse_args(&v(&["kill"])), Cmd::Usage(_)));
    }

    #[test]
    fn extra_positional_args_are_usage_errors() {
        assert!(matches!(parse_args(&v(&["work", "extra"])), Cmd::Usage(_)));
        assert!(matches!(parse_args(&v(&["ls", "x"])), Cmd::Usage(_)));
        assert!(matches!(
            parse_args(&v(&["server", "a", "b"])),
            Cmd::Usage(_)
        ));
        assert!(matches!(
            parse_args(&v(&["new", "-s", "a", "b"])),
            Cmd::Usage(_)
        ));
    }
}
