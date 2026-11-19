use super::*;
use crate::message::OWNER;
use crate::messaging::{Outgoing, SendError};
use crate::registry::NewSession;

const T0: i64 = 1_800_000_000;
/// Long enough after the registration for a session to be idle, short of gone.
const IDLE: i64 = T0 + 700;

fn agent(machine: &str, tool: &str) -> Agent {
    Agent {
        machine: machine.into(),
        tool: tool.into(),
    }
}

fn gm() -> Agent {
    agent("host-a", "claude")
}

fn mac() -> Agent {
    agent("mac", "codex")
}

fn mac_host() -> Agent {
    agent("mac", "host")
}

struct World {
    s: Store,
    browser: String,
    audio: String,
}

fn world() -> World {
    let s = Store::open_in_memory().unwrap();
    s.ensure_projects(&["all".into(), "demo".into()]).unwrap();
    let new = |role: &str, id: &str| NewSession {
        roles: vec![role.into()],
        project: "demo".into(),
        session_id: Some(id.into()),
        worktree: Some(format!("/work/{role}")),
        ..NewSession::default()
    };
    let browser = s
        .register(&gm(), new("browser", "sid-browser"), T0)
        .unwrap()
        .handle;
    let audio = s
        .register(&mac(), new("audio", "sid-audio"), T0)
        .unwrap()
        .handle;
    World { s, browser, audio }
}

fn out(kind: &str, handle: &str) -> Outgoing {
    Outgoing {
        handle: handle.into(),
        kind: kind.into(),
        ..Outgoing::default()
    }
}

fn ask(handle: &str, to: &str, what: &str) -> Outgoing {
    Outgoing {
        to: Some(to.into()),
        what: Some(what.into()),
        why: Some("w".into()),
        expect: Some("e".into()),
        by: Some("2h".into()),
        ..out("ask", handle)
    }
}

/// Sends with the hosts of both machines up (they asked for work a moment ago).
fn send(w: &World, who: &Agent, o: Outgoing, now: i64) -> crate::messaging::Sent {
    w.s.host_is_up("mac", now);
    w.s.host_is_up("host-a", now);
    w.s.send(who, o, now)
        .unwrap_or_else(|e: SendError| panic!("{e}"))
}

