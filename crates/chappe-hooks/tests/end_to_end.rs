//! `chappe` against a real chappe-bus on a local port: the scenarios of stage 1.

use std::collections::HashMap;
use std::net::SocketAddr;

use chappe_bus::auth::hash_token;
use chappe_bus::store::{Store, TokenEntry};
use chappe_bus::web::{AppState, serve};
use chappe_hooks::hook::run_hook;
use chappe_hooks::{Env, run};
use tokio::sync::watch;

const DEVBOX: &str = "devbox-token-long-enough-0123456789abcdef";
const MAC: &str = "mac-token-long-enough-0123456789-ABCDEF";

struct Bus {
    addr: SocketAddr,
    _stop: watch::Sender<bool>,
}

impl Bus {
    fn start() -> Bus {
        let store = Store::open_in_memory().unwrap();
        store
            .ensure_projects(&["all".into(), "demo".into()])
            .unwrap();
        store
            .create_topic("all", "laptop", None, "owner", 0)
            .unwrap();
        let entry = |name: &str, machine: &str, tool: &str, token: &str| TokenEntry {
            name: name.into(),
            machine: machine.into(),
            tool: tool.into(),
            hash: hash_token(token),
        };
        store
            .sync_tokens(&[
                entry("host-a.claude", "host-a", "claude", DEVBOX),
                entry("mac.codex", "mac", "codex", MAC),
            ])
            .unwrap();
        let (stop, shutdown) = watch::channel(false);
        let state = AppState::new(store, shutdown);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let mut stopped = stop.subscribe();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let _ = serve(listener, state, async move {
                    let _ = stopped.wait_for(|s| *s).await;
                })
                .await;
            });
        });
        Bus { addr, _stop: stop }
    }

    fn env(&self, token: Option<&str>) -> Env {
        let mut vars = HashMap::from([("CHAPPE_URL".to_string(), format!("http://{}", self.addr))]);
        if let Some(t) = token {
            vars.insert("CHAPPE_TOKEN".into(), t.into());
        } else {
            vars.insert("HOME".into(), "/nonexistent".into());
        }
        Env {
            vars,
            cwd: Some("/work/tree".into()),
        }
    }
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

