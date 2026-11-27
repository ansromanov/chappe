use super::*;

const T0: i64 = 1_800_000_000;

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn agent(machine: &str, tool: &str) -> Agent {
    Agent {
        machine: machine.into(),
        tool: tool.into(),
    }
}

fn new(role: &str, project: &str) -> NewSession {
    NewSession {
        roles: vec![role.into()],
        project: project.into(),
        ..NewSession::default()
    }
}

fn devbox() -> Agent {
    agent("host-a", "claude")
}

#[test]
fn state_follows_the_last_signal() {
    let s = |age: i64| state(T0 - age, None, T0);
    assert_eq!(s(0), State::Active);
    assert_eq!(s(ACTIVE_WINDOW_SECS - 1), State::Active);
    assert_eq!(s(ACTIVE_WINDOW_SECS), State::Idle);
    assert_eq!(s(GONE_AFTER_SECS - 1), State::Idle);
    assert_eq!(s(GONE_AFTER_SECS), State::Gone);
}

#[test]
fn leaving_ends_the_session_at_once() {
    assert_eq!(state(T0, Some(T0), T0), State::Gone);
}

#[test]
fn register_takes_machine_and_tool_from_the_token() {
    let s = store();
    let session = s.register(&devbox(), new("browser", "demo"), T0).unwrap();
    assert_eq!(
        (session.machine.as_str(), session.tool.as_str()),
        ("host-a", "claude")
    );
    assert_eq!(session.agent(), "claude@host-a");
    assert_eq!(session.state(T0), State::Active);
    assert!(session.handle.starts_with("browser-"));
    assert_eq!(session.handle.len(), "browser-".len() + 4);
}

#[test]
fn handles_are_unique() {
    let s = store();
    let handles: std::collections::HashSet<_> = (0..200)
        .map(|_| {
            s.register(&devbox(), new("kernel", "demo"), T0)
                .unwrap()
                .handle
        })
        .collect();
    assert_eq!(handles.len(), 200);
}

#[test]
fn register_validates_the_fields() {
    let s = store();
    let err = |n: NewSession| s.register(&devbox(), n, T0).unwrap_err().to_string();
    assert!(
        err(NewSession {
            roles: vec![],
            ..new("x", "p")
        })
        .contains("at least one role")
    );
    assert!(err(new("Bad Role", "p")).contains("role"));
    assert!(err(new("a/../b", "p")).contains("role"));
    assert!(err(new("browser", "Studio OS")).contains("project"));
    assert!(
        err(NewSession {
            roles: (0..9).map(|i| format!("r{i}")).collect(),
            ..new("x", "p")
        })
        .contains("at most")
    );
    assert!(
        err(NewSession {
            issue: Some("a\nb".into()),
            ..new("browser", "p")
        })
        .contains("control characters")
    );
    assert!(
        err(NewSession {
            branch: Some("x".repeat(101)),
            ..new("browser", "p")
        })
        .contains("longer than")
    );
    assert!(
        s.sessions(Filter::default(), T0).unwrap().is_empty(),
        "nothing was stored"
    );
}

#[test]
fn roles_are_deduplicated_and_blank_fields_are_absent() {
    let s = store();
    let n = NewSession {
        roles: vec!["audio".into(), " audio ".into(), "desktop".into()],
        issue: Some("  ".into()),
        branch: Some(" main ".into()),
        ..new("x", "demo")
    };
    let session = s.register(&devbox(), n, T0).unwrap();
    assert_eq!(session.roles, ["audio", "desktop"]);
    assert_eq!(
        (session.issue, session.branch.as_deref()),
        (None, Some("main"))
    );
}

#[test]
fn secrets_are_not_stored() {
    let s = store();
    let token = "0123456789abcdef".repeat(4);
    for field in 0..3 {
        let mut n = new("browser", "demo");
        match field {
            0 => n.issue = Some(format!("#1 {token}")),
            1 => n.worktree = Some(format!("/home/x/{token}")),
            _ => n.branch = Some("-----BEGIN OPENSSH PRIVATE KEY-----".into()),
        }
        let err = s.register(&devbox(), n, T0).unwrap_err().to_string();
        assert!(err.contains("looks like a secret"), "{err}");
        assert!(!err.contains(&token), "the error must not echo the value");
    }
    let uuid = NewSession {
        session_id: Some("3f2b8c1e-9d4a-4e7b-a1c2-5d6e7f8a9b0c".into()),
        ..new("browser", "p")
    };
    assert!(
        s.register(&devbox(), uuid, T0).is_ok(),
        "a session id is not a secret"
    );
}

