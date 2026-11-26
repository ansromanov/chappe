use super::*;
use crate::forge::GiteaFeed;
use crate::web::testutil::*;

async fn register(app: &Router, token: &str, body: &str) -> String {
    let (status, _, out) = call(app, api("POST", "/v1/sessions", Some(token), Some(body))).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    let start = out.find(r#""handle":""#).unwrap() + 10;
    out[start..start + out[start..].find('"').unwrap()].to_string()
}

#[tokio::test]
async fn token_is_accepted() {
    let (app, _) = app();
    let (status, _, body) = call(&app, api("GET", "/v1/whoami", Some(TOKEN), None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"machine":"host-a","tool":"claude"}"#);
}

#[tokio::test]
async fn missing_wrong_or_malformed_token_is_rejected() {
    let (app, _) = app();
    for authorization in [
        None,
        Some("Bearer nope"),
        Some("Basic abc"),
        Some("Bearer "),
        Some(TOKEN),
    ] {
        let mut req = axum::http::Request::get("/v1/whoami");
        if let Some(value) = authorization {
            req = req.header(header::AUTHORIZATION, value);
        }
        let (status, headers, body) =
            call(&app, req.body(axum::body::Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
        assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
        assert!(!body.contains(TOKEN));
    }
}

#[tokio::test]
async fn agent_interface_does_not_expose_the_owner_page() {
    let (app, _) = app();
    assert_eq!(call(&app, get_req("/v1/")).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registry_interface_needs_a_token() {
    let (app, _) = app();
    for (method, uri) in [
        ("POST", "/v1/sessions"),
        ("GET", "/v1/sessions"),
        ("POST", "/v1/sessions/x-0000/beat"),
        ("DELETE", "/v1/sessions/x-0000"),
        ("POST", "/v1/topics"),
        ("GET", "/v1/topics"),
    ] {
        let body = (method == "POST" && uri != "/v1/sessions/x-0000/beat")
            .then_some(r#"{"roles":["a"],"project":"p"}"#);
        for token in [None, Some("wrong")] {
            let (status, ..) = call(&app, api(method, uri, token, body)).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {token:?}");
        }
    }
}

#[tokio::test]
async fn beat_and_leave_belong_to_the_registering_machine() {
    let (app, _) = app();
    let h = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let beat = format!("/v1/sessions/{h}/beat");
    assert_eq!(
        call(&app, api("POST", &beat, Some(TOKEN), None)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, api("POST", &beat, Some(TOKEN_MAC), None))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let leave = format!("/v1/sessions/{h}");
    assert_eq!(
        call(&app, api("DELETE", &leave, Some(TOKEN_MAC), None))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &app,
            api("POST", "/v1/sessions/nope-0000/beat", Some(TOKEN), None)
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, api("DELETE", &leave, Some(TOKEN), None)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, api("DELETE", &leave, Some(TOKEN), None)).await.0,
        StatusCode::OK,
        "leaving twice is fine"
    );
    assert_eq!(
        call(&app, api("POST", &beat, Some(TOKEN), None)).await.0,
        StatusCode::GONE
    );
}

#[tokio::test]
async fn bad_registrations_are_refused_with_a_json_reason() {
    let (app, state) = app();
    for (body, reason) in [
        (r#"{"roles":[],"project":"p"}"#, "at least one role"),
        (r#"{"roles":["Bad Role"],"project":"p"}"#, "role"),
        (r#"{"roles":["a"],"project":"P P"}"#, "project"),
        (
            r#"{"roles":["a"],"project":"p","agent":"claude@other"}"#,
            "unknown field",
        ),
        (r#"{"roles":"a","project":"p"}"#, "roles"),
        ("not json", ""),
    ] {
        let (status, headers, out) =
            call(&app, api("POST", "/v1/sessions", Some(TOKEN), Some(body))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        assert!(
            out.starts_with(r#"{"error":""#) && out.contains(reason),
            "{out}"
        );
    }
    let secret = format!(
        r#"{{"roles":["a"],"project":"p","worktree":"/x/{}"}}"#,
        "ab".repeat(32)
    );
    let (status, _, out) = call(
        &app,
        api("POST", "/v1/sessions", Some(TOKEN), Some(&secret)),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(out.contains("looks like a secret") && !out.contains(&"ab".repeat(32)));
    assert!(
        state
            .store
            .sessions(Filter::default(), now())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn who_filters_by_project_and_role_and_hides_private_fields() {
    let (app, _) = app();
    register(
        &app,
        TOKEN,
        r#"{"roles":["browser"],"project":"demo","session_id":"sid-1","transcript":"/t"}"#,
    )
    .await;
    register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"core"}"#).await;
    let count = |out: &str| out.matches(r#""handle""#).count();
    let get = |q: &str| api("GET", &format!("/v1/sessions{q}"), Some(TOKEN), None);
    let (_, _, all) = call(&app, get("")).await;
    assert_eq!(count(&all), 2);
    assert!(
        all.contains(r#""agent":"codex@mac""#)
            && !all.contains("transcript")
            && !all.contains("session_id")
    );
    assert_eq!(count(&call(&app, get("?project=demo")).await.2), 1);
    assert_eq!(count(&call(&app, get("?role=audio")).await.2), 1);
    assert_eq!(
        count(&call(&app, get("?project=demo&role=audio")).await.2),
        0
    );
}

#[tokio::test]
async fn the_session_name_is_listed_and_follows_a_rename_through_the_inbox() {
    let (app, _) = app();
    let h = register(
        &app,
        TOKEN,
        r#"{"roles":["audio"],"project":"demo","name":"Graphics and sound"}"#,
    )
    .await;
    let who = || call(&app, api("GET", "/v1/sessions", Some(TOKEN), None));
    assert!(who().await.2.contains(r#""name":"Graphics and sound""#));
    let inbox = |name: &str| {
        api(
            "POST",
            "/v1/inbox",
            Some(TOKEN),
            Some(&format!(r#"{{"handle":"{h}","name":"{name}"}}"#)),
        )
    };
    let (status, ..) = call(&app, inbox("Audio")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(who().await.2.contains(r#""name":"Audio""#));
    // A name the service cannot take does not cost the session its inbox.
    let (status, ..) = call(&app, inbox(&"x".repeat(80))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(who().await.2.contains(r#""name":"Audio""#));
}

#[tokio::test]
async fn reading_the_inbox_tells_the_page_that_the_ask_was_delivered() {
    let (app, state) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    let ask = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"updated the driver","expect":"works or not","by":"30m"}}"#
    );
    let (status, _) = post(&app, TOKEN, "/v1/messages", &ask).await;
    assert_eq!(status, StatusCode::CREATED);
    let mut stream = state.events.subscribe();
    while stream.try_recv().is_ok() {}
    let (status, inbox) = post(
        &app,
        TOKEN_MAC,
        "/v1/inbox",
        &format!(r#"{{"handle":"{audio}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    assert!(inbox.contains("check the sound"));
    let heard: Vec<_> = std::iter::from_fn(|| stream.try_recv().ok()).collect();
    assert_eq!(
        heard,
        ["msg:demo/general"],
        "once, for the topic of the ask"
    );
}

#[tokio::test]
async fn the_inbox_call_tells_where_the_session_is_and_who_shows_its_claimed_issue() {
    let (app, state) = app();
    let h = register(&app, TOKEN, r#"{"roles":["dev"],"project":"core"}"#).await;
    let claim = serde_json::json!({
        "action": "created",
        "comment": {"id": 1, "body": "<!-- agent-lock: claimed -->\n- branch: `claude/x`"},
        "issue": {"number": 618, "title": "board"},
        "repository": {"full_name": "acme/core"},
    });
    state
        .store
        .gitea_event("issue_comment", "d1", &claim, now())
        .unwrap();
    let who = || call(&app, api("GET", "/v1/sessions", Some(TOKEN), None));
    assert!(who().await.2.contains(r#""issue":null"#));
    let mut stream = state.events.subscribe();
    while stream.try_recv().is_ok() {}
    let body = format!(r#"{{"handle":"{h}","worktree":"/w/x","branch":"claude/x"}}"#);
    let (status, _) = post(&app, TOKEN, "/v1/inbox", &body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stream.try_recv().ok().as_deref(), Some("board"));
    assert!(who().await.2.contains(r##""issue":"#618 board""##));
    // The same place again changes nothing; a bad value does not cost the inbox.
    post(&app, TOKEN, "/v1/inbox", &body).await;
    assert!(stream.try_recv().is_err());
    let bad = format!(r#"{{"handle":"{h}","branch":"{}"}}"#, "x".repeat(200));
    assert_eq!(post(&app, TOKEN, "/v1/inbox", &bad).await.0, StatusCode::OK);
}

#[tokio::test]
async fn a_session_that_registers_is_told_who_else_works_there_and_what_is_held() {
    let (app, state) = app();
    let first = api(
        "POST",
        "/v1/sessions",
        Some(TOKEN),
        Some(r#"{"roles":["dev"],"project":"core","name":"First","branch":"claude/x"}"#),
    );
    let (_, _, alone) = call(&app, first).await;
    assert!(
        !alone.contains("briefing"),
        "nobody else, nothing held: {alone}"
    );
    let claim = serde_json::json!({
        "action": "created",
        "comment": {"id": 1, "body": "<!-- agent-lock: claimed -->\n- branch: `claude/x`"},
        "issue": {"number": 618, "title": "board"},
        "repository": {"full_name": "acme/core"},
    });
    state
        .store
        .gitea_event("issue_comment", "d1", &claim, now())
        .unwrap();
    let lease = serde_json::json!({
        "action": "created",
        "comment": {"id": 2, "body": "<!-- resource-lease: exclusive -->\nagent: codex@mac\naction: reboot\nuntil: 2099-01-01 10:00 UTC"},
        "issue": {"number": 480, "title": "Laptop"},
        "repository": {"full_name": "acme/demo"},
    });
    state
        .store
        .gitea_event("issue_comment", "d2", &lease, now())
        .unwrap();
    let second = api(
        "POST",
        "/v1/sessions",
        Some(TOKEN_MAC),
        Some(r#"{"roles":["dev"],"project":"core"}"#),
    );
    let (_, _, out) = call(&app, second).await;
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let text = v["briefing"].as_str().unwrap();
    assert!(text.contains("Working in project core now:"), "{text}");
    assert!(
        text.contains("\"First\" (claude@host-a), task: #618 board, branch claude/x, active"),
        "{text}"
    );
    assert!(text.contains("Resource \"Laptop\" busy (exclusive lease): codex@mac, until 10:00 UTC (acme/demo#480)."), "{text}");
    assert!(
        !text.contains(v["handle"].as_str().unwrap()),
        "not about itself"
    );
    // Another project sees the lease but not those sessions.
    let third = api(
        "POST",
        "/v1/sessions",
        Some(TOKEN),
        Some(r#"{"roles":["dev"],"project":"other"}"#),
    );
    let (_, _, out) = call(&app, third).await;
    assert!(out.contains("Laptop") && !out.contains("First"), "{out}");
}

#[tokio::test]
async fn a_session_creates_a_topic_in_its_own_project() {
    let (app, state) = app();
    let mut events = state.events.subscribe();
    let h = register(&app, TOKEN, r#"{"roles":["audio"],"project":"demo"}"#).await;
    while events.try_recv().is_ok() {}
    let body = format!(r#"{{"handle":"{h}","name":"Audio Driver","about":"sound driver"}}"#);
    let (status, _, out) = call(&app, api("POST", "/v1/topics", Some(TOKEN), Some(&body))).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert!(out.contains(r#""project":"demo""#) && out.contains(r#""name":"audio-driver""#));
    assert_eq!(events.try_recv().unwrap(), "nav");
    let (_, _, page) = call(&app, get_req("/p/demo/audio-driver")).await;
    assert!(page.contains("sound driver"));
    let (_, _, list) = call(
        &app,
        api("GET", "/v1/topics?project=demo", Some(TOKEN), None),
    )
    .await;
    assert!(
        list.contains("audio-driver")
            && list.contains(r#""name":"general""#)
            && !list.contains(r#""project":"all""#)
    );
    let (_, _, every) = call(&app, api("GET", "/v1/topics", Some(TOKEN), None)).await;
    assert!(every.contains(r#""project":"all""#));
}

#[tokio::test]
async fn topic_creation_is_checked() {
    let (app, _) = app();
    let h = register(&app, TOKEN, r#"{"roles":["audio"],"project":"demo"}"#).await;
    let create = |token: &str, handle: &str, name: &str| {
        api(
            "POST",
            "/v1/topics",
            Some(token),
            Some(&format!(r#"{{"handle":"{handle}","name":"{name}"}}"#)),
        )
    };
    assert_eq!(
        call(&app, create(TOKEN, &h, "general")).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&app, create(TOKEN, &h, "a/b")).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&app, create(TOKEN, "nope-0000", "x")).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&app, create(TOKEN_MAC, &h, "x")).await.0,
        StatusCode::FORBIDDEN,
        "not your session"
    );
    for i in 0..3 {
        assert_eq!(
            call(&app, create(TOKEN, &h, &format!("t{i}"))).await.0,
            StatusCode::CREATED
        );
    }
    let (status, _, out) = call(&app, create(TOKEN, &h, "t3")).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(out.contains("no more than three"));
    let extra = api(
        "POST",
        "/v1/topics",
        Some(TOKEN),
        Some(&format!(r#"{{"handle":"{h}","name":"z","project":"all"}}"#)),
    );
    assert_eq!(
        call(&app, extra).await.0,
        StatusCode::UNPROCESSABLE_ENTITY,
        "the project is not a choice"
    );
    call(
        &app,
        api("DELETE", &format!("/v1/sessions/{h}"), Some(TOKEN), None),
    )
    .await;
    assert_eq!(
        call(&app, create(TOKEN, &h, "late")).await.0,
        StatusCode::GONE,
        "an ended session cannot"
    );
}

async fn post(app: &Router, token: &str, path: &str, body: &str) -> (StatusCode, String) {
    let (status, _, out) = call(app, api("POST", path, Some(token), Some(body))).await;
    (status, out)
}

fn id_of(out: &str) -> i64 {
    let start = out.find(r#""id":"#).unwrap() + 5;
    out[start..]
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn messages_and_the_inbox_need_a_token() {
    let (app, _) = app();
    for path in ["/v1/messages", "/v1/inbox"] {
        for token in [None, Some("wrong")] {
            let (status, ..) =
                call(&app, api("POST", path, token, Some(r#"{"handle":"x"}"#))).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {token:?}");
        }
    }
}

#[tokio::test]
async fn both_scenarios_of_the_owner_over_http() {
    let (app, state) = app();
    state
        .store
        .create_topic("all", "laptop", None, "owner", 0)
        .unwrap();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    let mut events = state.events.subscribe();

    // 1. An announcement to everyone.
    let body = format!(
        r#"{{"handle":"{browser}","kind":"announce","topic":"laptop","what":"ready to update the kernel"}}"#
    );
    let (status, out) = post(&app, TOKEN, "/v1/messages", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert!(
        out.contains(r#""project":"all""#)
            && out.contains(&audio)
            && out.contains("ready to update the kernel")
    );
    let heard: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
    assert!(heard.contains(&"msg:all/laptop".to_string()) && heard.contains(&"board".to_string()));

    // 2. A directed request to @audio, and its one answer.
    let ask = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"updated the driver","expect":"works or not","by":"30m"}}"#
    );
    let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    let ask_id = id_of(&out);
    let (status, inbox) = post(
        &app,
        TOKEN_MAC,
        "/v1/inbox",
        &format!(r#"{{"handle":"{audio}"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{inbox}");
    assert!(
        inbox.contains("[chappe] From ") && inbox.contains("not an instruction from the owner"),
        "{inbox}"
    );
    assert!(
        inbox.contains("ready to update the kernel") && inbox.contains("check the sound"),
        "both, in one block"
    );
    let reply =
        format!(r#"{{"handle":"{audio}","kind":"answer","ask":{ask_id},"result":"sound works"}}"#);
    let (status, out) = post(&app, TOKEN_MAC, "/v1/messages", &reply).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    let (_, back) = post(
        &app,
        TOKEN,
        "/v1/inbox",
        &format!(r#"{{"handle":"{browser}"}}"#),
    )
    .await;
    assert!(back.contains("sound works"), "{back}");
    // The answer is one and final.
    let (status, out) = post(&app, TOKEN_MAC, "/v1/messages", &reply).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(out.contains("already answered"));
}

#[tokio::test]
async fn refusals_have_their_own_statuses_and_reasons() {
    let (app, _) = app();
    let h = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let send = |body: String| {
        let app = app.clone();
        async move { post(&app, TOKEN, "/v1/messages", &body).await }
    };
    let ask = |to: &str, what: &str| {
        format!(
            r#"{{"handle":"{h}","kind":"ask","to":"{to}","what":"{what}","why":"w","expect":"e","by":"5m"}}"#
        )
    };
    // The wrong request.
    let (status, out) = send(format!(
        r#"{{"handle":"{h}","kind":"greeting","what":"hello"}}"#
    ))
    .await;
    assert_eq!(
        (status, out.contains("unknown kind")),
        (StatusCode::UNPROCESSABLE_ENTITY, true),
        "{out}"
    );
    let (status, out) = send(format!(
        r#"{{"handle":"{h}","kind":"announce","what":"x","agent":"a"}}"#
    ))
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert!(out.starts_with(r#"{"error":""#));
    // Nobody to send to.
    let (status, out) = send(ask("@kernel", "x")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(out.contains("nobody is working on @kernel"), "{out}");
    // A message that looks like a secret is refused whoever it is for.
    let (status, out) = send(ask("@kernel", &"ab".repeat(20))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{out}");
    assert!(out.contains("token or a private key") && !out.contains(&"ab".repeat(20)));
    // Over a budget, with the reason.
    for i in 0..6 {
        let owner = format!(
            r#"{{"handle":"{h}","kind":"owner","what":"question {i}","why":"w","expect":"e"}}"#
        );
        assert_eq!(send(owner).await.0, StatusCode::CREATED);
    }
    let over =
        format!(r#"{{"handle":"{h}","kind":"owner","what":"seventh","why":"w","expect":"e"}}"#);
    let (status, out) = send(over).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(out.contains("6 messages per hour"), "{out}");
    // Another machine's session, and a session that has ended.
    let (status, ..) = post(
        &app,
        TOKEN_MAC,
        "/v1/messages",
        &format!(r#"{{"handle":"{h}","kind":"announce","what":"x"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    call(
        &app,
        api("DELETE", &format!("/v1/sessions/{h}"), Some(TOKEN), None),
    )
    .await;
    let (status, ..) = post(
        &app,
        TOKEN,
        "/v1/messages",
        &format!(r#"{{"handle":"{h}","kind":"announce","what":"y"}}"#),
    )
    .await;
    assert_eq!(status, StatusCode::GONE);
    let (status, ..) = post(&app, TOKEN, "/v1/inbox", &format!(r#"{{"handle":"{h}"}}"#)).await;
    assert_eq!(status, StatusCode::GONE);
}

#[tokio::test]
async fn a_dry_run_says_who_would_get_it_and_sends_nothing() {
    let (app, state) = app();
    let browser = register(
        &app,
        TOKEN,
        r##"{"roles":["browser"],"project":"demo","issue":"#471"}"##,
    )
    .await;
    let audio = register(
        &app,
        TOKEN_MAC,
        r##"{"roles":["audio","desktop"],"project":"demo","issue":"#486"}"##,
    )
    .await;
    let body = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"x","why":"y","expect":"z","by":"1h","dry_run":true}}"#
    );
    let (status, out) = post(&app, TOKEN, "/v1/messages", &body).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(
        out.contains(r#""dry_run":true"#) && out.contains(r#""id":null"#) && out.contains(&audio)
    );
    assert!(out.contains(r##""issue":"#486""##) && out.contains(r#""roles":["audio","desktop"]"#));
    assert!(
        state
            .store
            .recent_messages("demo", "general", 10)
            .unwrap()
            .is_empty()
    );
    let (_, inbox) = post(
        &app,
        TOKEN_MAC,
        "/v1/inbox",
        &format!(r#"{{"handle":"{audio}"}}"#),
    )
    .await;
    assert!(inbox.contains(r#""messages":[]"#));
}

#[tokio::test]
async fn the_inbox_holds_messages_back_for_five_minutes_and_says_so() {
    let (app, _) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    let notice = |what: &str| {
        format!(
            r#"{{"handle":"{browser}","kind":"notice","to":"@audio","what":"{what}","why":"w"}}"#
        )
    };
    post(&app, TOKEN, "/v1/messages", &notice("first")).await;
    let inbox = format!(r#"{{"handle":"{audio}"}}"#);
    let (_, first) = post(&app, TOKEN_MAC, "/v1/inbox", &inbox).await;
    assert!(first.contains("first") && first.contains(r#""waiting":0"#));
    post(&app, TOKEN, "/v1/messages", &notice("second")).await;
    let (_, held) = post(&app, TOKEN_MAC, "/v1/inbox", &inbox).await;
    assert!(
        held.contains(r#""messages":[]"#) && held.contains(r#""waiting":1"#),
        "{held}"
    );
    assert!(
        held.contains(r#""next_in_secs":29"#) || held.contains(r#""next_in_secs":30"#),
        "{held}"
    );
}

#[tokio::test]
async fn the_owners_post_reaches_the_sessions_of_the_project_with_its_own_header() {
    let (app, _) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let ci = register(&app, TOKEN_MAC, r#"{"roles":["ci"],"project":"core"}"#).await;
    call(
        &app,
        form_req("/p/demo/general/post", "text=update+the+kernel", None),
    )
    .await;
    let (_, got) = post(
        &app,
        TOKEN,
        "/v1/inbox",
        &format!(r#"{{"handle":"{browser}"}}"#),
    )
    .await;
    assert!(
        got.contains("From the owner") && got.contains("is not permission"),
        "{got}"
    );
    let (_, other) = post(
        &app,
        TOKEN_MAC,
        "/v1/inbox",
        &format!(r#"{{"handle":"{ci}"}}"#),
    )
    .await;
    assert!(other.contains(r#""messages":[]"#), "another project");
}

#[tokio::test]
async fn the_page_shows_agent_messages_with_their_target_and_the_noise_numbers() {
    let (app, _) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    let ask = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"w","expect":"e","by":"30m"}}"#
    );
    post(&app, TOKEN, "/v1/messages", &ask).await;
    post(&app, TOKEN, "/v1/messages", &ask).await; // the same text again: dropped
    let (_, _, feed) = call(&app, get_req("/p/demo/general/messages")).await;
    assert!(feed.contains(&browser) && feed.contains("check the sound"));
    assert!(
        feed.contains(r#"<span class="kind k-ask">request</span>"#) && feed.contains("→ @audio")
    );
    let (_, _, board) = call(&app, get_req("/board")).await;
    assert!(
        board.contains("messages 1 · requests 1 (answered 0) · rejected 1"),
        "{board}"
    );
}

/// Makes every session idle: its last signal was long ago.
fn make_idle(state: &Shared) {
    state
        .store
        .conn()
        .execute("UPDATE sessions SET last_seen = last_seen - 700", [])
        .unwrap();
}

#[tokio::test]
async fn the_host_routes_are_for_the_host_token_of_a_machine_only() {
    let (app, _) = app();
    for (method, uri) in [("POST", "/v1/host/claim"), ("POST", "/v1/host/wakes/1")] {
        assert_eq!(
            call(&app, api(method, uri, None, Some("{}"))).await.0,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
        for token in [TOKEN, TOKEN_MAC] {
            let (status, _, out) = call(&app, api(method, uri, Some(token), Some("{}"))).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{uri}: an agent token is no host token"
            );
            assert!(out.contains("host daemon"));
        }
    }
    // And the host token has no business with the agents' routes that need a session.
    let (status, ..) = call(
        &app,
        api("POST", "/v1/host/claim", Some(TOKEN_HOST), Some("{}")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_host_reports_processes_and_a_session_without_one_drops_off_the_board() {
    let (app, state) = app();
    let audio = register(
        &app,
        TOKEN_MAC,
        r#"{"roles":["audio"],"project":"demo","session_id":"sid-9"}"#,
    )
    .await;
    let who = |app: &Router| {
        let app = app.clone();
        async move {
            let (_, _, out) = call(&app, api("GET", "/v1/sessions", Some(TOKEN), None)).await;
            out
        }
    };
    state
        .store
        .conn()
        .execute("UPDATE sessions SET last_seen = last_seen - 8000", [])
        .unwrap();
    // The process is there: silence alone does not end the session.
    let alive = r#"{"running":{"codex":["sid-9"]}}"#;
    let (status, _) = post(&app, TOKEN_HOST, "/v1/host/claim", alive).await;
    assert_eq!(status, StatusCode::OK);
    assert!(who(&app).await.contains(&audio));
    // The process is gone: after two hours of silence the session is gone too.
    let (status, _) = post(
        &app,
        TOKEN_HOST,
        "/v1/host/claim",
        r#"{"running":{"codex":[]}}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!who(&app).await.contains(&audio));
    // An agent token cannot make such a report.
    let (status, _) = post(&app, TOKEN_MAC, "/v1/host/claim", alive).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn an_agent_asks_whether_the_host_daemon_of_its_machine_answers() {
    let (app, _state) = app();
    let (status, _, out) = call(&app, api("GET", "/v1/host/status", Some(TOKEN_MAC), None)).await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert!(
        out.contains(r#""machine":"mac""#) && out.contains(r#""up":false"#),
        "{out}"
    );
    assert!(out.contains(r#""last_poll_secs_ago":null"#), "{out}");
    post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    let (_, _, out) = call(&app, api("GET", "/v1/host/status", Some(TOKEN_MAC), None)).await;
    assert!(out.contains(r#""up":true"#), "{out}");
    let (status, _, _) = call(&app, api("GET", "/v1/host/status", None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_ask_to_an_idle_session_is_woken_by_the_host_over_http() {
    let (app, state) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let audio = register(
        &app,
        TOKEN_MAC,
        r#"{"roles":["audio"],"project":"demo","session_id":"sid-9","worktree":"/work/audio"}"#,
    )
    .await;
    make_idle(&state);
    // The host daemon of the mac is up: it has just asked for work.
    post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    // The browser asks; both sessions are idle, but the sender is the one that just spoke.
    let ask = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"w","expect":"e","by":"2h","wait":true}}"#
    );
    let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert!(out.contains(r#""wake":"requested""#), "{out}");

    // The host of the mac claims it, with what it needs to wake the session.
    let (status, claimed) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        claimed.contains(&format!(r#""handle":"{audio}""#))
            && claimed.contains(r#""tool":"codex""#),
        "{claimed}"
    );
    assert!(
        claimed.contains(r#""session_id":"sid-9""#)
            && claimed.contains(r#""worktree":"/work/audio""#)
    );
    assert!(claimed.contains("[chappe] From ") && claimed.contains("check the sound"));
    let id = id_of(&claimed);
    let report = format!(
        r#"{{"outcome":"delivered","message_ids":[{}]}}"#,
        id_of(&out)
    );
    let (status, _) = post(&app, TOKEN_HOST, &format!("/v1/host/wakes/{id}"), &report).await;
    assert_eq!(status, StatusCode::OK);
    // The session has its message; its inbox has nothing more.
    let (_, inbox) = post(
        &app,
        TOKEN_MAC,
        "/v1/inbox",
        &format!(r#"{{"handle":"{audio}"}}"#),
    )
    .await;
    assert!(inbox.contains(r#""messages":[]"#), "{inbox}");
    let (_, again) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    assert_eq!(again, "[]");
}

#[tokio::test]
async fn the_third_wake_in_an_hour_is_queued_and_the_sender_is_told_on_the_spot() {
    let (app, state) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    for i in 0..3 {
        make_idle(&state);
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"question {i}","why":"w","expect":"e","by":"2h"}}"#
        );
        let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        let expected = if i < 2 { "requested" } else { "limit" };
        assert!(
            out.contains(&format!(r#""wake":"{expected}""#)),
            "{i}: {out}"
        );
        let (_, claimed) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        if i < 2 {
            let report = format!(
                r#"{{"outcome":"delivered","message_ids":[{}]}}"#,
                id_of(&out)
            );
            post(
                &app,
                TOKEN_HOST,
                &format!("/v1/host/wakes/{}", id_of(&claimed)),
                &report,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn a_wake_that_cannot_be_done_leaves_a_note_in_the_senders_inbox() {
    let (app, state) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    make_idle(&state);
    post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    let ask = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"x","why":"w","expect":"e","by":"2h"}}"#
    );
    post(&app, TOKEN, "/v1/messages", &ask).await;
    let (_, claimed) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    let report = r#"{"outcome":"unsupported","reason":"no adapter for codex"}"#;
    post(
        &app,
        TOKEN_HOST,
        &format!("/v1/host/wakes/{}", id_of(&claimed)),
        report,
    )
    .await;
    let (_, inbox) = post(
        &app,
        TOKEN,
        "/v1/inbox",
        &format!(r#"{{"handle":"{browser}"}}"#),
    )
    .await;
    assert!(
        inbox.contains("has no way to wake") && inbox.contains(&audio),
        "{inbox}"
    );
    // A report that is not one of the four is refused.
    let (status, _) = post(
        &app,
        TOKEN_HOST,
        "/v1/host/wakes/1",
        r#"{"outcome":"explode"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = post(
        &app,
        TOKEN_HOST,
        "/v1/host/wakes/999",
        r#"{"outcome":"deferred","reason":"x"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn without_a_host_the_sender_is_told_there_is_nobody_to_wake_the_session() {
    let (app, state) = app();
    let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
    register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
    make_idle(&state);
    let ask = format!(
        r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"x","why":"w","expect":"e","by":"2h"}}"#
    );
    let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
    assert_eq!(status, StatusCode::CREATED, "{out}");
    assert!(out.contains(r#""wake":"no_host""#), "{out}");
    // No wake was made while nobody could carry it out. A host that comes up
    // afterwards finds the ask still waiting, and the session is woken then.
    let (_, jobs) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
    assert!(
        jobs.contains(r#""tool":"codex""#) && jobs.contains("Need: x"),
        "{jobs}"
    );
}

#[tokio::test]
async fn a_long_poll_waits_and_comes_back_empty() {
    let (app, _) = app();
    let started = std::time::Instant::now();
    let (status, out) = post(&app, TOKEN_HOST, "/v1/host/claim", r#"{"wait_secs":1}"#).await;
    assert_eq!((status, out.as_str()), (StatusCode::OK, "[]"));
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
}
