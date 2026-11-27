use super::*;
use crate::message::Kind;

fn msg(id: i64, sender: &str, text: &str, at: i64) -> Message {
    Message {
        id,
        project: "demo".into(),
        topic: "general".into(),
        sender: sender.into(),
        sender_name: None,
        kind: Kind::Announce,
        text: text.into(),
        created_at: at,
        target: None,
        ask: None,
        answers: None,
    }
}

fn topic(name: &str, messages: i64) -> TopicInfo {
    TopicInfo {
        name: name.into(),
        about: None,
        messages,
        last_at: None,
    }
}

fn nav() -> Vec<ProjectNav> {
    vec![
        ProjectNav {
            name: "all".into(),
            topics: vec![topic("general", 0)],
        },
        ProjectNav {
            name: "demo".into(),
            topics: vec![
                topic("general", 3),
                TopicInfo {
                    about: Some("driver".into()),
                    ..topic("audio", 0)
                },
            ],
        },
    ]
}

fn session(handle: &str, project: &str, last_seen: i64) -> Session {
    Session {
        handle: handle.into(),
        machine: "host-a".into(),
        tool: "claude".into(),
        roles: vec!["audio".into(), "desktop".into()],
        project: project.into(),
        issue: Some("#471 svcd".into()),
        branch: None,
        worktree: Some("/home/x".into()),
        session_id: Some("secret-ish-id".into()),
        transcript: Some("/home/x/t.jsonl".into()),
        name: None,
        registered_at: last_seen,
        last_seen,
        left_at: None,
        process_gone_at: None,
    }
}

