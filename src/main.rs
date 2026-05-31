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
    let cmd = parse_args(&args);
    if let Some(msg) = nested_block(&cmd, std::env::var("DVLPR").ok().as_deref()) {
        eprintln!("{msg}");
        std::process::exit(1);
    }
    let result = match cmd {
        Cmd::Server { session } => run_server(&session).await,
        Cmd::Run { session } => run_or_attach(&session).await,
        Cmd::Attach { session } => attach_existing(&session).await,
        Cmd::Ls => list_sessions().await,
        Cmd::Kill { session } => kill_session(&session).await,
        Cmd::Ssh {
            destination,
            session,
        } => run_ssh(&destination, session.as_deref()),
        Cmd::Help => {
            print!("{}", dvlpr::help::cli_help());
            std::process::exit(0);
        }
        Cmd::Version => {
            println!(
                "dvlpr {} ({})",
                env!("CARGO_PKG_VERSION"),
                env!("DVLPR_TARGET")
            );
            Ok(())
        }
        Cmd::Update => {
            let code = dvlpr::update::run();
            std::process::exit(code);
        }
        Cmd::Usage(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("dvlpr: {e}");
        std::process::exit(1);
    }
    // Exit explicitly instead of falling through to the `#[tokio::main]` runtime
    // drop. `tokio::io::stdin()` runs its `read()` on a blocking-pool thread that
    // can't be cancelled when the client's select! loop exits on detach; dropping
    // the runtime joins that thread and would hang until the parked `read()`
    // returns (which, with the terminal already restored to canonical mode, needs
    // an Enter keypress). The scoped terminal guards have already restored state
    // by the time any client command returns, so there is nothing left to flush.
    std::process::exit(0);
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
    /// `dvlpr ssh <destination> [session]` — exec `ssh -t <dest> dvlpr [session]`
    Ssh {
        destination: String,
        session: Option<String>,
    },
    /// `dvlpr -h` / `dvlpr --help` / `dvlpr help` — print command list (exit 0)
    Help,
    /// `dvlpr --version` / `-V` — prints `dvlpr <CARGO_PKG_VERSION> (<DVLPR_TARGET>)`.
    Version,
    /// `dvlpr update` — fetch the latest GH release and replace this binary.
    Update,
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
    // NOTE: the in-app help overlay's Commands tab mirrors these subcommands in
    // `help::COMMAND_ROWS` (src/help/mod.rs). When you add/rename/remove a
    // subcommand here, update that table (and its coverage test) to match.
    const USAGE: &str =
        "usage: dvlpr [<name>] | new -s <name> | attach -t <name> | ssh <dest> [name] | ls | kill -t <name> | update | --version";
    // `-h`/`--help` anywhere, or a bare `help` subcommand, prints the command
    // list. Checked before routing so `dvlpr ls --help` shows help, not an error.
    if args.iter().any(|a| a == "-h" || a == "--help")
        || args.first().map(String::as_str) == Some("help")
    {
        return Cmd::Help;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        return Cmd::Version;
    }
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
        Some("ssh") => match &args[1..] {
            [dest] if !dest.starts_with('-') => Cmd::Ssh {
                destination: dest.clone(),
                session: None,
            },
            [dest, session] if !dest.starts_with('-') && !session.starts_with('-') => Cmd::Ssh {
                destination: dest.clone(),
                session: Some(session.clone()),
            },
            _ => Cmd::Usage("usage: dvlpr ssh <destination> [session]".into()),
        },
        Some("update") => match args.len() {
            1 => Cmd::Update,
            _ => Cmd::Usage("usage: dvlpr update".into()),
        },
        // `version` is not a subcommand — use `-V` or `--version`.
        Some("version") => {
            Cmd::Usage("use `dvlpr --version` or `dvlpr -V` (no subcommand alias)".into())
        }
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

/// Returns an error message if `cmd` must not run inside an existing dvlpr
/// session. `dvlpr_env` is the value of `$DVLPR` (`None` when not nested).
/// Only the interactive create/attach commands are refused; `ls`/`kill`/`ssh`/
/// `server` are safe to run from inside a session.
fn nested_block(cmd: &Cmd, dvlpr_env: Option<&str>) -> Option<String> {
    let parent = dvlpr_env?; // not nested -> always allow
    match cmd {
        Cmd::Run { .. } | Cmd::Attach { .. } => Some(format!(
            "dvlpr: refusing to open a nested session (already inside '{parent}'); \
             unset DVLPR to force"
        )),
        _ => None,
    }
}

/// Pure: build the argv passed to `ssh`. `-t` forces remote PTY allocation so the
/// remote client's raw mode and `terminal::size()` work; the remote command is
/// `dvlpr [session]` (bare `dvlpr <name>` is create-or-attach).
fn build_ssh_argv(destination: &str, session: Option<&str>) -> Vec<String> {
    let mut v = vec!["-t".into(), destination.into(), "dvlpr".into()];
    if let Some(s) = session {
        v.push(s.into());
    }
    v
}

/// Exec `ssh -t <destination> dvlpr [session]`, replacing this process. Clears
/// `$DVLPR` so an ssh `SendEnv`/remote `AcceptEnv` cannot make the remote `dvlpr`
/// falsely refuse as nested. Returns only on failure (validation or exec error).
fn run_ssh(destination: &str, session: Option<&str>) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    if let Some(s) = session {
        socket::validate_session_name(s)?;
    }
    let err = std::process::Command::new("ssh")
        .env_remove("DVLPR")
        .args(build_ssh_argv(destination, session))
        .exec();
    // `exec` returns only on failure; surface ssh-not-found etc. as an error so
    // main()'s `Err` path prints it and exits non-zero.
    Err(std::io::Error::other(format!("failed to exec ssh: {err}")))
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
        let snap_path = dvlpr::persist::snapshot_path(session);
        let restore = match dvlpr::persist::load(&snap_path) {
            Some(snap) => {
                let plan = dvlpr::persist::plan_restore(&snap);
                if prompt_restore(session, &plan) {
                    println!("Restoring {}.", plan.summary());
                    true
                } else {
                    dvlpr::persist::archive(&snap_path);
                    false
                }
            }
            None => false,
        };
        daemon::spawn_detached_server(session, restore)?;
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

/// Prompt `Found a saved layout … Restore? [Y/n]`. Default Yes (Enter). A
/// non-interactive stdin (not a TTY) returns Yes without blocking.
fn prompt_restore(session: &str, plan: &dvlpr::persist::RestorePlan) -> bool {
    use std::io::IsTerminal;
    eprint!(
        "Found a saved layout for '{session}' ({}). Restore? [Y/n] ",
        plan.summary()
    );
    let _ = std::io::Write::flush(&mut std::io::stderr());
    if !std::io::stdin().is_terminal() {
        eprintln!("(non-interactive: restoring)");
        return true;
    }
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return true;
    }
    let ans = line.trim().to_ascii_lowercase();
    ans.is_empty() || ans == "y" || ans == "yes"
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
        // Fold the spacing into the marker so an unattached row has no trailing space.
        let attached = if info.clients > 0 {
            "   (attached)"
        } else {
            ""
        };
        println!("{name}   {} window(s){attached}", info.windows);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_ssh_argv, nested_block, parse_args, Cmd};

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn nested_block_refuses_run_and_attach_only_when_env_set() {
        let run = Cmd::Run {
            session: "x".into(),
        };
        let attach = Cmd::Attach {
            session: "x".into(),
        };
        let ls = Cmd::Ls;
        let kill = Cmd::Kill {
            session: "x".into(),
        };
        let server = Cmd::Server {
            session: "x".into(),
        };
        let ssh = Cmd::Ssh {
            destination: "host".into(),
            session: None,
        };

        // Not nested ($DVLPR unset): everything is allowed.
        for c in [&run, &attach, &ls, &kill, &server, &ssh] {
            assert_eq!(nested_block(c, None), None);
        }

        // Nested: only Run and Attach are refused.
        assert!(nested_block(&run, Some("parent")).is_some());
        assert!(nested_block(&attach, Some("parent")).is_some());
        assert_eq!(nested_block(&ls, Some("parent")), None);
        assert_eq!(nested_block(&kill, Some("parent")), None);
        assert_eq!(nested_block(&server, Some("parent")), None);

        // The message names the parent session.
        assert!(nested_block(&run, Some("parent"))
            .unwrap()
            .contains("parent"));
        assert!(nested_block(&attach, Some("parent"))
            .unwrap()
            .contains("parent"));
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
    fn parses_help_flags_and_subcommand() {
        assert_eq!(parse_args(&v(&["-h"])), Cmd::Help);
        assert_eq!(parse_args(&v(&["--help"])), Cmd::Help);
        assert_eq!(parse_args(&v(&["help"])), Cmd::Help);
        // A help flag anywhere wins over routing, so it never errors out.
        assert_eq!(parse_args(&v(&["ls", "--help"])), Cmd::Help);
        assert_eq!(parse_args(&v(&["attach", "-h"])), Cmd::Help);
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

    #[test]
    fn build_ssh_argv_shapes_the_remote_command() {
        assert_eq!(
            build_ssh_argv("host", None),
            vec!["-t".to_string(), "host".into(), "dvlpr".into()]
        );
        assert_eq!(
            build_ssh_argv("me@host", Some("work")),
            vec![
                "-t".to_string(),
                "me@host".into(),
                "dvlpr".into(),
                "work".into()
            ]
        );
    }

    #[test]
    fn parses_ssh_command() {
        assert_eq!(
            parse_args(&v(&["ssh", "host"])),
            Cmd::Ssh {
                destination: "host".into(),
                session: None
            }
        );
        assert_eq!(
            parse_args(&v(&["ssh", "me@host", "work"])),
            Cmd::Ssh {
                destination: "me@host".into(),
                session: Some("work".into())
            }
        );
        // No destination, leading-dash destination, leading-dash session, and extra args are usage errors.
        assert!(matches!(parse_args(&v(&["ssh"])), Cmd::Usage(_)));
        assert!(matches!(parse_args(&v(&["ssh", "-X"])), Cmd::Usage(_)));
        assert!(matches!(
            parse_args(&v(&["ssh", "host", "-x"])),
            Cmd::Usage(_)
        ));
        assert!(matches!(
            parse_args(&v(&["ssh", "host", "work", "extra"])),
            Cmd::Usage(_)
        ));
    }

    #[test]
    fn nested_block_allows_ssh_when_env_set() {
        let ssh = Cmd::Ssh {
            destination: "host".into(),
            session: None,
        };
        assert_eq!(nested_block(&ssh, Some("parent")), None);
    }

    #[test]
    fn parse_args_version_flag_routes_to_version_cmd() {
        assert_eq!(parse_args(&v(&["-V"])), Cmd::Version);
        assert_eq!(parse_args(&v(&["--version"])), Cmd::Version);
        // `version` is NOT a subcommand alias — only the flag forms are accepted,
        // to keep parity with how `--help` is routed.
        assert!(matches!(parse_args(&v(&["version"])), Cmd::Usage(_)));
    }

    #[test]
    fn parse_args_update_routes_to_update_cmd() {
        assert_eq!(parse_args(&v(&["update"])), Cmd::Update);
        // Extra positional after `update` is a usage error.
        assert!(matches!(
            parse_args(&v(&["update", "extra"])),
            Cmd::Usage(_)
        ));
        // `-h` after `update` still routes to Help (Help check runs first).
        assert_eq!(parse_args(&v(&["update", "-h"])), Cmd::Help);
    }
}