fn wakes(s: &Store) -> Vec<(String, String)> {
    let conn = s.conn();
    let mut stmt = conn
        .prepare("SELECT handle, state FROM wakes ORDER BY id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn notes(s: &Store, handle: &str) -> Vec<String> {
    let conn = s.conn();
    let mut stmt = conn
        .prepare("SELECT text FROM notes WHERE handle = ?1 ORDER BY id")
        .unwrap();
    stmt.query_map([handle], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn queued(s: &Store, handle: &str) -> i64 {
    s.conn()
        .query_row(
            "SELECT COUNT(*) FROM deliveries WHERE handle = ?1 AND state = 'queued'",
            [handle],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn an_ask_to_an_idle_session_asks_for_a_wake_and_one_to_a_working_session_does_not() {
    let w = world();
    // The browser is working (a signal just now); the audio session is idle.
    w.s.beat(&gm(), &w.browser, IDLE - 5).unwrap();
    let to_idle = send(&w, &gm(), ask(&w.browser, "@audio", "one"), IDLE);
    assert_eq!(to_idle.wake, Some(WakeOutcome::Requested));
    assert_eq!(wakes(&w.s), [(w.audio.clone(), "pending".to_string())]);
    w.s.beat(&mac(), &w.audio, IDLE + 1).unwrap();
    let to_active = send(&w, &mac(), ask(&w.audio, "@browser", "two"), IDLE + 2);
    assert_eq!(to_active.wake, Some(WakeOutcome::NotNeeded));
    assert_eq!(
        wakes(&w.s).len(),
        1,
        "a working session reads at its checkpoint"
    );
}

#[test]
fn a_session_that_was_working_when_the_ask_came_and_went_quiet_is_woken_later() {
    let w = world();
    // The audio session signals a minute before the ask: working, so no wake is asked for.
    w.s.beat(&mac(), &w.audio, IDLE - 60).unwrap();
    let sent = send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    assert_eq!(sent.wake, Some(WakeOutcome::NotNeeded));
    assert!(wakes(&w.s).is_empty());
    // The host asks again while the session is still working: nothing.
    assert!(w.s.claim_wakes(&mac_host(), IDLE + 120).unwrap().is_empty());
    assert!(wakes(&w.s).is_empty());
    // Ten minutes after its last signal it is idle, and the ask is still waiting: woken.
    let later = IDLE - 60 + 10 * 60 + 5;
    let jobs = w.s.claim_wakes(&mac_host(), later).unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].handle, w.audio);
    assert!(jobs[0].text.contains("Need: x"));
    assert_eq!(wakes(&w.s), [(w.audio.clone(), "claimed".to_string())]);
    // Asking again does not make a second wake.
    assert!(w.s.claim_wakes(&mac_host(), later + 10).unwrap().is_empty());
    assert_eq!(wakes(&w.s).len(), 1);
}

#[test]
fn an_ask_that_ran_out_of_time_or_whose_receiver_left_is_not_woken_for() {
    let w = world();
    w.s.beat(&mac(), &w.audio, IDLE - 60).unwrap();
    let sent = send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    assert_eq!(sent.wake, Some(WakeOutcome::NotNeeded));
    // The ask said two hours; after that it is not worth waking anyone.
    assert!(
        w.s.claim_wakes(&mac_host(), IDLE + 2 * 3600 + 5)
            .unwrap()
            .is_empty()
    );
    assert!(wakes(&w.s).is_empty());
    let w = world();
    w.s.beat(&mac(), &w.audio, IDLE - 60).unwrap();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    w.s.leave(&mac(), &w.audio, IDLE + 5).unwrap();
    assert!(
        w.s.claim_wakes(&mac_host(), IDLE + 15 * 60)
            .unwrap()
            .is_empty()
    );
    assert!(wakes(&w.s).is_empty());
}

#[test]
fn without_a_host_daemon_on_the_machine_nothing_is_woken_and_the_sender_is_told() {
    let w = world();
    // No host has ever asked for work.
    let sent =
        w.s.send(&gm(), ask(&w.browser, "@audio", "x"), IDLE)
            .unwrap();
    assert_eq!(sent.wake, Some(WakeOutcome::NoHost));
    assert!(wakes(&w.s).is_empty(), "no wake that nobody will carry out");
    assert_eq!(
        queued(&w.s, &w.audio),
        1,
        "the ask waits for the session's checkpoint"
    );
    // A host that asked three minutes ago is taken to be down; one that asked a minute ago is up.
    w.s.host_is_up("mac", IDLE + 10);
    let late =
        w.s.send(&gm(), ask(&w.browser, "@audio", "y"), IDLE + 10 + 180)
            .unwrap();
    assert_eq!(late.wake, Some(WakeOutcome::NoHost));
    w.s.host_is_up("mac", IDLE + 400);
    let soon =
        w.s.send(&gm(), ask(&w.browser, "@audio", "z"), IDLE + 400 + 60)
            .unwrap();
    assert_eq!(soon.wake, Some(WakeOutcome::Requested));
    // A host of another machine says nothing about this one.
    w.s.host_is_up("host-a", IDLE + 500);
    assert_eq!(wakes(&w.s).len(), 1);
}

#[test]
fn a_poll_of_the_host_is_what_marks_it_up() {
    let w = world();
    assert!(w.s.claim_wakes(&mac_host(), IDLE - 1).unwrap().is_empty());
    assert_eq!(
        w.s.send(&gm(), ask(&w.browser, "@audio", "x"), IDLE)
            .unwrap()
            .wake,
        Some(WakeOutcome::Requested)
    );
}

#[test]
fn only_an_ask_wakes_not_an_announcement_a_notice_or_a_message_to_the_owner() {
    let w = world();
    w.s.create_topic("all", "laptop", None, OWNER, T0).unwrap();
    let announce = Outgoing {
        topic: Some("laptop".into()),
        what: Some("x".into()),
        ..out("announce", &w.browser)
    };
    assert_eq!(send(&w, &gm(), announce, IDLE).wake, None);
    let notice = Outgoing {
        to: Some("@audio".into()),
        what: Some("x".into()),
        why: Some("y".into()),
        ..out("notice", &w.browser)
    };
    assert_eq!(send(&w, &gm(), notice, IDLE + 1).wake, None);
    let owner = Outgoing {
        what: Some("x".into()),
        why: Some("y".into()),
        expect: Some("z".into()),
        ..out("owner", &w.browser)
    };
    assert_eq!(send(&w, &gm(), owner, IDLE + 2).wake, None);
    assert!(wakes(&w.s).is_empty());
}

#[test]
fn a_second_ask_rides_on_the_wake_that_is_already_waiting() {
    let w = world();
    assert_eq!(
        send(&w, &gm(), ask(&w.browser, "@audio", "one"), IDLE).wake,
        Some(WakeOutcome::Requested)
    );
    assert_eq!(
        send(&w, &gm(), ask(&w.browser, "@audio", "two"), IDLE + 1).wake,
        Some(WakeOutcome::Pending)
    );
    assert_eq!(wakes(&w.s).len(), 1);
    let job = &w.s.claim_wakes(&agent("mac", "host"), IDLE + 2).unwrap()[0];
    assert_eq!(job.message_ids.len(), 2, "both asks in one wake");
}

#[test]
fn a_session_is_woken_at_most_twice_an_hour_and_the_sender_is_told() {
    let w = world();
    let wake_once = |ask_text: &str, now: i64| {
        let sent = send(&w, &gm(), ask(&w.browser, "@audio", ask_text), now);
        let job = w.s.claim_wakes(&mac_host(), now + 1).unwrap();
        if let Some(job) = job.first() {
            w.s.finish_wake(
                &mac_host(),
                job.id,
                Finish::Delivered {
                    message_ids: job.message_ids.clone(),
                },
                now + 2,
            )
            .unwrap();
        }
        sent.wake
    };
    assert_eq!(wake_once("first", IDLE), Some(WakeOutcome::Requested));
    assert_eq!(wake_once("second", IDLE + 60), Some(WakeOutcome::Requested));
    assert_eq!(
        wake_once("third", IDLE + 120),
        Some(WakeOutcome::Limit),
        "the third wake in an hour is queued"
    );
    assert_eq!(
        queued(&w.s, &w.audio),
        1,
        "the ask waits for the session's checkpoint"
    );
    assert_eq!(wakes(&w.s).len(), 2, "no third wake was made");
    // An hour after the first wake there is room again.
    assert_eq!(
        wake_once("fourth", IDLE + 3600 + 10),
        Some(WakeOutcome::Requested)
    );
}

#[test]
fn a_dry_run_says_what_would_happen_and_leaves_no_wake() {
    let w = world();
    let sent = send(
        &w,
        &gm(),
        Outgoing {
            dry_run: true,
            ..ask(&w.browser, "@audio", "x")
        },
        IDLE,
    );
    assert_eq!(sent.wake, Some(WakeOutcome::Requested));
    assert!(wakes(&w.s).is_empty());
}

#[test]
fn an_answer_wakes_the_asker_only_if_the_ask_said_wait() {
    for (wait, expected) in [(false, None), (true, Some("pending".to_string()))] {
        let w = world();
        let asked = send(
            &w,
            &gm(),
            Outgoing {
                wait,
                ..ask(&w.browser, "@audio", "x")
            },
            T0 + 1,
        );
        w.s.beat(&mac(), &w.audio, IDLE).unwrap();
        // The asker has gone quiet since; the audio session answers.
        let reply = Outgoing {
            ask: asked.id,
            result: Some("done".into()),
            ..out("answer", &w.audio)
        };
        let sent = send(&w, &mac(), reply, IDLE + 1);
        assert_eq!(sent.wake.is_some(), wait, "wait={wait}");
        let browser_wakes: Vec<_> = wakes(&w.s)
            .into_iter()
            .filter(|(h, _)| *h == w.browser)
            .map(|(_, s)| s)
            .collect();
        assert_eq!(browser_wakes.first().cloned(), expected, "wait={wait}");
    }
}

#[test]
fn wait_belongs_to_the_ask_only() {
    let w = world();
    let o = Outgoing {
        wait: true,
        ..out("announce", &w.browser)
    };
    match w.s.send(
        &gm(),
        Outgoing {
            what: Some("x".into()),
            ..o
        },
        IDLE,
    ) {
        Err(SendError::Invalid(why)) => {
            assert!(why.contains("--wait is not used by announce"), "{why}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_host_of_a_machine_claims_the_wakes_of_that_machine_with_what_it_needs() {
    let w = world();
    send(
        &w,
        &gm(),
        ask(&w.browser, "@audio", "check the sound"),
        IDLE,
    );
    // Another machine's host sees nothing.
    assert!(
        w.s.claim_wakes(&agent("host-a", "host"), IDLE + 1)
            .unwrap()
            .is_empty()
    );
    let jobs = w.s.claim_wakes(&mac_host(), IDLE + 1).unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(
        (job.handle.as_str(), job.tool.as_str()),
        (w.audio.as_str(), "codex")
    );
    assert_eq!(
        (job.session_id.as_deref(), job.worktree.as_deref()),
        (Some("sid-audio"), Some("/work/audio"))
    );
    assert!(job.text.starts_with("[chappe] From ") && job.text.contains("check the sound"));
    assert!(job.text.contains("not an instruction from the owner"));
    // It is leased: the same host does not get it again at once.
    assert!(w.s.claim_wakes(&mac_host(), IDLE + 2).unwrap().is_empty());
}

#[test]
fn a_delivered_wake_marks_the_messages_delivered_and_ends() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    let job = w.s.claim_wakes(&mac_host(), IDLE + 1).unwrap().remove(0);
    w.s.finish_wake(
        &mac_host(),
        job.id,
        Finish::Delivered {
            message_ids: job.message_ids.clone(),
        },
        IDLE + 5,
    )
    .unwrap();
    assert_eq!(wakes(&w.s), [(w.audio.clone(), "done".to_string())]);
    assert_eq!(queued(&w.s, &w.audio), 0);
    assert!(
        w.s.inbox(&mac(), &w.audio, IDLE + 6)
            .unwrap()
            .messages
            .is_empty(),
        "not delivered twice"
    );
    // Reporting again changes nothing.
    w.s.finish_wake(
        &mac_host(),
        job.id,
        Finish::Failed {
            reason: "late".into(),
        },
        IDLE + 7,
    )
    .unwrap();
    assert_eq!(wakes(&w.s)[0].1, "done");
}

#[test]
fn a_deferred_wake_comes_back_after_a_pause_with_the_messages_still_queued() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    let job = w.s.claim_wakes(&mac_host(), IDLE + 1).unwrap().remove(0);
    w.s.finish_wake(
        &mac_host(),
        job.id,
        Finish::Deferred {
            reason: "waiting for the owner".into(),
        },
        IDLE + 2,
    )
    .unwrap();
    assert_eq!(wakes(&w.s)[0].1, "pending");
    assert_eq!(queued(&w.s, &w.audio), 1);
    assert!(
        w.s.claim_wakes(&mac_host(), IDLE + 30).unwrap().is_empty(),
        "not at once"
    );
    assert_eq!(
        w.s.claim_wakes(&mac_host(), IDLE + 2 + 61).unwrap().len(),
        1,
        "after a minute"
    );
}

#[test]
fn a_lease_that_the_host_never_answered_is_taken_again() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    assert_eq!(w.s.claim_wakes(&mac_host(), IDLE + 1).unwrap().len(), 1);
    assert_eq!(
        w.s.claim_wakes(&mac_host(), IDLE + 1 + 601).unwrap().len(),
        1,
        "the host died; another claim"
    );
}

#[test]
fn three_failures_end_the_wake_and_the_sender_is_told() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    let mut now = IDLE + 1;
    for _ in 0..3 {
        let job = w.s.claim_wakes(&mac_host(), now).unwrap().remove(0);
        w.s.finish_wake(
            &mac_host(),
            job.id,
            Finish::Failed {
                reason: "claude exited with 1".into(),
            },
            now + 1,
        )
        .unwrap();
        now += 400;
    }
    assert_eq!(wakes(&w.s)[0].1, "expired");
    let told = notes(&w.s, &w.browser);
    assert_eq!(told.len(), 1);
    assert!(
        told[0].contains("Not delivered")
            && told[0].contains(&w.audio)
            && told[0].contains("claude exited with 1")
    );
    assert_eq!(
        queued(&w.s, &w.audio),
        1,
        "the ask stays in the queue for the checkpoint"
    );
}

#[test]
fn a_tool_without_a_way_to_wake_ends_the_wake_at_once_and_says_so() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    let job = w.s.claim_wakes(&mac_host(), IDLE + 1).unwrap().remove(0);
    w.s.finish_wake(
        &mac_host(),
        job.id,
        Finish::Unsupported {
            reason: "no adapter for codex".into(),
        },
        IDLE + 2,
    )
    .unwrap();
    assert_eq!(wakes(&w.s)[0].1, "expired");
    assert!(notes(&w.s, &w.browser)[0].contains("has no way to wake"));
}

#[test]
fn a_wake_nobody_carried_out_for_two_hours_expires_and_the_sender_hears_it() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    // The machine is asleep: no host claims anything. The next thing that
    // happens on the service finds the wake old.
    let later = IDLE + 2 * 3600 + 1;
    w.s.beat(&gm(), &w.browser, later - 100).unwrap();
    let inbox = w.s.inbox(&gm(), &w.browser, later).unwrap();
    assert_eq!(wakes(&w.s)[0].1, "expired");
    assert_eq!(inbox.notes.len(), 1);
    assert!(
        inbox.notes[0].contains("was not woken within 2 hours")
            && inbox.notes[0].contains(&w.audio)
    );
    assert!(
        w.s.inbox(&gm(), &w.browser, later + 1)
            .unwrap()
            .notes
            .is_empty(),
        "told once"
    );
}