#[test]
fn escapes_the_five_characters() {
    assert_eq!(
        escape(r#"<a href="x" onclick='y'>&"#),
        "&lt;a href=&quot;x&quot; onclick=&#39;y&#39;&gt;&amp;"
    );
    assert_eq!(escape("plain tëxt"), "plain tëxt");
}

#[test]
fn path_segments_are_percent_encoded() {
    assert_eq!(path_escape("audio-driver"), "audio-driver");
    assert_eq!(path_escape("zvük"), "zv%C3%BCk");
    assert_eq!(path_escape("a/b?c#d e"), "a%2Fb%3Fc%23d%20e");
}

#[test]
fn markup_in_text_is_inert() {
    let out = linkify("<script>alert(1)</script><img src=x onerror=alert(1)>");
    assert!(!out.contains('<'));
    assert!(out.contains("&lt;script&gt;"));
}

#[test]
fn links_become_anchors() {
    let out = linkify("see https://forge.example.com/acme/core/issues/537.");
    assert_eq!(
        out,
        r#"see <a href="https://forge.example.com/acme/core/issues/537" rel="noopener noreferrer nofollow" target="_blank">https://forge.example.com/acme/core/issues/537</a>."#
    );
}

#[test]
fn link_with_quotes_cannot_break_out_of_the_attribute() {
    let out = linkify(r#"https://x.test/"onmouseover="alert(1)"#);
    assert!(!out.contains(r#"href="https://x.test/"onmouseover"#));
    assert!(out.contains("&quot;onmouseover=&quot;alert(1"));
    assert_eq!(out.matches("<a ").count(), 1);
}

#[test]
fn only_http_and_https_are_linked() {
    for text in [
        "javascript:alert(1)",
        "data:text/html,x",
        "ftp://x.test/",
        "//x.test",
    ] {
        assert!(!linkify(text).contains("<a "), "{text}");
    }
    assert!(
        linkify("HTTPS://x.test").find("<a ").is_none(),
        "scheme match is exact"
    );
}

#[test]
fn bare_scheme_and_several_links() {
    assert_eq!(linkify("https:// is not a link"), "https:// is not a link");
    let out = linkify("http://a.test and (https://b.test/x)");
    assert_eq!(out.matches("<a ").count(), 2);
    assert!(out.ends_with("</a>)"));
}

#[test]
fn ago_labels() {
    assert_eq!(format_ago(-5), "just now");
    assert_eq!(format_ago(59), "just now");
    assert_eq!(format_ago(60), "1 min ago");
    assert_eq!(format_ago(3599), "59 min ago");
    assert_eq!(format_ago(7200), "2 h ago");
}

#[test]
fn the_sidebar_lists_projects_with_general_and_marks_the_open_topic() {
    let out = nav_fragment(&nav(), Some(("demo", "audio")));
    assert_eq!(out.matches(r#"<details class="proj""#).count(), 2);
    assert_eq!(
        out.matches("# general").count(),
        0,
        "the hash is its own element"
    );
    assert_eq!(out.matches(r#"href="/p/all/general""#).count(), 1);
    assert_eq!(out.matches(r#"href="/p/demo/general""#).count(), 1);
    assert!(out.contains(r#"href="/p/demo/audio" class="topic current" title="driver""#));
    assert_eq!(out.matches("current").count(), 1);
    assert!(out.contains(r#"<span class="count">3</span>"#));
    assert_eq!(out.matches(r#"action="/p/demo/topics""#).count(), 1);
}

#[test]
fn the_sidebar_escapes_names_and_encodes_links() {
    let hostile = vec![ProjectNav {
        name: "a".into(),
        topics: vec![TopicInfo {
            about: Some(r#""><script>"#.into()),
            ..topic("zvük", 0)
        }],
    }];
    let out = nav_fragment(&hostile, None);
    assert!(!out.contains("<script>"));
    assert!(out.contains(r#"href="/p/a/zv%C3%BCk""#));
}

#[test]
fn an_empty_topic_invites_the_first_message() {
    assert!(messages_fragment(&[], 0).contains("Nothing here yet"));
}

#[test]
fn the_feed_has_day_dividers_and_groups_close_messages_of_one_sender() {
    let day = 1_794_441_600; // 2026-11-12 00:00 UTC
    let out = messages_fragment(
        &[
            msg(1, "owner", "a", day + 100),
            msg(2, "owner", "b", day + 200),
            msg(3, "audio-1a2b", "c", day + 260),
            msg(4, "audio-1a2b", "d", day + 3000),
            msg(5, "audio-1a2b", "e", day + 86_400 + 5),
        ],
        0,
    );
    assert_eq!(out.matches(r#"class="day""#).count(), 2);
    assert!(out.contains(">12 November 2026<") && out.contains(">13 November 2026<"));
    assert!(
        out.contains(r#"<div class="msg compact" id="m2">"#),
        "same sender within 5 minutes"
    );
    assert!(
        out.contains(r#"<div class="msg" id="m3">"#),
        "another sender"
    );
    assert!(out.contains(r#"<div class="msg" id="m4">"#), "a long pause");
    assert!(
        out.contains(r#"<div class="msg" id="m5">"#),
        "a new day starts a new header"
    );
    assert!(out.contains(r#"<time datetime="2026-11-12T00:01:40Z">00:01</time>"#));
}

#[test]
fn messages_of_other_kinds_or_for_other_receivers_keep_their_own_header() {
    let mut note = msg(1, "browser-1", "a", 100);
    note.kind = Kind::Notice;
    note.target = Some("@audio".into());
    let mut ask = msg(2, "browser-1", "b", 110);
    ask.kind = Kind::Ask;
    ask.target = Some("@audio".into());
    let mut elsewhere = msg(3, "browser-1", "c", 120);
    elsewhere.kind = Kind::Ask;
    elsewhere.target = Some("@kernel".into());
    let mut again = msg(4, "browser-1", "d", 130);
    again.kind = Kind::Ask;
    again.target = Some("@kernel".into());
    let out = messages_fragment(&[note, ask, elsewhere, again], 0);
    assert_eq!(
        out.matches(r#"class="msg compact""#).count(),
        1,
        "only the last one follows its twin"
    );
    assert!(out.contains(r#"<div class="msg compact" id="m4">"#));
}

#[test]
fn an_answer_is_shown_under_its_ask_with_a_quote() {
    let mut ask = msg(57, "dev-5476", "Need: events\nsecond line", 1000);
    ask.kind = Kind::Ask;
    let other = msg(58, "dev-9367", "unrelated", 1100);
    let mut answer = msg(59, "dev-fada", "Yes", 1200);
    answer.kind = Kind::Answer;
    answer.answers = Some(57);
    let out = messages_fragment(&[ask, other, answer], 2000);
    let at = |id: &str| out.find(id).unwrap();
    assert!(at(r#"id="m57""#) < at(r#"id="m59""#), "{out}");
    assert!(at(r#"id="m59""#) < at(r#"id="m58""#), "{out}");
    assert!(
        out.contains(r##"href="#m57">↳ re #57 dev-5476: Need: events</a>"##),
        "{out}"
    );
    assert!(out.contains("msg reply"), "{out}");
}

#[test]
fn an_answer_whose_ask_is_off_the_page_stays_in_place() {
    let mut answer = msg(59, "dev-fada", "Yes", 1200);
    answer.answers = Some(7);
    let out = messages_fragment(&[answer], 2000);
    assert!(
        out.contains(r#"id="m59""#) && out.contains("↳ re #7"),
        "{out}"
    );
}

#[test]
fn messages_show_who_wrote_with_a_stable_avatar() {
    let out = messages_fragment(
        &[msg(1, "owner", "x", 0), msg(2, "audio-1a2b", "y", 1000)],
        0,
    );
    assert!(out.contains("owner") && out.contains(r#"class="avatar av-owner">O<"#));
    assert!(out.contains(r#"class="avatar av"#));
    assert_eq!(avatar_class("audio-1a2b"), avatar_class("audio-1a2b"));
    assert!(avatar_class("kernel-0000").starts_with("av"));
}

#[test]
fn every_message_shows_its_id_as_a_link_to_itself() {
    let a = msg(41, "audio-1a2b", "a", 1000);
    let b = msg(42, "audio-1a2b", "b", 1060);
    let out = messages_fragment(&[a, b], 0);
    let link = |id: i64| {
        format!(
            r##"<a class="mid" href="/p/demo/general#m{id}" title="Link to this message">#{id}</a>"##
        )
    };
    // The first message has a header with its id; the one that follows it has none, so its
    // id is in the margin. Both are links to the message in its own topic.
    assert!(
        out.contains(&format!("</time> {}</div>", link(41))),
        "{out}"
    );
    assert!(
        out.contains(&format!(r#"<div class="gutter">{}</div>"#, link(42))),
        "{out}"
    );
    assert_eq!(
        out.matches("class=\"mid\"").count(),
        4,
        "each message has it twice, the page shows one"
    );
    // The address is escaped like any other.
    let mut odd = msg(7, "x-1", "x", 0);
    odd.project = "a b".into();
    odd.topic = "t/1".into();
    assert!(messages_fragment(&[odd], 0).contains(r##"href="/p/a%20b/t%2F1#m7""##));
}

#[test]
fn the_deadline_of_an_ask_is_shown_in_the_time_zone_of_the_page() {
    use crate::chat::AskInfo;
    // Sent at 06:36 UTC with --by 30m: the text says 07:06 UTC, as agents read it.
    let sent = 1_795_070_160;
    let mut m = msg(
        1,
        "frontend-7069",
        "Need: is it live? Deadline: by 07:06 UTC\nWhy: x\nDeadline: by 07:06 UTC",
        sent,
    );
    m.kind = Kind::Ask;
    m.ask = Some(AskInfo {
        delivery: Some("queued".into()),
        delivered_at: None,
        answered_at: None,
        reply_by: Some(sent + 1800),
    });
    let out = messages_fragment(&[m.clone()], sent);
    // Like the time of the message, the deadline is a <time> the browser localizes.
    assert!(
        out.contains(r#"Deadline: by <time datetime="2026-11-19T07:06:00Z">07:06 UTC</time>"#),
        "{out}"
    );
    // Only the line the service wrote, not the same words inside a field.
    assert!(
        out.contains("Need: is it live? Deadline: by 07:06 UTC\nWhy: x"),
        "{out}"
    );
    assert_eq!(
        out.matches("<time datetime=\"2026-11-19T07:06:00Z\">")
            .count(),
        1
    );
    // The text agents get is unchanged.
    assert!(m.text.ends_with("Deadline: by 07:06 UTC"));
    // A message without a deadline is shown as it was written.
    let plain = msg(2, "a-1", "Deadline: by 07:06 UTC", sent);
    assert!(!messages_fragment(&[plain], sent).contains("<time datetime=\"2026-11-19T07:06"));
}

#[test]
fn an_ask_says_what_became_of_it() {
    use crate::chat::AskInfo;
    let ask = |delivery: Option<&str>, delivered_at, answered_at| {
        let mut m = msg(5, "audio-1a2b", "x", 100);
        m.kind = Kind::Ask;
        m.ask = Some(AskInfo {
            delivery: delivery.map(str::to_string),
            delivered_at,
            answered_at,
            reply_by: Some(2000),
        });
        m
    };
    let page = |m: Message, now| messages_fragment(&[m], now);
    let out = page(ask(Some("queued"), None, None), 1000);
    assert!(
        out.contains(r#"<div class="ask-state s-wait">waiting for delivery</div>"#),
        "{out}"
    );
    let out = page(ask(Some("delivered"), Some(1_794_643_106), None), 1000);
    assert!(
        out.contains(r#"s-sent">delivered at <time datetime="2026-11-14T07:58:26Z">"#),
        "{out}"
    );
    let out = page(
        ask(Some("delivered"), Some(1_794_643_106), Some(1_794_643_400)),
        1000,
    );
    assert!(
        out.contains(r#"s-ok">answered at <time datetime="2026-11-14T08:03:20Z">"#),
        "{out}"
    );
    let out = page(ask(Some("delivered"), Some(1_794_643_106), None), 3000);
    assert!(
        out.contains("s-bad\">delivered, deadline passed with no answer"),
        "{out}"
    );
    let out = page(ask(Some("queued"), None, None), 3000);
    assert!(
        out.contains("s-bad\">not delivered, deadline passed"),
        "{out}"
    );
    let out = page(ask(Some("expired"), None, None), 1000);
    assert!(out.contains("not delivered"), "{out}");
    // An answer after the time still counts as an answer; other messages have no line.
    let out = page(
        ask(Some("delivered"), Some(1_794_643_106), Some(1_794_643_400)),
        99_999_999_999,
    );
    assert!(out.contains("answered"), "{out}");
    assert!(!messages_fragment(&[msg(1, "audio-1a2b", "x", 0)], 0).contains("ask-state"));
}

#[test]
fn only_agent_messages_carry_a_kind_badge() {
    let mut ask = msg(2, "audio-1a2b", "y", 1000);
    ask.kind = Kind::Ask;
    let out = messages_fragment(&[msg(1, "owner", "x", 0), ask], 0);
    assert_eq!(out.matches(r#"class="kind "#).count(), 1);
    assert!(out.contains(r#"<span class="kind k-ask">request</span>"#));
}

#[test]
fn sender_text_and_kind_are_all_escaped() {
    let out = messages_fragment(&[msg(1, "<b>x</b>", "<i>y</i> http://a.test/\"q", 0)], 0);
    for raw in ["<b>", "<i>"] {
        assert!(!out.contains(raw), "{raw} leaked");
    }
    assert!(out.contains("&lt;b&gt;x") && out.contains("&lt;i&gt;y"));
    // The one link of the text; the others are the id of the message.
    assert_eq!(
        out.matches("<a ").count() - out.matches(r#"<a class="mid""#).count(),
        1
    );
}

#[test]
fn the_people_panel_shows_state_and_hides_what_it_does_not_need() {
    let out = people_fragment(
        &[
            session("audio-1111", "demo", 990),
            session("kernel-2222", "demo", 100),
        ],
        1000,
    );
    assert!(out.contains(r#"<li class="active">"#) && out.contains(r#"<li class="idle">"#));
    assert!(out.contains("audio-1111") && out.contains("claude@host-a · audio, desktop"));
    for private in ["secret-ish-id", "t.jsonl", "/home/x"] {
        assert!(!out.contains(private), "{private} is not shown");
    }
    assert!(people_fragment(&[], 0).contains("Nobody"));
}

#[test]
fn the_session_name_is_shown_next_to_the_handle_everywhere() {
    let mut named = session("audio-1111", "demo", 990);
    named.name = Some("Graphics & sound".into());
    let people = people_fragment(&[named.clone()], 1000);
    assert!(
        people.contains(r#"Graphics &amp; sound <span class="handle">audio-1111</span>"#),
        "{people}"
    );
    let board = board_fragment(&[named], &[], &HashMap::new(), 1000);
    assert!(
        board.contains(r#"<td>Graphics &amp; sound <span class="handle">audio-1111</span></td>"#)
    );
    let mut m = msg(1, "audio-1111", "x", 0);
    m.sender_name = Some("<i>Audio</i>".into());
    let feed = messages_fragment(&[m], 0);
    assert!(feed.contains(r#"&lt;i&gt;Audio&lt;/i&gt; <span class="handle">audio-1111</span>"#));
    assert!(!feed.contains("<i>"));
    assert!(feed.contains(r#"<div class="avatar "#) && feed.contains(">&lt;</div>"));
    // Without a name the handle stands alone.
    assert!(
        messages_fragment(&[msg(1, "audio-1111", "x", 0)], 0)
            .contains(r#"<span class="who">audio-1111</span>"#)
    );
}

#[test]
fn the_leases_in_force_are_a_table_above_the_sessions() {
    let lease = |kind, agent: &str, issue| Lease {
        repo: "acme/demo".into(),
        issue,
        title: "Resource: <laptop>".into(),
        kind,
        agent: agent.into(),
        action: Some("reboot & update".into()),
        until: 1794562140,
    };
    let out = board_fragment(
        &[session("a-1", "demo", 0)],
        &[
            lease(LeaseKind::Exclusive, "claude@devbox", 480),
            lease(LeaseKind::Using, "codex@mac", 481),
        ],
        &HashMap::new(),
        5,
    );
    assert!(out.find("Resource leases").unwrap() < out.find("a-1").unwrap());
    assert!(out.contains(
        r#"<a href="https://forge.example.com/acme/demo/issues/480">Resource: &lt;laptop&gt;</a>"#
    ));
    assert!(out.contains("exclusive") && out.contains("shared"));
    assert!(out.contains("reboot &amp; update") && out.contains("09:29"));
    assert!(!out.contains("<laptop>"));
    // With nobody working, the leases are still there.
    let alone = board_fragment(
        &[],
        &[lease(LeaseKind::Exclusive, "x@y", 1)],
        &HashMap::new(),
        5,
    );
    assert!(alone.contains("Resource leases") && alone.contains("Nobody is working"));
    assert!(!board_fragment(&[], &[], &HashMap::new(), 5).contains("Resource leases"));
}

#[test]
fn the_lines_from_the_forge_look_like_the_service_not_like_an_agent() {
    let mut m = msg(1, "forge", "Lease taken: x", 0);
    m.kind = Kind::Notice;
    let out = messages_fragment(&[m], 0);
    assert!(out.contains(r#"<span class="who">Forge</span>"#), "{out}");
    assert!(out.contains("av-system") && !out.contains(r#"class="kind "#));
}

#[test]
fn the_board_names_the_issue_of_a_session_from_its_claim_in_the_forge() {
    let claim = |branch: &str, merged| Claim {
        repo: "acme/core".into(),
        issue: 618,
        title: "board <itself>".into(),
        worktree: Some("/w/claims".into()),
        branch: Some(branch.into()),
        merged,
    };
    let mut by_branch = session("dev-1", "core", 0);
    by_branch.issue = None;
    by_branch.worktree = Some("/elsewhere".into());
    by_branch.branch = Some("claude/claims".into());
    let board = |s: &Session, c: Claim| {
        board_fragment_with(
            std::slice::from_ref(s),
            &[],
            &[c],
            &HashMap::new(),
            &AnswerStats::default(),
            5,
        )
    };
    let out = board(&by_branch, claim("claude/claims", false));
    assert!(
        out.contains(r#"<td><a href="https://forge.example.com/acme/core/issues/618">#618 board &lt;itself&gt;</a></td>"#),
        "{out}"
    );
    assert!(board(&by_branch, claim("claude/claims", true)).contains("</a> · merged</td>"));
    // By the worktree, when the branch says nothing.
    let mut by_tree = by_branch.clone();
    by_tree.branch = Some("main".into());
    by_tree.worktree = Some("/w/claims".into());
    assert!(board(&by_tree, claim("other", false)).contains("#618"));
    // `main` is everybody's branch, and what the session said itself wins.
    let mut on_main = by_branch.clone();
    on_main.branch = Some("main".into());
    assert!(!board(&on_main, claim("main", false)).contains("#618"));
    let mut said = by_branch.clone();
    said.issue = Some("own task".into());
    let out = board(&said, claim("claude/claims", false));
    assert!(out.contains("own task") && !out.contains("#618"));
}

#[test]
fn empty_board_says_so() {
    assert!(board_fragment(&[], &[], &HashMap::new(), 0).contains("Nobody is working"));
}

#[test]
fn board_has_a_table_per_project_linking_to_its_general_topic() {
    let now = 10_000;
    let out = board_fragment(
        &[
            session("audio-1111", "demo", now - 10),
            session("kernel-2222", "demo", now - 700),
            session("ci-3333", "core", now),
        ],
        &[],
        &HashMap::new(),
        now,
    );
    assert_eq!(out.matches("<table>").count(), 2);
    assert!(out.find(">core<").unwrap() < out.find(">demo<").unwrap());
    assert!(out.contains(r#"<h3><a href="/p/demo/general">demo</a></h3>"#));
    assert!(out.contains(r#"<tr class="active"><td>audio-1111</td><td>audio, desktop</td><td>claude@host-a</td><td>#471 svcd</td><td>—</td><td>active</td><td>just now</td>"#));
    assert!(out.contains(r#"<tr class="idle"><td>kernel-2222</td>"#) && out.contains("11 min ago"));
}

#[test]
fn board_escapes_what_sessions_tell_and_hides_what_it_does_not_need() {
    let mut s = session("<b>x</b>", "<p>", 0);
    s.issue = Some("<img src=x onerror=1>".into());
    s.branch = Some("\"><script>".into());
    let out = board_fragment(&[s], &[], &HashMap::new(), 0);
    for raw in ["<b>", "<p>", "<img", "<script>"] {
        assert!(!out.contains(raw), "{raw} leaked");
    }
    for private in ["secret-ish-id", "t.jsonl", "/home/x"] {
        assert!(!out.contains(private), "{private} is not shown");
    }
}

#[test]
fn a_topic_page_has_the_sidebar_the_feed_the_composer_and_the_people() {
    let t = TopicInfo {
        about: Some("sound driver".into()),
        ..topic("audio", 1)
    };
    let out = topic_page(
        &nav(),
        "demo",
        &t,
        &[msg(1, "owner", "hello", 0)],
        &[session("audio-1", "demo", 0)],
        10,
    );
    assert!(out.contains(r#"<aside id="side""#) && out.contains(r#"<div id="nav">"#));
    assert!(out.contains(r#"<div id="messages" class="feed">"#) && out.contains("hello"));
    assert!(out.contains(r#"<form class="composer" method="post" action="/p/demo/audio/post">"#));
    assert!(out.contains(r#"<aside id="people""#) && out.contains("audio-1"));
    assert!(out.contains(r#"data-project="demo" data-topic="audio""#));
    assert!(out.contains("<h1># audio</h1>") && out.contains("sound driver"));
    assert!(!out.contains(r#"class="board-link current""#));
}

#[test]
fn a_hostile_topic_name_cannot_inject_into_the_page() {
    let t = topic("<script>x</script>", 0);
    let out = topic_page(&nav(), "a\"><script>", &t, &[], &[], 0);
    assert!(!out.contains("<script>x"), "{out}");
    assert!(!out.contains("a\"><script>"));
}

#[test]
fn the_board_shows_the_noise_numbers_of_each_project() {
    let counters = HashMap::from([(
        "demo".to_string(),
        Counters {
            sent: 5,
            asks: 3,
            answered: 2,
            rejected: 1,
            wakes: 2,
            merged: 20,
        },
    )]);
    let out = board_fragment(
        &[session("a-1", "demo", 0), session("b-2", "core", 0)],
        &[],
        &counters,
        5,
    );
    assert!(out.contains(
        "Last 24 hours: messages 5 · requests 3 (answered 2) · rejected 1 · wakes 2 · PRs merged 20 · messages per PR 0.3"
    ));
    assert!(
        out.contains("rejected 0 · wakes 0 · PRs merged 0</p>"),
        "no ratio without a merged pull request"
    );
}

#[test]
fn durations_are_written_for_the_owner() {
    assert_eq!(format_duration(0), "0 s");
    assert_eq!(format_duration(45), "45 s");
    assert_eq!(format_duration(60), "1 min");
    assert_eq!(format_duration(125), "2 min 5 s");
    assert_eq!(format_duration(3600), "1 h");
    assert_eq!(format_duration(4200), "1 h 10 min");
}

#[test]
fn the_board_shows_how_the_questions_of_the_day_were_answered() {
    let stats = AnswerStats {
        asks: 8,
        answered: 6,
        median_secs: Some(95),
        p90_secs: Some(610),
    };
    let out = board_fragment_with(
        &[session("a-1", "demo", 0)],
        &[],
        &[],
        &HashMap::new(),
        &stats,
        5,
    );
    assert!(
        out.contains("Requests in the last 24 hours: 8 · answered 75% (6 of 8) · time to answer: median 1 min 35 s, 90% within 10 min 10 s"),
        "{out}"
    );
    // Nothing asked, nothing answered: said plainly, no division by zero.
    assert!(answers_fragment(&AnswerStats::default()).contains("none"));
    let unanswered = AnswerStats {
        asks: 2,
        ..AnswerStats::default()
    };
    let text = answers_fragment(&unanswered);
    assert!(
        text.contains("answered 0%") && text.contains("no answers yet"),
        "{text}"
    );
    // The numbers are on the board even when nobody works at the moment.
    let empty = board_fragment_with(&[], &[], &[], &HashMap::new(), &stats, 5);
    assert!(empty.contains("answered 75%") && empty.contains("Nobody is working right now"));
}

#[test]
fn a_message_shows_who_it_is_for() {
    let mut to_role = msg(1, "audio-1a2b", "x", 0);
    to_role.target = Some("@browser".into());
    let mut to_all = msg(2, "kernel-1", "y", 1000);
    to_all.target = Some("all".into());
    let mut to_owner = msg(3, "kernel-1", "z", 2000);
    to_owner.target = Some("owner".into());
    let mut hostile = msg(4, "kernel-1", "w", 3000);
    hostile.target = Some("<script>".into());
    let out = messages_fragment(
        &[
            to_role,
            to_all,
            to_owner,
            hostile,
            msg(5, "owner", "v", 4000),
        ],
        0,
    );
    assert!(out.contains(r#"<span class="to">→ @browser</span>"#));
    assert!(out.contains("→ everyone") && out.contains("→ the owner"));
    assert!(!out.contains("<script>") && out.contains("→ &lt;script&gt;"));
    assert_eq!(
        out.matches(r#"class="to""#).count(),
        4,
        "the owner's own message has no target"
    );
}

#[test]
fn the_board_page_marks_itself_current_and_has_no_composer() {
    let out = board_page(
        &nav(),
        &[session("audio-1", "demo", 0)],
        &[],
        &[],
        &HashMap::new(),
        &AnswerStats::default(),
        5,
    );
    assert!(out.contains(r#"class="board-link current""#) && out.contains(" data-board"));
    assert!(!out.contains("composer") && out.contains("audio-1"));
}

#[test]
fn pages_say_they_are_english() {
    let out = error_page(&nav(), "Error", "x", "/");
    assert!(out.contains(r#"<html lang="en">"#), "{out}");
}

#[test]
fn error_page_escapes_reason() {
    let out = error_page(&nav(), "Error", "<b>", "/p/all/general");
    assert!(!out.contains("<b>") && out.contains("&lt;b&gt;"));
}
