//! `chappe-host`: run it as a user service of the machine. Settings are
//! `CHAPPE_*` variables; see docs-site, "Agent Bus".

use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

use chappe_host::claude::ClaudeAdapter;
use chappe_host::codex::CodexAdapter;
use chappe_host::{Adapter, Config, Host};

fn main() -> ExitCode {
    let config = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("chappe-host: {e}");
            return ExitCode::FAILURE;
        }
    };
    let adapters: Vec<Box<dyn Adapter>> = vec![
        Box::new(ClaudeAdapter::new(
            config.claude_program.clone(),
            config.claude_model.clone(),
            config.claude_sessions.clone(),
            config.wake_timeout,
        )),
        Box::new(CodexAdapter::new(
            config.codex_program.clone(),
            config.codex_model.clone(),
            config.codex_locks.clone(),
            config.wake_timeout,
        )),
    ];
    eprintln!(
        "chappe-host: serving {} for the tools: claude, codex",
        config.url
    );
    // The service manager stops the process with a signal; a wake in progress is
    // leased on the service for ten minutes and then taken again.
    Host::new(&config.url, config.token, adapters).run(&AtomicBool::new(false));
    ExitCode::SUCCESS
}
