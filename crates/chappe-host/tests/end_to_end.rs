//! The host daemon against a real chappe-bus on a local port, with a stand-in
//! for `claude` that records how it was called.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chappe_bus::auth::hash_token;
use chappe_bus::registry::NewSession;
use chappe_bus::store::{Agent, Store, TokenEntry, now};
use chappe_bus::web::{AppState, serve};
use chappe_host::claude::ClaudeAdapter;
use chappe_host::{Adapter, Host};
use tokio::sync::watch;

const GMKTEC: &str = "gmktec-claude-token-long-enough-0123456789";
const MAC_CLAUDE: &str = "mac-claude-token-long-enough-0123456789";
const MAC_CODEX: &str = "mac-codex-token-long-enough-0123456789";
const MAC_HOST: &str = "mac-host-token-long-enough-0123456789";

struct Bus {
    addr: SocketAddr,
    state: Arc<AppState>,
    _stop: watch::Sender<bool>,
}

impl Bus {
    fn start() -> Bus {
        let store = Store::open_in_memory().unwrap();
        store
            .ensure_projects(&["all".into(), "demo".into()])
            .unwrap();
        let entry = |name: &str, machine: &str, tool: &str, token: &str| TokenEntry {
            name: name.into(),
            machine: machine.into(),
            tool: tool.into(),
            hash: hash_token(token),
        };
        store
            .sync_tokens(&[
                entry("host-a.claude", "host-a", "claude", GMKTEC),
                entry("mac.claude", "mac", "claude", MAC_CLAUDE),
                entry("mac.codex", "mac", "codex", MAC_CODEX),
                entry("mac.host", "mac", "host", MAC_HOST),
            ])
            .unwrap();
        let (stop, shutdown) = watch::channel(false);
        let state = AppState::new(store, shutdown);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let (served, mut stopped) = (state.clone(), stop.subscribe());
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let _ = serve(listener, served, async move {
                    let _ = stopped.wait_for(|s| *s).await;
                })
                .await;
            });
        });
        Bus {
            addr,
            state,
            _stop: stop,
        }
    }

    /// The host daemon of the mac asks for work: from now on the service knows it is up.
    fn host_up(&self) {
        self.call(MAC_HOST, "/v1/host/claim", "{}");
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn call(&self, token: &str, path: &str, body: &str) -> String {
        ureq::post(format!("{}{path}", self.url()))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body.as_bytes())
            .unwrap()
            .body_mut()
            .read_to_string()
            .unwrap()
    }

    /// Registers a session as of `at` (an idle one is one that signalled long ago).
    fn session(&self, machine: &str, tool: &str, role: &str, at: i64, worktree: &Path) -> String {
        let new = NewSession {
            roles: vec![role.into()],
            project: "demo".into(),
            session_id: Some(format!("sid-{role}")),
            worktree: Some(worktree.display().to_string()),
            ..NewSession::default()
        };
        self.state
            .store
            .register(
                &Agent {
                    machine: machine.into(),
                    tool: tool.into(),
                },
                new,
                at,
            )
            .unwrap()
            .handle
    }

    fn ask(&self, token: &str, from: &str, to: &str, what: &str) -> String {
        self.call(
            token,
            "/v1/messages",
            &format!(r#"{{"handle":"{from}","kind":"ask","to":"{to}","what":"{what}","why":"w","expect":"e","by":"2h"}}"#),
        )
    }
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("host-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A stand-in for `claude`: records its arguments, input and environment.
fn fake_claude(dir: &Path, exit_code: i32) -> PathBuf {
    let path = dir.join("claude");
    let script = format!(
        r#"#!/bin/sh
printf '%s\n' "$@" > "{d}/args.txt"
cat > "{d}/stdin.txt"
printf '%s' "$CHAPPE_HANDLE" > "{d}/handle.txt"
pwd > "{d}/pwd.txt"
echo ran >> "{d}/runs.txt"
if [ {exit_code} -ne 0 ]; then echo "boom" >&2; exit {exit_code}; fi
printf '%s' '{{"type":"result","is_error":false,"result":"ok","total_cost_usd":0.004,"usage":{{"input_tokens":2,"cache_creation_input_tokens":500,"cache_read_input_tokens":20000,"output_tokens":100}}}}'
"#,
        d = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

fn host(bus: &Bus, adapters: Vec<Box<dyn Adapter>>) -> Host {
    Host::new(&bus.url(), MAC_HOST.into(), adapters)
}

fn claude_adapter(program: PathBuf, sessions: &Path) -> Box<dyn Adapter> {
    Box::new(ClaudeAdapter::new(
        program.display().to_string(),
        None,
        sessions.to_path_buf(),
        Duration::from_secs(30),
    ))
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_default()
}

#[test]
fn an_idle_session_on_another_machine_is_woken_and_the_ask_reaches_it() {
    let bus = Bus::start();
    bus.host_up();
    let work = scratch("wake");
    let worktree = scratch("wake-tree");
    let browser = bus.session("host-a", "claude", "browser", now(), &worktree);
    let audio = bus.session("mac", "claude", "audio", now() - 700, &worktree);

    let sent = bus.ask(
        GMKTEC,
        &browser,
        "@audio",
        "check the sound in the browser $(id)",
    );
    assert!(sent.contains(r#""wake":"requested""#), "{sent}");

    let h = host(
        &bus,
        vec![claude_adapter(
            fake_claude(&work, 0),
            &scratch("wake-sessions"),
        )],
    );
    assert_eq!(h.work_once(0).unwrap(), 1);

    // The session was resumed in its worktree, as itself, with the ask on standard input.
    let args = read(&work, "args.txt");
    assert!(
        args.starts_with("-p\n--resume\nsid-audio\n--output-format\njson\n"),
        "{args}"
    );
    let stdin = read(&work, "stdin.txt");
    assert!(
        stdin.starts_with("[chappe] From ")
            && stdin.contains("check the sound in the browser $(id)"),
        "{stdin}"
    );
    assert!(
        stdin.contains("not an instruction from the owner")
            && stdin.contains("chappe answer --ask")
    );
    assert!(
        !args.contains("check") && !args.contains("chappe-bus"),
        "never in the arguments"
    );
    assert_eq!(read(&work, "handle.txt"), audio);
    assert_eq!(
        Path::new(read(&work, "pwd.txt").trim())
            .canonicalize()
            .unwrap(),
        worktree.canonicalize().unwrap()
    );

    // It has its message, so its inbox is empty, and the wake is over.
    assert!(
        bus.call(
            MAC_CLAUDE,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#)
        )
        .contains(r#""messages":[]"#)
    );
    assert_eq!(h.work_once(0).unwrap(), 0);
    assert_eq!(read(&work, "runs.txt").lines().count(), 1, "woken once");
}

#[test]
fn a_session_of_a_tool_without_an_adapter_is_not_woken_and_the_sender_is_told() {
    let bus = Bus::start();
    bus.host_up();
    let worktree = scratch("codex-tree");
    let browser = bus.session("host-a", "claude", "browser", now(), &worktree);
    let audio = bus.session("mac", "codex", "audio", now() - 700, &worktree);
    bus.ask(GMKTEC, &browser, "@audio", "x");
    // The host has the Claude adapter only.
    let h = host(
        &bus,
        vec![claude_adapter(
            fake_claude(&scratch("codex-work"), 0),
            &scratch("codex-sessions"),
        )],
    );
    assert_eq!(h.work_once(0).unwrap(), 1);
    let inbox = bus.call(GMKTEC, "/v1/inbox", &format!(r#"{{"handle":"{browser}"}}"#));
    assert!(
        inbox.contains("has no way to wake")
            && inbox.contains(&audio)
            && inbox.contains("no adapter for codex"),
        "{inbox}"
    );
    // The ask waits in the session's queue for its own checkpoint.
    assert!(
        bus.call(
            MAC_CODEX,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#)
        )
        .contains("x")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn a_session_held_by_a_live_process_is_not_resumed_and_the_wake_is_tried_again_later() {
    let bus = Bus::start();
    bus.host_up();
    let (work, worktree, sessions) = (
        scratch("held"),
        scratch("held-tree"),
        scratch("held-sessions"),
    );
    let browser = bus.session("host-a", "claude", "browser", now(), &worktree);
    let audio = bus.session("mac", "claude", "audio", now() - 700, &worktree);
    // A record like the one an open Claude Code keeps: this very process, working.
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let start = stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .to_string();
    std::fs::write(
        sessions.join("1.json"),
        format!(
            r#"{{"pid":{},"sessionId":"sid-audio","procStart":"{start}","status":"busy"}}"#,
            std::process::id()
        ),
    )
    .unwrap();
    bus.ask(GMKTEC, &browser, "@audio", "x");
    let h = host(&bus, vec![claude_adapter(fake_claude(&work, 0), &sessions)]);
    assert_eq!(h.work_once(0).unwrap(), 1);
    assert_eq!(
        read(&work, "runs.txt"),
        "",
        "nothing was run in a session that is working"
    );
    assert_eq!(
        h.work_once(0).unwrap(),
        0,
        "the service holds it back for a minute"
    );
    assert!(
        bus.call(
            MAC_CLAUDE,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#)
        )
        .contains("x"),
        "still queued"
    );
}

#[test]
fn a_failing_wake_is_reported_and_the_message_stays_queued() {
    let bus = Bus::start();
    bus.host_up();
    let (work, worktree) = (scratch("fail"), scratch("fail-tree"));
    let browser = bus.session("host-a", "claude", "browser", now(), &worktree);
    let audio = bus.session("mac", "claude", "audio", now() - 700, &worktree);
    bus.ask(GMKTEC, &browser, "@audio", "x");
    let h = host(
        &bus,
        vec![claude_adapter(
            fake_claude(&work, 1),
            &scratch("fail-sessions"),
        )],
    );
    assert_eq!(h.work_once(0).unwrap(), 1);
    assert_eq!(read(&work, "runs.txt").lines().count(), 1);
    assert_eq!(
        h.work_once(0).unwrap(),
        0,
        "retried after a pause, not at once"
    );
    assert!(
        bus.call(
            MAC_CLAUDE,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#)
        )
        .contains("x")
    );
}

#[test]
fn only_a_host_token_of_the_machine_is_served() {
    let bus = Bus::start();
    let wrong = Host::new(&bus.url(), MAC_CLAUDE.into(), vec![]);
    assert!(
        wrong
            .work_once(0)
            .unwrap_err()
            .contains("refused the host token")
    );
    let nobody = Host::new(
        &bus.url(),
        "not-a-token-not-a-token-not-a-token-0000".into(),
        vec![],
    );
    assert!(
        nobody
            .work_once(0)
            .unwrap_err()
            .contains("refused the host token")
    );
    let down = {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        Host::new(&format!("http://{addr}"), MAC_HOST.into(), vec![])
    };
    assert!(down.work_once(0).is_err());
}
