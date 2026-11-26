use super::*;

#[test]
fn the_report_of_a_sent_ask_says_what_became_of_the_wake() {
    let row = |wake: Option<&str>| SentRow {
        id: Some(5),
        project: "demo".into(),
        topic: "general".into(),
        text: "x".into(),
        receivers: vec![ReceiverRow {
            handle: "audio-1a2b".into(),
            agent: "codex@mac".into(),
            roles: vec!["audio".into()],
            issue: None,
            active: false,
        }],
        wake: wake.map(String::from),
        dry_run: false,
        similar: vec![],
    };
    let first = "sent #5 to audio-1a2b in demo/general\n";
    assert_eq!(sent_report(&row(None), "ask"), first);
    assert_eq!(
        sent_report(&row(Some("not_needed")), "ask"),
        first,
        "a working session needs no word"
    );
    for (wake, says) in [
        ("requested", "the host of its machine was asked to wake it"),
        ("pending", "a wake is already on its way"),
        ("limit", "woken twice in the last hour"),
        ("no_host", "No host daemon is running on its machine"),
    ] {
        let report = sent_report(&row(Some(wake)), "ask");
        assert!(
            report.starts_with(first) && report.contains(says),
            "{wake}: {report}"
        );
    }
}

#[test]
fn a_look_before_asking_shows_what_was_answered_before() {
    let row = SentRow {
        id: None,
        project: "p".into(),
        topic: "general".into(),
        text: "x".into(),
        receivers: vec![],
        wake: None,
        dry_run: true,
        similar: vec![SimilarRow {
            id: 16,
            question: "which token opens the PR".into(),
            answer: "EXAMPLE_TOKEN".into(),
        }],
    };
    let out = sent_report(&row, "ask");
    assert!(
        out.contains("This was asked before, and answered:\n  #16 which token opens the PR\n     -> EXAMPLE_TOKEN\n"),
        "{out}"
    );
    assert!(out.find("asked before").unwrap() < out.find("send the same command").unwrap());
}

#[test]
fn query_string() {
    assert_eq!(query(None, None), "");
    assert_eq!(query(Some("demo"), None), "?project=demo");
    assert_eq!(query(Some("a b&c"), Some("x")), "?project=a%20b%26c&role=x");
}

#[test]
fn who_output_is_aligned() {
    let rows = vec![
        Row {
            handle: "audio-1a2b".into(),
            name: Some("Audio".into()),
            roles: vec!["audio".into(), "desktop".into()],
            agent: "claude@host-a".into(),
            project: "demo".into(),
            issue: None,
            state: "active".into(),
            last_seen_secs_ago: 5,
        },
        Row {
            handle: "k-1".into(),
            name: None,
            roles: vec!["kernel".into()],
            agent: "codex@mac".into(),
            project: "p".into(),
            issue: Some("#471 svcd".into()),
            state: "idle".into(),
            last_seen_secs_ago: 7300,
        },
    ];
    let out = who_table(&rows);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0],
        "HANDLE      NAME   ROLES          AGENT          PROJECT  ISSUE      STATE   SIGNAL"
    );
    assert_eq!(
        lines[1],
        "audio-1a2b  Audio  audio,desktop  claude@host-a  demo     -          active  now"
    );
    assert!(lines[2].ends_with("idle    2h ago"));
    assert_eq!(who_table(&[]), "nobody is registered\n");
}