#[test]
fn registering_again_with_the_same_session_id_returns_the_same_record() {
    let s = store();
    let n = |branch: &str| NewSession {
        session_id: Some("sid-1".into()),
        branch: Some(branch.into()),
        ..new("audio", "demo")
    };
    let first = s.register(&devbox(), n("one"), T0).unwrap();
    let second = s.register(&devbox(), n("two"), T0 + 60).unwrap();
    assert_eq!(first.handle, second.handle);
    assert_eq!(
        (second.branch.as_deref(), second.last_seen),
        (Some("two"), T0 + 60)
    );
    assert_eq!(s.sessions(Filter::default(), T0 + 60).unwrap().len(), 1);
    // The same id on another machine is another session.
    let other = s.register(&agent("mac", "claude"), n("one"), T0).unwrap();
    assert_ne!(other.handle, first.handle);
}

#[test]
fn a_session_id_of_an_ended_session_starts_a_new_one() {
    let s = store();
    let n = || NewSession {
        session_id: Some("sid-1".into()),
        ..new("audio", "demo")
    };
    let first = s.register(&devbox(), n(), T0).unwrap();
    s.leave(&devbox(), &first.handle, T0 + 1).unwrap();
    assert_ne!(
        s.register(&devbox(), n(), T0 + 2).unwrap().handle,
        first.handle
    );
}

#[test]
fn beat_keeps_a_session_active_and_reports_the_previous_state() {
    let s = store();
    let h = s
        .register(&devbox(), new("kernel", "demo"), T0)
        .unwrap()
        .handle;
    assert_eq!(s.beat(&devbox(), &h, T0 + 300).unwrap(), State::Active);
    let later = T0 + 300 + ACTIVE_WINDOW_SECS + 1;
    assert_eq!(
        s.sessions(Filter::default(), later).unwrap()[0].state(later),
        State::Idle
    );
    assert_eq!(s.beat(&devbox(), &h, later).unwrap(), State::Idle);
    assert_eq!(
        s.sessions(Filter::default(), later).unwrap()[0].state(later),
        State::Active
    );
}

#[test]
fn a_session_without_signal_becomes_idle_then_disappears() {
    let s = store();
    let h = s
        .register(&devbox(), new("kernel", "demo"), T0)
        .unwrap()
        .handle;
    let idle = T0 + ACTIVE_WINDOW_SECS;
    assert_eq!(
        s.sessions(Filter::default(), idle).unwrap()[0].state(idle),
        State::Idle
    );
    let gone = T0 + GONE_AFTER_SECS;
    assert!(s.sessions(Filter::default(), gone).unwrap().is_empty());
    assert!(
        matches!(s.beat(&devbox(), &h, gone), Err(SessionError::Gone)),
        "no resurrection by a late beat"
    );
}

#[test]
fn leave_removes_the_session_and_is_idempotent() {
    let s = store();
    let h = s
        .register(&devbox(), new("kernel", "demo"), T0)
        .unwrap()
        .handle;
    s.leave(&devbox(), &h, T0 + 5).unwrap();
    s.leave(&devbox(), &h, T0 + 9).unwrap();
    assert!(s.sessions(Filter::default(), T0 + 10).unwrap().is_empty());
    assert!(matches!(
        s.beat(&devbox(), &h, T0 + 10),
        Err(SessionError::Gone)
    ));
}

#[test]
fn only_the_owning_machine_and_tool_may_beat_or_leave() {
    let s = store();
    let h = s
        .register(&devbox(), new("kernel", "demo"), T0)
        .unwrap()
        .handle;
    for other in [agent("mac", "claude"), agent("host-a", "codex")] {
        assert!(matches!(
            s.beat(&other, &h, T0),
            Err(SessionError::Forbidden)
        ));
        assert!(matches!(
            s.leave(&other, &h, T0),
            Err(SessionError::Forbidden)
        ));
    }
    assert_eq!(
        s.sessions(Filter::default(), T0).unwrap().len(),
        1,
        "still there"
    );
    assert!(matches!(
        s.beat(&devbox(), "nope-0000", T0),
        Err(SessionError::NotFound)
    ));
}

#[test]
fn who_filters_by_project_and_role_and_sorts_active_first() {
    let s = store();
    let idle = s.register(&devbox(), new("audio", "demo"), T0).unwrap();
    let active = s
        .register(
            &agent("mac", "codex"),
            NewSession {
                roles: vec!["browser".into(), "audio".into()],
                ..new("x", "demo")
            },
            T0 + 2 * ACTIVE_WINDOW_SECS,
        )
        .unwrap();
    s.register(
        &devbox(),
        new("kernel", "core"),
        T0 + 2 * ACTIVE_WINDOW_SECS,
    )
    .unwrap();
    let now = T0 + 2 * ACTIVE_WINDOW_SECS + 1;

    let studio = s
        .sessions(
            Filter {
                project: Some("demo"),
                role: None,
            },
            now,
        )
        .unwrap();
    let handles: Vec<_> = studio.iter().map(|x| x.handle.as_str()).collect();
    assert_eq!(handles, [active.handle.as_str(), idle.handle.as_str()]);
    assert_eq!(
        s.sessions(
            Filter {
                project: None,
                role: Some("audio")
            },
            now
        )
        .unwrap()
        .len(),
        2
    );
    assert_eq!(
        s.sessions(
            Filter {
                project: Some("core"),
                role: Some("audio")
            },
            now
        )
        .unwrap()
        .len(),
        0
    );
    assert_eq!(s.sessions(Filter::default(), now).unwrap().len(), 3);
}

