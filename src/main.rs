use std::time::Duration;

use dvlpr::client;
use dvlpr::daemon;
use dvlpr::server::{self, socket, ServerConfig};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let mode = std::env::args().nth(1);
    let result = match mode.as_deref() {
        Some("server") => run_server().await,
        _ => run_client().await, // default: attach (auto-spawning a daemon if needed)
    };

    if let Err(e) = result {
        eprintln!("dvlpr: {e}");
        std::process::exit(1);
    }
}

/// Foreground daemon (the process spawned by `spawn_detached_server`).
async fn run_server() -> std::io::Result<()> {
    let config = ServerConfig::for_default_session()?;
    let dir = socket::runtime_dir();
    socket::ensure_runtime_dir(&dir)?;
    let lock_path = dir.join("daemon.lock");
    // Hold the lock for the daemon's lifetime.
    let _lock = match daemon::acquire_instance_lock(&lock_path) {
        Ok(lock) => lock,
        Err(_) => return Ok(()), // another daemon already running; nothing to do.
    };
    server::run(config).await
}

/// Default invocation: connect, auto-spawning a daemon if none is live.
async fn run_client() -> std::io::Result<()> {
    let path = socket::default_socket_path();

    if !socket::is_live(&path).await {
        daemon::spawn_detached_server()?;
        // Wait for the daemon to come up.
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