#[test]
fn the_session_name_comes_from_the_environment_or_the_claude_record() {
    let home = std::env::temp_dir().join(format!("chappe-names-{}", std::process::id()));
    let sessions = home.join(".claude/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let record = |pid: u32, id: &str, name: &str, at: i64| {
        let v = serde_json::json!({"pid": pid, "sessionId": id, "name": name, "updatedAt": at});
        std::fs::write(sessions.join(format!("{pid}.json")), v.to_string()).unwrap();
    };
    record(1, "sid-1", "Old name", 10);
    record(2, "sid-1", "Graphics and sound", 20);
    record(3, "sid-2", "Other", 5);
    std::fs::write(sessions.join("4.json"), "not json").unwrap();
    let home_dir = home.display().to_string();
    let env = |extra: &[(&str, &str)]| Env {
        vars: [("HOME", home_dir.as_str())]
            .iter()
            .chain(extra)
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
    };
    let plain = env(&[]);
    assert_eq!(
        plain.session_name(Some("sid-1")).as_deref(),
        Some("Graphics and sound"),
        "the latest record of the session"
    );
    assert_eq!(plain.session_name(Some("nope")), None);
    assert_eq!(plain.session_name(None), None);
    assert_eq!(
        env(&[("CHAPPE_NAME", "  My\tname ")])
            .session_name(Some("sid-1"))
            .as_deref(),
        Some("My name"),
        "the environment wins; whitespace is tidied"
    );
    let long = "é".repeat(100);
    assert_eq!(
        env(&[("CHAPPE_NAME", &long)])
            .session_name(None)
            .unwrap()
            .chars()
            .count(),
        60
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn token_comes_from_the_environment_or_a_file_and_is_trimmed() {
    let dir = std::env::temp_dir().join(format!("chappe-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("chappe")).unwrap();
    std::fs::write(dir.join("chappe/token"), "from-file\n").unwrap();
    let env = |pairs: &[(&str, &str)]| Env {
        vars: pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
    };
    let cfg = dir.display().to_string();
    assert_eq!(
        env(&[("CHAPPE_TOKEN", " from-env\n"), ("XDG_CONFIG_HOME", &cfg)])
            .token()
            .unwrap(),
        "from-env"
    );
    assert_eq!(
        env(&[("XDG_CONFIG_HOME", &cfg)]).token().unwrap(),
        "from-file"
    );
    // A machine has one token per tool: the tool's own file wins when there is one.
    std::fs::write(dir.join("chappe/token.codex"), "codex-token\n").unwrap();
    assert_eq!(
        env(&[("XDG_CONFIG_HOME", &cfg), ("CODEX_THREAD_ID", "t-1")])
            .token()
            .unwrap(),
        "codex-token"
    );
    assert_eq!(
        env(&[("XDG_CONFIG_HOME", &cfg), ("CLAUDECODE", "1")])
            .token()
            .unwrap(),
        "from-file",
        "no file of its own: the plain one"
    );
    std::fs::write(dir.join("chappe/token.claude"), "claude-token\n").unwrap();
    assert_eq!(
        env(&[("XDG_CONFIG_HOME", &cfg), ("CLAUDECODE", "1")])
            .token()
            .unwrap(),
        "claude-token"
    );
    let explicit = dir.join("chappe/token").display().to_string();
    assert_eq!(
        env(&[("CHAPPE_TOKEN_FILE", &explicit)]).token().unwrap(),
        "from-file"
    );
    let missing = env(&[("HOME", "/nonexistent")]).token().unwrap_err();
    assert!(missing.contains("no token") && missing.contains("CHAPPE_TOKEN"));
    std::fs::write(dir.join("chappe/token"), "  \n").unwrap();
    assert!(
        env(&[("XDG_CONFIG_HOME", &cfg)])
            .token()
            .unwrap_err()
            .contains("empty")
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn the_session_id_is_the_one_of_the_tool_that_runs_the_command() {
    let env = |pairs: &[(&str, &str)]| Env {
        vars: pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        cwd: None,
    };
    assert_eq!(
        env(&[("CLAUDE_CODE_SESSION_ID", "c-1")]).tool_session_id(),
        Some("c-1")
    );
    assert_eq!(
        env(&[("CODEX_THREAD_ID", "x-1")]).tool_session_id(),
        Some("x-1")
    );
    // Codex started from a Claude Code session has both.
    assert_eq!(
        env(&[
            ("CLAUDE_CODE_SESSION_ID", "c-1"),
            ("CODEX_THREAD_ID", "x-1")
        ])
        .tool_session_id(),
        Some("x-1")
    );
    assert_eq!(env(&[]).tool_session_id(), None);
}

#[test]
fn handle_comes_from_the_flag_or_the_environment() {
    let env = Env {
        vars: [("CHAPPE_HANDLE".to_string(), "env-1".to_string())].into(),
        cwd: None,
    };
    assert_eq!(handle_of(Some("flag-1".into()), &env).unwrap(), "flag-1");
    assert_eq!(handle_of(None, &env).unwrap(), "env-1");
    assert!(
        handle_of(
            None,
            &Env {
                vars: HashMap::new(),
                cwd: None
            }
        )
        .is_err()
    );
}