#[test]
fn a_session_that_read_its_messages_meanwhile_needs_no_wake() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    // The session came back by itself and read its inbox.
    assert_eq!(
        w.s.inbox(&mac(), &w.audio, IDLE + 10)
            .unwrap()
            .messages
            .len(),
        1
    );
    assert!(w.s.claim_wakes(&mac_host(), IDLE + 11).unwrap().is_empty());
    assert_eq!(wakes(&w.s)[0].1, "done");
}

#[test]
fn an_ask_the_sender_stopped_waiting_for_is_not_delivered_by_a_wake() {
    let w = world();
    send(
        &w,
        &gm(),
        Outgoing {
            by: Some("1m".into()),
            ..ask(&w.browser, "@audio", "x")
        },
        IDLE,
    );
    assert!(w.s.claim_wakes(&mac_host(), IDLE + 61).unwrap().is_empty());
    assert_eq!(wakes(&w.s)[0].1, "done");
}

#[test]
fn a_wake_for_an_ended_session_is_dropped_and_the_sender_told() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    w.s.leave(&mac(), &w.audio, IDLE + 1).unwrap();
    assert!(w.s.claim_wakes(&mac_host(), IDLE + 2).unwrap().is_empty());
    assert_eq!(wakes(&w.s)[0].1, "expired");
    assert!(notes(&w.s, &w.browser)[0].contains("has already ended"));
}