/// Splits a command line like a shell does for double quotes.
fn words(line: &str) -> Vec<String> {
    let (mut out, mut word, mut quoted, mut started) = (Vec::new(), String::new(), false, false);
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

fn chappe(env: &Env, line: &str) -> Out {
    let args = words(line);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, env, &mut out, &mut err);
    Out {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn get(bus: &Bus, path: &str) -> String {
    ureq::get(format!("http://{}{path}", bus.addr))
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap()
}

#[test]
fn two_machines_register_are_seen_and_leave() {
    let bus = Bus::start();
    let (devbox, mac) = (bus.env(Some(DEVBOX)), bus.env(Some(MAC)));

    let a = chappe(
        &devbox,
        "register --role browser --project demo --issue 471 --branch fix/sound",
    );
    assert_eq!(a.code, 0, "{}", a.err);
    let browser = a.out.trim().to_string();
    assert!(browser.starts_with("browser-"), "{browser}");
    assert!(
        a.err.contains(&browser),
        "register tells how to use the handle"
    );
    let b = chappe(&mac, "register --role audio,desktop --project demo");
    assert_eq!(b.code, 0, "{}", b.err);
    let audio = b.out.trim().to_string();

    // `who` from either machine sees both, with the agent taken from the token.
    for env in [&devbox, &mac] {
        let who = chappe(env, "who");
        assert_eq!(who.code, 0, "{}", who.err);
        assert!(
            who.out.contains(&browser) && who.out.contains(&audio),
            "{}",
            who.out
        );
        assert!(
            who.out.contains("claude@host-a") && who.out.contains("codex@mac"),
            "{}",
            who.out
        );
        assert!(who.out.contains("active"));
    }
    assert!(chappe(&mac, "who --role browser").out.contains(&browser));
    assert!(!chappe(&mac, "who --role browser").out.contains(&audio));
    assert!(
        chappe(&mac, "who --project nothing")
            .out
            .contains("nobody is registered")
    );

    // The owner's board shows both and the rooms stay empty.
    let board = get(&bus, "/board");
    assert!(
        board.contains(&browser) && board.contains(&audio) && board.contains("fix/sound"),
        "{board}"
    );
    assert!(get(&bus, "/p/all/general/messages").contains("Nothing here yet"));
    assert!(get(&bus, "/p/demo/general/messages").contains("Nothing here yet"));

    // Beat, with the handle from the flag or the environment.
    assert_eq!(
        chappe(&devbox, &format!("beat --handle {browser}")).out,
        "ok\n"
    );
    let mut with_env = bus.env(Some(MAC));
    with_env.vars.insert("CHAPPE_HANDLE".into(), audio.clone());
    assert_eq!(chappe(&with_env, "beat").out, "ok\n");

    // Nobody can end or keep alive another machine's session.
    let steal = chappe(&mac, &format!("leave --handle {browser}"));
    assert_eq!(steal.code, 1);
    assert!(
        steal.err.contains("belongs to another machine"),
        "{}",
        steal.err
    );
    assert!(chappe(&mac, "who").out.contains(&browser));

    // Leave: the record disappears from `who` and from the board.
    assert_eq!(
        chappe(&devbox, &format!("leave --handle {browser}")).out,
        "ok\n"
    );
    assert!(!chappe(&mac, "who").out.contains(&browser));
    assert!(!get(&bus, "/board").contains(&browser));
    assert!(get(&bus, "/board").contains(&audio));
    let late = chappe(&devbox, &format!("beat --handle {browser}"));
    assert_eq!(late.code, 1);
    assert!(late.err.contains("register again"), "{}", late.err);
}

#[test]
fn mistakes_are_explained_and_the_token_is_never_printed() {
    let bus = Bus::start();
    let devbox = bus.env(Some(DEVBOX));

    let wrong = chappe(
        &bus.env(Some("wrong-token-wrong-token-wrong-token-0000")),
        "who",
    );
    assert_eq!(wrong.code, 1);
    assert!(wrong.err.contains("token was rejected"));

    let none = chappe(&bus.env(None), "who");
    assert_eq!(none.code, 1);
    assert!(none.err.contains("no token"));

    let bad_role = chappe(&devbox, "register --role Bad_Role --project demo");
    assert_eq!(bad_role.code, 1);
    assert!(bad_role.err.contains("role"), "{}", bad_role.err);

    let no_project = chappe(&devbox, "register --role audio");
    assert_eq!(no_project.code, 2);
    assert!(no_project.err.contains("--project"));

    let secret = format!(
        "register --role audio --project demo --issue {}",
        "ab".repeat(20)
    );
    let leaked = chappe(&devbox, &secret);
    assert_eq!(leaked.code, 1);
    assert!(leaked.err.contains("looks like a secret"), "{}", leaked.err);

    for run in [&wrong, &none, &bad_role, &no_project, &leaked] {
        for token in [DEVBOX, MAC, "wrong-token-wrong-token-wrong-token-0000"] {
            assert!(!run.out.contains(token) && !run.err.contains(token));
        }
    }
    assert_eq!(
        chappe(&devbox, "who").out,
        "nobody is registered\n",
        "nothing was stored"
    );
}

#[test]
fn registering_again_with_the_same_session_id_returns_the_same_handle() {
    // A tool whose start hook fires twice must not appear twice on the board.
    let bus = Bus::start();
    let env = bus.env(Some(DEVBOX));
    let line = "register --role audio --project demo --session-id s-1";
    let first = chappe(&env, line).out;
    let second = chappe(&env, line).out;
    assert_eq!(first, second);
    assert_eq!(chappe(&env, "who").out.matches("audio-").count(), 1);
}

#[test]
fn agents_make_topics_for_parts_of_the_system_and_the_owner_sees_them() {
    let bus = Bus::start();
    let (devbox, mac) = (bus.env(Some(DEVBOX)), bus.env(Some(MAC)));
    let handle = chappe(&devbox, "register --role audio --project demo")
        .out
        .trim()
        .to_string();

    // Every project has general; a session makes more for its own project.
    let before = chappe(&mac, "topics --project demo");
    assert_eq!(before.code, 0, "{}", before.err);
    assert!(
        before.out.contains("general") && !before.out.contains("audio-driver"),
        "{}",
        before.out
    );

    let made = chappe(
        &devbox,
        &format!("topic create audio-driver --about driver --handle {handle}"),
    );
    assert_eq!(made.code, 0, "{}", made.err);
    assert_eq!(made.out, "created #audio-driver in demo\n");

    // Visible to another machine, in the sidebar and as a page of its own.
    assert!(
        chappe(&mac, "topics --project demo")
            .out
            .contains("audio-driver")
    );
    assert!(
        !chappe(&mac, "topics --project all")
            .out
            .contains("audio-driver")
    );
    assert!(get(&bus, "/nav").contains("/p/demo/audio-driver"));
    assert!(get(&bus, "/p/demo/audio-driver").contains("<h1># audio-driver</h1>"));

    // The same name twice, a bad name, general, and another machine's session.
    let twice = chappe(
        &devbox,
        &format!("topic create audio-driver --handle {handle}"),
    );
    assert_eq!(twice.code, 1);
    assert!(twice.err.contains("already exists"), "{}", twice.err);
    assert_eq!(
        chappe(&devbox, &format!("topic create a/b --handle {handle}")).code,
        1
    );
    assert_eq!(
        chappe(&devbox, &format!("topic create general --handle {handle}")).code,
        1
    );
    let stolen = chappe(&mac, &format!("topic create sneaky --handle {handle}"));
    assert_eq!(stolen.code, 1);
    assert!(
        stolen.err.contains("belongs to another machine"),
        "{}",
        stolen.err
    );
    assert_eq!(
        chappe(&devbox, "topic create x").code,
        2,
        "a handle is needed"
    );
}

#[test]
fn a_service_that_is_down_does_not_fail_the_agent() {
    // Nothing listens here.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let env = Env {
        vars: HashMap::from([
            ("CHAPPE_URL".to_string(), format!("http://{addr}")),
            ("CHAPPE_TOKEN".to_string(), DEVBOX.to_string()),
        ]),
        cwd: None,
    };
    for line in [
        "who",
        "register --role audio --project demo",
        "beat --handle audio-0000",
        "leave --handle audio-0000",
    ] {
        let r = chappe(&env, line);
        assert_eq!(r.code, 0, "{line}: {}", r.err);
        assert!(
            r.err.contains("not reachable") && r.err.contains("carry on"),
            "{line}: {}",
            r.err
        );
        assert!(!r.err.contains(DEVBOX));
    }
}

#[test]
fn usage_errors_exit_with_status_two() {
    let bus = Bus::start();
    let env = bus.env(Some(DEVBOX));
    for line in ["", "hello", "beat --nope x", "beat"] {
        assert_eq!(chappe(&env, line).code, 2, "{line:?}");
    }
    let help = chappe(&env, "help");
    assert_eq!(help.code, 0);
    assert!(help.out.contains("register") && help.out.contains("leave"));
}

/// The handle that `register` printed.
fn register(env: &Env, role: &str, project: &str) -> String {
    let r = chappe(env, &format!("register --role {role} --project {project}"));
    assert_eq!(r.code, 0, "{}", r.err);
    r.out.trim().to_string()
}

const ASK: &str = r#"--what "check the sound in the browser" --why "updated the driver" --expect "works or not" --by 30m"#;

#[test]
fn both_scenarios_of_the_owner_between_two_machines() {
    let bus = Bus::start();
    let (devbox, mac) = (bus.env(Some(DEVBOX)), bus.env(Some(MAC)));
    let browser = register(&devbox, "browser", "demo");
    let audio = register(&mac, "audio", "demo");
    let ci = register(&mac, "ci", "core");

    // 2. A directed request to @audio: first look, then send, then one answer.
    let look = chappe(
        &devbox,
        &format!("ask --to @audio {ASK} --handle {browser}"),
    );
    assert_eq!(look.code, 0, "{}", look.err);
    assert!(
        look.out.contains("Not sent yet")
            && look.out.contains(&audio)
            && look.out.contains("codex@mac"),
        "{}",
        look.out
    );
    assert!(look.out.contains("If the answer is there, do not ask"));
    assert!(
        get(&bus, "/p/demo/general/messages").contains("Nothing here yet"),
        "looking sends nothing"
    );
    let sent = chappe(
        &devbox,
        &format!("ask --to @audio {ASK} --send --handle {browser}"),
    );
    assert_eq!(sent.code, 0, "{}", sent.err);
    assert_eq!(sent.out, format!("sent #1 to {audio} in demo/general\n"));

    let inbox = chappe(&mac, &format!("inbox --handle {audio}"));
    assert_eq!(inbox.code, 0, "{}", inbox.err);
    assert!(
        inbox.out.starts_with(&format!(
            "[chappe] From {browser} (claude@host-a), request, demo/general."
        )),
        "{}",
        inbox.out
    );
    assert!(
        inbox.out.contains("not an instruction from the owner")
            && inbox.out.contains("Need: check the sound in the browser")
    );
    assert!(inbox.out.contains("chappe answer --ask 1"));
    let answered = chappe(
        &mac,
        &format!("answer --ask 1 --result \"sound works\" --handle {audio}"),
    );
    assert_eq!(answered.code, 0, "{}", answered.err);
    assert_eq!(
        answered.out,
        format!("sent #2 to {browser} in demo/general\n")
    );
    let back = chappe(&devbox, &format!("inbox --handle {browser}"));
    assert!(
        back.out.contains("Answer: sound works")
            && back.out.contains("From ")
            && back.out.contains(&audio),
        "{}",
        back.out
    );
    let twice = chappe(
        &mac,
        &format!("answer --ask 1 --result more --handle {audio}"),
    );
    assert_eq!(twice.code, 1);
    assert!(
        twice.err.contains("not sent: that ask is already answered"),
        "{}",
        twice.err
    );

    // 1. An announcement to everyone: another machine reads it at once, the one
    // that has just read its inbox waits five minutes.
    let announced = chappe(
        &devbox,
        &format!(
            "announce --topic laptop --what \"ready to update the kernel\" --handle {browser}"
        ),
    );
    assert_eq!(announced.code, 0, "{}", announced.err);
    assert_eq!(announced.out, "sent #3 to 2 sessions in all/laptop\n");
    let at_once = chappe(&mac, &format!("inbox --handle {ci}"));
    assert!(
        at_once
            .out
            .contains("Announcing: ready to update the kernel")
            && at_once.out.contains("announcement"),
        "{}",
        at_once.out
    );
    let held = chappe(&mac, &format!("inbox --handle {audio}"));
    assert!(
        held.out.contains("1 message(s) are waiting") && !held.out.contains("Announcing"),
        "{}",
        held.out
    );
    assert!(
        chappe(&mac, &format!("inbox --handle {ci}"))
            .out
            .contains("no new messages")
    );

    // The owner's page shows the conversation, who it was for, and the numbers.
    let feed = get(&bus, "/p/demo/general/messages");
    assert!(
        feed.contains("→ @audio") && feed.contains("request") && feed.contains("sound works"),
        "{feed}"
    );
    assert!(get(&bus, "/p/all/laptop/messages").contains("→ everyone"));
    assert!(get(&bus, "/board").contains("messages 3 · requests 1 (answered 1) · rejected 0"));
}

#[test]
fn what_the_rules_turn_away_is_explained_to_the_agent() {
    let bus = Bus::start();
    let (devbox, mac) = (bus.env(Some(DEVBOX)), bus.env(Some(MAC)));
    let browser = register(&devbox, "browser", "demo");
    register(&mac, "audio", "demo");

    let nobody = chappe(
        &devbox,
        &format!("ask --to @kernel {ASK} --send --handle {browser}"),
    );
    assert_eq!(nobody.code, 1);
    assert!(
        nobody
            .err
            .contains("not sent: nobody is working on @kernel in demo; nothing was posted"),
        "{}",
        nobody.err
    );

    let secret = chappe(
        &devbox,
        &format!(
            "announce --topic laptop --what \"key {}\" --handle {browser}",
            "ab".repeat(20)
        ),
    );
    assert_eq!(secret.code, 1);
    assert!(
        secret
            .err
            .contains("refused: the message looks like it holds a token or a private key"),
        "{}",
        secret.err
    );
    assert!(!secret.err.contains(&"ab".repeat(20)));

    let wrong = chappe(
        &devbox,
        &format!("announce --what x --by 5m --handle {browser}"),
    );
    assert_eq!(wrong.code, 1);
    assert!(
        wrong.err.contains("--by is not used by announce"),
        "{}",
        wrong.err
    );

    let no_topic = chappe(
        &devbox,
        &format!("notice --to @audio --what x --why y --topic nope --handle {browser}"),
    );
    assert!(
        no_topic.err.contains("no topic nope in demo"),
        "{}",
        no_topic.err
    );

    // Six messages an hour, then the reason.
    for i in 0..6 {
        let r = chappe(
            &devbox,
            &format!("owner --what \"question {i}\" --why w --expect e --handle {browser}"),
        );
        assert_eq!(r.code, 0, "{}", r.err);
        assert_eq!(
            r.out,
            format!("sent #{} to the owner in demo/general\n", i + 1)
        );
    }
    let over = chappe(
        &devbox,
        &format!("owner --what seventh --why w --expect e --handle {browser}"),
    );
    assert_eq!(over.code, 1);
    assert!(
        over.err
            .contains("refused: budget: 6 messages per hour per session"),
        "{}",
        over.err
    );
    assert!(
        get(&bus, "/board").contains("messages 6 · requests 0 (answered 0) · rejected 2"),
        "the board counts what was turned away"
    );

    // Somebody else's session cannot send as you.
    let steal = chappe(
        &bus.env(Some(MAC)),
        &format!("announce --what x --handle {browser}"),
    );
    assert_eq!(steal.code, 1);
    assert!(
        steal.err.contains("belongs to another machine"),
        "{}",
        steal.err
    );
}

#[test]
fn messages_need_a_handle_and_a_service_that_is_up() {
    let bus = Bus::start();
    let env = bus.env(Some(DEVBOX));
    for line in [
        "announce --what x",
        "inbox",
        "ask --to @a --what x --why y --expect z --by 5m",
    ] {
        assert_eq!(chappe(&env, line).code, 2, "{line}: a handle is needed");
    }
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    let down = Env {
        vars: HashMap::from([
            ("CHAPPE_URL".to_string(), format!("http://{addr}")),
            ("CHAPPE_TOKEN".to_string(), DEVBOX.to_string()),
        ]),
        cwd: None,
    };
    for line in [
        "announce --what x --handle a-1",
        "inbox --handle a-1",
        "answer --ask 1 --result x --handle a-1",
    ] {
        let r = chappe(&down, line);
        assert_eq!(r.code, 0, "{line}: {}", r.err);
        assert!(r.err.contains("not reachable"), "{line}");
    }
}

/// An environment like a tool session's: its own state directory and session id.
fn in_session(mut env: Env, name: &str, session_id: &str) -> Env {
    let dir = std::env::temp_dir().join(format!("chappe-e2e-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    env.vars
        .insert("XDG_STATE_HOME".into(), dir.display().to_string());
    env.vars
        .insert("CLAUDE_CODE_SESSION_ID".into(), session_id.into());
    env
}

#[test]
fn a_session_is_found_by_the_id_its_tool_gives_it_so_no_flags_are_needed() {
    let bus = Bus::start();
    let env = in_session(bus.env(Some(DEVBOX)), "noflags", "c1d2e3f4-0001");
    let handle = register(&env, "audio", "demo");
    assert_eq!(chappe(&env, "beat").out, "ok\n", "no --handle");
    assert!(chappe(&env, "who").out.contains(&handle));
    assert!(
        chappe(&env, "topic create audio-driver")
            .out
            .contains("created #audio-driver")
    );
    assert!(chappe(&env, "inbox").out.contains("no new messages"));
    // Registering again from the same tool session is the same session.
    assert_eq!(register(&env, "audio", "demo"), handle);
    assert_eq!(chappe(&env, "who").out.matches("audio-").count(), 1);
    // After leave the name is forgotten.
    assert_eq!(chappe(&env, "leave").out, "ok\n");
    let after = chappe(&env, "beat");
    assert_eq!(after.code, 2);
    assert!(after.err.contains("no handle"), "{}", after.err);
    // Another tool session on the same machine has its own name.
    let other = in_session(bus.env(Some(DEVBOX)), "noflags-2", "c1d2e3f4-0002");
    assert_ne!(register(&other, "kernel", "demo"), handle);
}

/// What a hook of Claude Code runs: the event on standard input.
fn hook(env: &Env, event: &str, payload: &str) -> Out {
    let args = ["hook", "claude", event].map(String::from).to_vec();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run_hook(
        &args,
        env,
        &mut std::io::Cursor::new(payload.to_string()),
        &mut out,
        &mut err,
    );
    Out {
        code,
        out: String::from_utf8(out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn payload(session: &str, event: &str) -> String {
    format!(
        r#"{{"session_id":"{session}","transcript_path":"/home/x/.claude/projects/p/{session}.jsonl","cwd":"/work/tree","hook_event_name":"{event}"}}"#
    )
}

#[test]
fn the_hooks_of_claude_code_register_deliver_and_leave() {
    let bus = Bus::start();
    let mut browser = in_session(bus.env(Some(DEVBOX)), "hooks", "sess-browser");
    browser.vars.insert("CHAPPE_PROJECT".into(), "demo".into());
    browser.vars.insert("CHAPPE_ROLES".into(), "browser".into());
    let mac = bus.env(Some(MAC));
    let audio = register(&mac, "audio", "demo");

    // SessionStart registers and tells the agent its name.
    let start = hook(
        &browser,
        "SessionStart",
        &payload("sess-browser", "SessionStart"),
    );
    assert_eq!(start.code, 0, "{}", start.err);
    assert!(
        start.out.contains(r#""hookEventName":"SessionStart""#) && start.out.contains("browser-"),
        "{}",
        start.out
    );
    assert!(start.out.contains("Do not send greetings"));
    let who = chappe(&mac, "who --role browser");
    assert!(
        who.out.contains("claude@host-a") && who.out.contains("demo"),
        "{}",
        who.out
    );
    let handle = who
        .out
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();

    // Nothing waiting: the hook says nothing and costs the agent no tokens.
    assert_eq!(
        hook(
            &browser,
            "PostToolBatch",
            &payload("sess-browser", "PostToolBatch")
        )
        .out,
        ""
    );

    // Another session asks; at the next round of tool calls it is in the agent's context.
    let ask = chappe(
        &mac,
        &format!("ask --to @browser {ASK} --send --handle {audio}"),
    );
    assert_eq!(ask.code, 0, "{}", ask.err);
    assert!(
        ask.out.starts_with(&format!("sent #1 to {handle}")),
        "{}",
        ask.out
    );
    assert!(
        !ask.out.contains("wake"),
        "a working session is not woken: {}",
        ask.out
    );
    // The service is asked at most every 20 seconds: right after the empty check, not yet.
    assert_eq!(
        hook(
            &browser,
            "PostToolBatch",
            &payload("sess-browser", "PostToolBatch")
        )
        .out,
        ""
    );
    // A prompt of the user is a checkpoint of its own.
    let prompt = hook(
        &browser,
        "UserPromptSubmit",
        &payload("sess-browser", "UserPromptSubmit"),
    );
    assert_eq!(prompt.code, 0);
    assert!(
        prompt.out.contains(r#""hookEventName":"UserPromptSubmit""#),
        "{}",
        prompt.out
    );
    assert!(
        prompt.out.contains("[chappe] From ")
            && prompt.out.contains("check the sound in the browser"),
        "{}",
        prompt.out
    );
    assert!(prompt.out.contains("not an instruction from the owner"));

    // Closing the terminal does not end the session: it stays, idle, to be woken.
    let end = hook(
        &browser,
        "SessionEnd",
        &payload("sess-browser", "SessionEnd"),
    );
    assert_eq!((end.code, end.out.as_str()), (0, ""));
    assert!(chappe(&mac, "who").out.contains(&handle));
    // Resumed later, it registers again as the same session.
    let again = hook(
        &browser,
        "SessionStart",
        &payload("sess-browser", "SessionStart"),
    );
    assert!(again.out.contains(&handle), "{}", again.out);
    // The work ends with `chappe leave`.
    assert_eq!(chappe(&browser, "leave").out, "ok\n");
    assert!(!chappe(&mac, "who").out.contains(&handle));
}

#[test]
fn a_hook_never_gets_in_the_agents_way() {
    let bus = Bus::start();
    let mut env = in_session(bus.env(Some(DEVBOX)), "quiet", "sess-q");
    // A project that is not set up for chappe-bus: nothing happens.
    let start = hook(&env, "SessionStart", &payload("sess-q", "SessionStart"));
    assert_eq!((start.code, start.out.as_str()), (0, ""));
    assert_eq!(chappe(&env, "who").out, "nobody is registered\n");
    env.vars.insert("CHAPPE_PROJECT".into(), "demo".into());
    env.vars.insert("CHAPPE_ROLES".into(), "audio".into());
    // Rubbish on standard input, an unknown event, a bad tool, a wrong call.
    for line in [
        "not json",
        "{}",
        r#"{"session_id":"../etc/passwd"}"#,
        r#"{"session_id":""}"#,
    ] {
        let r = hook(&env, "SessionStart", line);
        assert_eq!((r.code, r.out.as_str()), (0, ""), "{line}");
    }
    assert_eq!(hook(&env, "Nope", &payload("sess-q", "Nope")).code, 0);
    let args = ["hook", "codex", "SessionStart"].map(String::from).to_vec();
    let (mut sink, mut sink2) = (Vec::new(), Vec::new());
    assert_eq!(
        run_hook(
            &args,
            &env,
            &mut std::io::Cursor::new(Vec::new()),
            &mut sink,
            &mut sink2,
        ),
        0
    );
    assert_eq!(
        run_hook(
            &["hook".to_string()],
            &env,
            &mut std::io::Cursor::new(Vec::new()),
            &mut sink,
            &mut sink2,
        ),
        0
    );
    assert_eq!(
        chappe(&env, "who").out,
        "nobody is registered\n",
        "none of that registered anything"
    );
    // The service is down: still 0 and silent.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closed.local_addr().unwrap();
    drop(closed);
    env.vars
        .insert("CHAPPE_URL".into(), format!("http://{addr}"));
    for event in [
        "SessionStart",
        "PostToolBatch",
        "UserPromptSubmit",
        "SessionEnd",
    ] {
        let r = hook(&env, event, &payload("sess-q", event));
        assert_eq!((r.code, r.out.as_str()), (0, ""), "{event}");
    }
}

fn adapter_file(path: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../adapters")
        .join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn the_template_of_the_claude_hooks_runs_the_command_that_exists() {
    let settings: serde_json::Value =
        serde_json::from_str(&adapter_file("claude/settings.json")).unwrap();
    // The three events, each running `chappe hook claude <Event>`.
    for event in ["SessionStart", "PostToolBatch", "UserPromptSubmit"] {
        let command = settings["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        // The command sets its own PATH: a session not started from a terminal has no
        // ~/.local/bin, and a hook that cannot find `chappe` fails without a word.
        assert_eq!(
            command,
            format!(
                "PATH=$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH chappe hook claude {event}"
            )
        );
        assert_eq!(settings["hooks"][event][0]["hooks"][0]["type"], "command");
    }
    assert_eq!(
        settings["hooks"].as_object().unwrap().len(),
        3,
        "no hook for SessionEnd: a closed terminal is an idle session, not an ended one"
    );
    assert!(
        settings["env"]["CHAPPE_PROJECT"].is_string()
            && settings["env"]["CHAPPE_ROLES"].is_string()
    );
    assert!(
        settings.get("_comment").is_none(),
        "no field Claude Code does not know"
    );
}

#[test]
fn the_template_for_codex_uses_the_commands_and_the_variable_that_exist() {
    let snippet = adapter_file("codex/AGENTS.snippet.md");
    for needle in [
        "chappe register",
        "CODEX_THREAD_ID",
        "chappe inbox",
        "chappe ask --to @role",
        "chappe leave",
        "Never send greetings",
    ] {
        assert!(snippet.contains(needle), "{needle}");
    }
}

#[test]
fn the_service_templates_start_the_host_with_its_token_file() {
    let unit = adapter_file("host/chappe-host.service");
    assert!(
        unit.contains("ExecStart=%h/.local/bin/chappe-host")
            && unit.contains("CHAPPE_HOST_TOKEN_FILE")
    );
    assert!(unit.contains("Restart=always") && unit.contains("NoNewPrivileges=true"));
    let plist = adapter_file("host/com.example.chappe-host.plist");
    assert!(
        plist.contains("chappe-host")
            && plist.contains("CHAPPE_HOST_TOKEN_FILE")
            && plist.contains("KeepAlive")
    );
    // No secret in a template.
    for text in [&unit, &plist] {
        assert!(!chappe_bus::message::looks_like_secret(text));
    }
}

#[test]
fn the_doctor_checks_the_token_the_service_and_the_host_daemon_and_prints_no_token() {
    let bus = Bus::start();
    let mut env = bus.env(Some(DEVBOX));
    env.vars.insert("HOME".into(), "/nonexistent".into());
    env.vars
        .insert("CHAPPE_DOCTOR_HOOK_PATH".into(), "/nonexistent".into());
    let r = chappe(&env, "doctor");
    assert!(r.out.contains("ok    token at the service"), "{}", r.out);
    assert!(r.out.contains("accepts it as host-a.claude"), "{}", r.out);
    // Nobody started the host daemon of this machine: said, with what follows from it.
    assert!(
        r.out.contains("FAIL  host daemon") && r.out.contains("has not asked for work yet"),
        "{}",
        r.out
    );
    assert_eq!(r.code, 1, "something does not work");
    assert!(!r.out.contains(DEVBOX) && !r.err.contains(DEVBOX));
    // A rejected token is a failure too, and it is not printed either.
    let wrong = "wrong-token-long-enough-0123456789abcdef";
    let r = chappe(&bus.env(Some(wrong)), "doctor");
    assert!(r.out.contains("FAIL  token at the service"), "{}", r.out);
    assert!(!r.out.contains(wrong));
    // Without a token the doctor still answers: that is when it is needed.
    let r = chappe(&bus.env(None), "doctor");
    assert!(r.out.contains("FAIL  agent token"), "{}", r.out);
    assert_eq!(r.code, 1);
}