#[test]
fn old_records_are_deleted_on_the_next_registration() {
    let s = store();
    let left = s
        .register(&devbox(), new("kernel", "p"), T0)
        .unwrap()
        .handle;
    s.leave(&devbox(), &left, T0).unwrap();
    let expired = s.register(&devbox(), new("audio", "p"), T0).unwrap().handle;
    let count = |s: &Store| -> i64 {
        s.conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count(&s), 2);
    s.register(&devbox(), new("desktop", "p"), T0 + KEEP_GONE_SECS - 1)
        .unwrap();
    assert_eq!(count(&s), 3, "kept for a week");
    s.register(&devbox(), new("ci", "p"), T0 + KEEP_GONE_SECS + 1)
        .unwrap();
    assert_eq!(count(&s), 2, "the left and the expired record are gone");
    assert!(
        load(&s.conn(), &left).unwrap().is_none() && load(&s.conn(), &expired).unwrap().is_none()
    );
}

#[test]
fn a_session_name_is_stored_cleaned_and_updated() {
    let s = store();
    let named = |name: Option<&str>| NewSession {
        session_id: Some("sid-1".into()),
        name: name.map(str::to_string),
        ..new("audio", "demo")
    };
    let first = s
        .register(&devbox(), named(Some("  Graphics and sound ")), T0)
        .unwrap();
    assert_eq!(first.name.as_deref(), Some("Graphics and sound"));
    // Registering again without a name keeps it; with one, replaces it.
    let again = s.register(&devbox(), named(None), T0 + 1).unwrap();
    assert_eq!(again.name.as_deref(), Some("Graphics and sound"));
    let renamed = s.register(&devbox(), named(Some("Audio")), T0 + 2).unwrap();
    assert_eq!(renamed.name.as_deref(), Some("Audio"));
    s.set_name(&devbox(), &first.handle, "Audio 2", T0 + 3)
        .unwrap();
    s.set_name(&devbox(), &first.handle, "  ", T0 + 4).unwrap();
    let loaded = load(&s.conn(), &first.handle).unwrap().unwrap();
    assert_eq!(
        loaded.name.as_deref(),
        Some("Audio 2"),
        "blank keeps the name"
    );
    assert!(matches!(
        s.set_name(&agent("mac", "claude"), &first.handle, "x", T0),
        Err(SessionError::Forbidden)
    ));
}

#[test]
fn a_bad_session_name_is_refused() {
    let s = store();
    let err = |name: String| {
        s.register(
            &devbox(),
            NewSession {
                name: Some(name),
                ..new("audio", "p")
            },
            T0,
        )
        .unwrap_err()
        .to_string()
    };
    assert!(err("a\nb".into()).contains("control characters"));
    assert!(err("x".repeat(MAX_NAME + 1)).contains("longer than"));
    assert!(err("0123456789abcdef".repeat(4)).contains("secret"));
}

#[test]
fn a_session_says_where_it_is_now() {
    let s = store();
    let h = s.register(&devbox(), new("dev", "p"), T0).unwrap().handle;
    assert!(
        s.set_place(&devbox(), &h, Some("/w/a"), Some("claude/a"))
            .unwrap()
    );
    assert!(
        !s.set_place(&devbox(), &h, Some("/w/a"), Some("claude/a"))
            .unwrap(),
        "nothing changed"
    );
    // An absent value keeps the old one.
    assert!(s.set_place(&devbox(), &h, Some("/w/b"), None).unwrap());
    let loaded = load(&s.conn(), &h).unwrap().unwrap();
    assert_eq!(
        (loaded.worktree.as_deref(), loaded.branch.as_deref()),
        (Some("/w/b"), Some("claude/a"))
    );
    assert!(matches!(
        s.set_place(&agent("mac", "claude"), &h, Some("/x"), None),
        Err(SessionError::Forbidden)
    ));
    assert!(
        s.set_place(&devbox(), &h, Some(&"x".repeat(300)), None)
            .is_err()
    );
}

#[test]
fn registry_adds_no_messages() {
    let s = store();
    s.ensure_projects(&["all".into()]).unwrap();
    let h = s
        .register(&devbox(), new("kernel", "demo"), T0)
        .unwrap()
        .handle;
    s.beat(&devbox(), &h, T0 + 1).unwrap();
    s.leave(&devbox(), &h, T0 + 2).unwrap();
    assert!(s.recent_messages("all", "general", 10).unwrap().is_empty());
    assert_eq!(s.nav().unwrap()[0].topics[0].messages, 0);
}