#[test]
fn only_the_host_of_the_sessions_machine_reports_a_wake() {
    let w = world();
    send(&w, &gm(), ask(&w.browser, "@audio", "x"), IDLE);
    let job = w.s.claim_wakes(&mac_host(), IDLE + 1).unwrap().remove(0);
    let other = agent("host-a", "host");
    assert!(matches!(
        w.s.finish_wake(
            &other,
            job.id,
            Finish::Delivered {
                message_ids: vec![]
            },
            IDLE + 2
        ),
        Err(SessionError::Forbidden)
    ));
    assert!(matches!(
        w.s.finish_wake(
            &mac_host(),
            999,
            Finish::Delivered {
                message_ids: vec![]
            },
            IDLE + 2
        ),
        Err(SessionError::NotFound)
    ));
    assert_eq!(wakes(&w.s)[0].1, "claimed");
}

#[test]
fn reports_have_a_json_shape() {
    let d: Finish = serde_json::from_str(r#"{"outcome":"delivered","message_ids":[1,2]}"#).unwrap();
    assert_eq!(
        d,
        Finish::Delivered {
            message_ids: vec![1, 2]
        }
    );
    let f: Finish = serde_json::from_str(r#"{"outcome":"deferred","reason":"busy"}"#).unwrap();
    assert_eq!(
        f,
        Finish::Deferred {
            reason: "busy".into()
        }
    );
    assert!(serde_json::from_str::<Finish>(r#"{"outcome":"explode"}"#).is_err());
}

fn running(tool: &str, ids: &[&str]) -> std::collections::HashMap<String, Vec<String>> {
    std::collections::HashMap::from([(
        tool.to_string(),
        ids.iter().map(|s| s.to_string()).collect(),
    )])
}

fn audio_listed(w: &World, now: i64) -> bool {
    w.s.sessions(crate::registry::Filter::default(), now)
        .unwrap()
        .iter()
        .any(|s| s.handle == w.audio)
}

#[test]
fn a_session_silent_for_two_hours_whose_process_is_gone_is_gone_and_is_not_asked() {
    use crate::registry::PROCESS_GONE_AFTER_SECS as TWO_HOURS;
    let w = world();
    // The host of the Mac finds no Codex process for the audio session.
    w.s.note_processes(&mac_host(), &running("codex", &[]), IDLE)
        .unwrap();
    // Under two hours of silence it is only idle: still asked, and woken.
    let soon = T0 + TWO_HOURS - 1;
    assert!(audio_listed(&w, soon));
    w.s.beat(&gm(), &w.browser, soon).unwrap();
    let sent = send(&w, &gm(), ask(&w.browser, "@audio", "early"), soon);
    assert_eq!(sent.wake, Some(WakeOutcome::Requested));
    // At two hours it is gone: off the board and never the receiver of a question to its role.
    let late = T0 + TWO_HOURS;
    let session = crate::registry::load(&w.s.conn(), &w.audio)
        .unwrap()
        .unwrap();
    assert_eq!(session.state(late), State::Gone);
    assert!(!audio_listed(&w, late));
    w.s.beat(&gm(), &w.browser, late).unwrap();
    let refused = w.s.send(&gm(), ask(&w.browser, "@audio", "late"), late);
    assert!(
        matches!(refused, Err(SendError::Unavailable(_))),
        "{refused:?}"
    );
}

#[test]
fn a_session_with_a_process_or_one_the_host_did_not_check_is_never_gone_for_silence() {
    let w = world();
    let late = T0 + 3 * 3600;
    // The process is alive (a terminal left open): silence alone is not the end.
    w.s.note_processes(&mac_host(), &running("codex", &["sid-audio"]), IDLE)
        .unwrap();
    assert!(audio_listed(&w, late));
    // The host listed another tool only: Codex sessions are not touched.
    w.s.note_processes(&mac_host(), &running("claude", &[]), IDLE)
        .unwrap();
    assert!(audio_listed(&w, late));
    // Another machine's host cannot speak for this machine's sessions.
    w.s.note_processes(&agent("host-a", "host"), &running("codex", &[]), IDLE)
        .unwrap();
    assert!(audio_listed(&w, late));
}

#[test]
fn a_signal_or_a_process_that_appears_brings_a_gone_session_back() {
    let w = world();
    let late = T0 + 3 * 3600;
    w.s.note_processes(&mac_host(), &running("codex", &[]), IDLE)
        .unwrap();
    assert!(!audio_listed(&w, late));
    // The host finds the process again (the session was resumed).
    w.s.note_processes(&mac_host(), &running("codex", &["sid-audio"]), late)
        .unwrap();
    assert!(audio_listed(&w, late));
    // Or the session itself signals, which clears the mark at once.
    w.s.note_processes(&mac_host(), &running("codex", &[]), late + 1)
        .unwrap();
    assert!(!audio_listed(&w, late + 1));
    assert!(w.s.beat(&mac(), &w.audio, late + 2).is_ok());
    assert!(audio_listed(&w, late + 3));
    let session = crate::registry::load(&w.s.conn(), &w.audio)
        .unwrap()
        .unwrap();
    assert_eq!(session.process_gone_at, None);
}

#[test]
fn the_host_status_says_when_the_daemon_of_a_machine_last_asked() {
    let w = world();
    assert_eq!(w.s.host_last_poll("mac").unwrap(), None);
    w.s.claim_wakes(&mac_host(), IDLE).unwrap();
    assert_eq!(w.s.host_last_poll("mac").unwrap(), Some(IDLE));
    assert_eq!(w.s.host_last_poll("host-a").unwrap(), None);
}
