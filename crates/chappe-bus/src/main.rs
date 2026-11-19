//! `chappe-bus` serves the page and the agent interface.
//! `chappe-bus backup <file>` writes a consistent copy of the database.

use std::path::Path;
use std::process::ExitCode;

use chappe_bus::auth;
use chappe_bus::config::Config;
use chappe_bus::store::Store;
use chappe_bus::web::{AppState, serve as serve_web};
use tokio::net::TcpListener;
use tokio::sync::watch;

const USAGE: &str = "usage: chappe-bus            run the service (settings in CHAPPE_* variables)\n       chappe-bus backup FILE  copy the database to FILE, which must not exist";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => serve().await,
        ["backup", dest] => backup(Path::new(dest)),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("chappe-bus: {e}");
            ExitCode::FAILURE
        }
    }
}

fn backup(dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    Store::open(&config.db)?.backup_to(dest)?;
    println!("database copied to {}", dest.display());
    Ok(())
}

async fn serve() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let store = Store::open(&config.db)?;
    store.ensure_projects(&config.projects)?;
    let tokens = match &config.tokens_dir {
        Some(dir) => {
            let sync = store.sync_tokens(&auth::load_dir(dir)?)?;
            format!("tokens +{} ~{} -{}", sync.added, sync.rotated, sync.removed)
        }
        None => "no token directory, agents cannot authenticate".to_string(),
    };

    if let Some(url) = &config.forge_url {
        chappe_bus::links::set_forge_url(url);
    }
    let (stop, shutdown) = watch::channel(false);
    let state = AppState::with_webhook_secret(store, shutdown, config.webhook_secret.clone());
    let listener = TcpListener::bind(config.listen).await?;
    eprintln!(
        "chappe-bus: listening on {}, database {}, schema v{}, projects {}, {tokens}, forge webhooks {}",
        config.listen,
        config.db.display(),
        state.store.schema_version()?,
        config.projects.join(","),
        if config.webhook_secret.is_some() {
            "on"
        } else {
            "off"
        },
    );
    tokio::spawn(digests(state.clone()));
    serve_web(listener, state, async move {
        wait_for_signal().await;
        eprintln!("chappe-bus: shutting down");
        let _ = stop.send(true);
    })
    .await?;
    Ok(())
}

/// Writes the digest of a finished day once, a few minutes into the next one.
async fn digests(state: chappe_bus::web::Shared) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
    loop {
        tick.tick().await;
        match state.store.post_digests(chappe_bus::store::now()) {
            Ok(pages) => pages.into_iter().for_each(|p| state.notify(p)),
            Err(e) => eprintln!("chappe-bus: digest: {e}"),
        }
    }
}

async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        // Without signal handlers the process is still stopped by SIGKILL.
        return std::future::pending().await;
    };
    tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
}
