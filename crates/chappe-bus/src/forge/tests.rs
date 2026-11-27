use serde_json::{Value, json};

use super::*;

/// 2026-11-13 08:29 UTC.
const T0: i64 = 1_794_558_540;
const DAY: i64 = 86_400;

fn store() -> Store {
    let s = Store::open_in_memory().unwrap();
    s.ensure_projects(&["all".into(), "demo".into(), "core".into()])
        .unwrap();
    s
}

fn lines(s: &Store, project: &str) -> Vec<String> {
    s.recent_messages(project, EVENTS, 50)
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect()
}

fn comment(action: &str, id: i64, body: &str) -> Value {
    json!({
        "action": action,
        "comment": {"id": id, "body": body},
        "issue": {"number": 480, "title": "Resource: laptop with Studio OS"},
        "repository": {"full_name": "acme/demo"},
    })
}

const EXCLUSIVE: &str = "<!-- resource-lease: exclusive -->\nagent: claude@host-a\nsession: Claude Code, Graphics and sound\naction: install the package and reboot\nauthorized: owner\nstarts: 2026-11-13 08:29 UTC\nuntil: 2026-11-13 09:29 UTC";
const RELEASED: &str = "<!-- resource-lease: released -->\nagent: claude@host-a\nresult: the laptop came back on the new build.";

#[test]
fn the_signature_is_an_hmac_sha256() {
    // RFC 4231, test cases 1 and 2.
    let key = [0x0bu8; 20];
    assert_eq!(
        hex(&hmac_sha256(&key, b"Hi There")),
        "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
    assert_eq!(
        hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
        "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    );
    // A key longer than a block is hashed first (RFC 4231, test case 6).
    assert_eq!(
        hex(&hmac_sha256(
            &[0xaau8; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
}

#[test]
fn only_the_right_signature_matches() {
    let sig = hex(&hmac_sha256(b"secret-0123456789", b"{}"));
    assert!(signature_matches("secret-0123456789", b"{}", &sig));
    assert!(signature_matches(
        "secret-0123456789",
        b"{}",
        &format!("sha256={sig}")
    ));
    assert!(
        !signature_matches("secret-0123456789", b"{ }", &sig),
        "another body"
    );
    assert!(
        !signature_matches("another-secret-000", b"{}", &sig),
        "another secret"
    );
    for bad in ["", "sha256=", &sig[..63], &format!("{sig}0")] {
        assert!(
            !signature_matches("secret-0123456789", b"{}", bad),
            "{bad:?}"
        );
    }
}

#[test]
fn a_lease_record_is_read_from_the_comment() {
    let r = parse_lease(EXCLUSIVE).unwrap();
    assert_eq!(r.kind, LeaseKind::Exclusive);
    assert_eq!(r.agent, "claude@host-a");
    assert_eq!(r.action.as_deref(), Some("install the package and reboot"));
    assert_eq!((r.starts, r.until), (Some(T0), Some(T0 + 3600)));
    let r = parse_lease(RELEASED).unwrap();
    assert_eq!(r.kind, LeaseKind::Released);
    assert_eq!(
        r.result.as_deref(),
        Some("the laptop came back on the new build.")
    );
    let using = parse_lease(
        "<!-- resource-lease: using -->\r\nagent: codex@mac\r\nuntil: 2026-11-13 09:29 UTC\r\n",
    )
    .unwrap();
    assert_eq!(using.kind, LeaseKind::Using);
    assert_eq!(using.until, Some(T0 + 3600));
    assert_eq!(using.action, None);
}

#[test]
fn what_is_not_a_lease_record_is_not_read_as_one() {
    for body in [
        "",
        "just a comment",
        "agent: claude@host-a\n<!-- resource-lease: exclusive -->",
        "<!-- resource-lease: borrowed -->\nagent: x@y",
        "<!-- resource-lease: exclusive -->\nno agent here",
        "<!-- resource-lease: exclusive\nagent: x@y",
    ] {
        assert_eq!(parse_lease(body), None, "{body:?}");
    }
    // Leading blank lines or a byte order mark do not matter; a marker deeper in the text does.
    assert!(parse_lease("\n\u{feff}<!-- resource-lease: using -->\nagent: x@y").is_some());
    assert_eq!(
        parse_lease("see:\n<!-- resource-lease: using -->\nagent: x@y"),
        None
    );
}

#[test]
fn times_are_read_as_utc() {
    assert_eq!(parse_utc("2026-11-13 08:29 UTC"), Some(T0));
    assert_eq!(parse_utc("2026-11-13T08:29:00Z"), Some(T0));
    assert_eq!(parse_utc("2026-11-12 22:55 UTC"), Some(1_794_524_100));
    assert_eq!(parse_utc("1970-01-01 00:00 UTC"), Some(0));
    for bad in [
        "",
        "soon",
        "2026-13-01 00:00",
        "2026-11-13 25:00",
        "2026-11-13",
        "1969-12-31 23:59",
    ] {
        assert_eq!(parse_utc(bad), None, "{bad:?}");
    }
}

#[test]
fn a_project_is_named_after_its_repository() {
    assert_eq!(project_of("acme/demo").as_deref(), Some("demo"));
    assert_eq!(
        project_of("owner/Shop_Frontend").as_deref(),
        Some("shop-frontend")
    );
    assert_eq!(project_of("core").as_deref(), Some("core"));
    assert_eq!(project_of("owner/").as_deref(), None);
    assert_eq!(project_of("").as_deref(), None);
}

#[test]
fn an_exclusive_lease_shows_once_in_its_project_and_in_all_and_its_release_once_more() {
    let s = store();
    let refresh = s
        .gitea_event("issue_comment", "d1", &comment("created", 1, EXCLUSIVE), T0)
        .unwrap();
    assert!(refresh.contains(&"msg:demo/events".to_string()));
    assert!(
        refresh.contains(&"msg:all/events".to_string()) && refresh.contains(&"board".to_string())
    );
    for project in ["demo", "all"] {
        let l = lines(&s, project);
        assert_eq!(l.len(), 1, "{project}: {l:?}");
        assert!(
            l[0].starts_with("Lease taken: Resource: laptop with Studio OS (acme/demo#480).")
                && l[0].contains(
                    "claude@host-a is changing the resource: install the package and reboot"
                )
                && l[0].contains("Until 09:29 UTC")
                && l[0].contains("https://forge.example.com/acme/demo/issues/480"),
            "{}",
            l[0]
        );
    }
    let in_force = s.leases(T0 + 60).unwrap();
    assert_eq!(in_force.len(), 1);
    assert_eq!(
        (in_force[0].kind, in_force[0].issue, in_force[0].until),
        (LeaseKind::Exclusive, 480, T0 + 3600)
    );

    // Gitea redelivers, and the agent edits the comment: neither is announced again.
    assert!(
        s.gitea_event(
            "issue_comment",
            "d1",
            &comment("created", 1, EXCLUSIVE),
            T0 + 5
        )
        .unwrap()
        .is_empty()
    );
    let edited = EXCLUSIVE.replace("reboot", "reboot twice");
    s.gitea_event(
        "issue_comment",
        "d2",
        &comment("edited", 1, &edited),
        T0 + 10,
    )
    .unwrap();
    assert_eq!(lines(&s, "demo").len(), 1);
    assert!(
        s.leases(T0 + 60).unwrap()[0]
            .action
            .as_deref()
            .unwrap()
            .ends_with("twice")
    );

    // The release: once in the project, once in all, and the lease is gone.
    s.gitea_event(
        "issue_comment",
        "d3",
        &comment("created", 2, RELEASED),
        T0 + 300,
    )
    .unwrap();
    for project in ["demo", "all"] {
        let l = lines(&s, project);
        assert_eq!(l.len(), 2, "{project}: {l:?}");
        assert!(
            l[1].starts_with("Lease released:") && l[1].contains("Result: the laptop came back"),
            "{}",
            l[1]
        );
    }
    assert!(s.leases(T0 + 301).unwrap().is_empty());
}

#[test]
fn a_using_lease_and_a_plain_release_stay_in_their_project() {
    let s = store();
    let using = "<!-- resource-lease: using -->\nagent: codex@mac\naction: a long measurement\nuntil: 2026-11-13 09:29 UTC";
    s.gitea_event("issue_comment", "d1", &comment("created", 1, using), T0)
        .unwrap();
    assert_eq!(lines(&s, "demo").len(), 1);
    assert!(lines(&s, "demo")[0].starts_with("Resource in use:"));
    assert!(
        lines(&s, "all").is_empty(),
        "only an exclusive lease goes to all"
    );
    assert_eq!(s.leases(T0 + 1).unwrap()[0].kind, LeaseKind::Using);
    // Another agent's release does not end this lease.
    s.gitea_event(
        "issue_comment",
        "d2",
        &comment("created", 2, RELEASED),
        T0 + 5,
    )
    .unwrap();
    assert_eq!(s.leases(T0 + 6).unwrap().len(), 1);
    assert!(
        lines(&s, "all").is_empty(),
        "a release that follows no exclusive lease"
    );
    // The agent's own release does.
    let own = "<!-- resource-lease: released -->\nagent: codex@mac\nresult: done";
    s.gitea_event("issue_comment", "d3", &comment("created", 3, own), T0 + 9)
        .unwrap();
    assert!(s.leases(T0 + 10).unwrap().is_empty());
    assert!(lines(&s, "all").is_empty());
}

#[test]
fn a_lease_past_its_time_is_not_in_force() {
    let s = store();
    s.gitea_event("issue_comment", "d1", &comment("created", 1, EXCLUSIVE), T0)
        .unwrap();
    assert_eq!(s.leases(T0 + 3599).unwrap().len(), 1);
    assert!(s.leases(T0 + 3600).unwrap().is_empty());
    // Without `until` a lease holds two hours from its start.
    let open = "<!-- resource-lease: using -->\nagent: x@y\nstarts: 2026-11-13 08:29 UTC";
    s.gitea_event("issue_comment", "d2", &comment("created", 2, open), T0)
        .unwrap();
    assert_eq!(s.leases(T0 + 7199).unwrap().len(), 1);
    assert!(s.leases(T0 + 7200).unwrap().is_empty());
}

#[test]
fn a_deleted_record_is_forgotten() {
    let s = store();
    s.gitea_event("issue_comment", "d1", &comment("created", 1, EXCLUSIVE), T0)
        .unwrap();
    s.gitea_event("issue_comment", "d2", &comment("deleted", 1, ""), T0 + 5)
        .unwrap();
    assert!(s.leases(T0 + 6).unwrap().is_empty());
}

#[test]
fn merged_pull_requests_and_claims_are_counted_not_posted_and_summed_up_once_a_day() {
    let s = store();
    let pr = |number: i64, merged: bool| {
        json!({
            "action": "closed",
            "pull_request": {"number": number, "merged": merged},
            "repository": {"full_name": "acme/core"},
        })
    };
    let claim = |id: i64| {
        json!({
            "action": "created",
            "comment": {"id": id, "body": "<!-- agent-lock: claimed -->\nzanyato"},
            "issue": {"number": 9, "title": "x"},
            "repository": {"full_name": "acme/core"},
        })
    };
    let day = T0 - T0 % DAY;
    s.gitea_event("pull_request", "p1", &pr(571, true), day + 100)
        .unwrap();
    s.gitea_event("pull_request", "p2", &pr(572, true), day + 200)
        .unwrap();
    s.gitea_event("pull_request", "p3", &pr(573, false), day + 300)
        .unwrap();
    s.gitea_event("pull_request", "p1", &pr(571, true), day + 400)
        .unwrap();
    s.gitea_event("issue_comment", "c1", &claim(10), day + 500)
        .unwrap();
    s.gitea_event("issue_comment", "c2", &claim(11), day + 600)
        .unwrap();
    assert!(lines(&s, "core").is_empty(), "nothing is posted one by one");

    // The board counts them.
    let counts = crate::forge::activity_counts(&s.conn(), day + 700).unwrap();
    assert_eq!(counts["core"].0, 2);

    // Not before the day is over.
    assert!(s.post_digests(day + DAY - 1).unwrap().is_empty());
    let refresh = s.post_digests(day + DAY + 60).unwrap();
    assert_eq!(refresh, ["msg:core/events"]);
    let l = lines(&s, "core");
    assert_eq!(l.len(), 1);
    assert!(
        l[0].starts_with(
            "Summary for 13 November 2026: PRs merged — 2 (#571, #572); issues claimed — 2."
        ),
        "{}",
        l[0]
    );
    // Once.
    assert!(s.post_digests(day + DAY + 600).unwrap().is_empty());
    assert_eq!(lines(&s, "core").len(), 1);
    // A day without activity gets none.
    assert!(s.post_digests(day + 5 * DAY).unwrap().is_empty());
}

#[test]
fn a_long_list_of_pull_requests_is_cut_in_the_digest() {
    let s = store();
    let day = T0 - T0 % DAY;
    for n in 1..=12 {
        let pr = json!({"action": "closed", "pull_request": {"number": n, "merged": true},
                        "repository": {"full_name": "acme/core"}});
        s.gitea_event("pull_request", &format!("p{n}"), &pr, day + n)
            .unwrap();
    }
    s.post_digests(day + DAY + 1).unwrap();
    let l = lines(&s, "core");
    assert!(
        l[0].contains("PRs merged — 12 (#1, #2, #3, #4, #5, #6, #7, #8 and 4 more)"),
        "{}",
        l[0]
    );
}

#[test]
fn a_published_release_is_a_line_and_a_draft_is_not() {
    let s = store();
    let release = |action: &str, draft: bool| {
        json!({"action": action, "release": {"tag_name": "v0.1.3", "draft": draft,
               "html_url": "https://forge.example.com/acme/demo/releases/tag/v0.1.3"},
               "repository": {"full_name": "acme/demo"}})
    };
    s.gitea_event("release", "r1", &release("created", true), T0)
        .unwrap();
    s.gitea_event("release", "r2", &release("published", true), T0)
        .unwrap();
    assert!(lines(&s, "demo").is_empty());
    s.gitea_event("release", "r3", &release("published", false), T0)
        .unwrap();
    assert_eq!(
        lines(&s, "demo"),
        [
            "Release published: acme/demo v0.1.3. https://forge.example.com/acme/demo/releases/tag/v0.1.3"
        ]
    );
}

fn run(branch: &str, conclusion: &str, workflow: &str) -> Value {
    json!({
        "action": "completed",
        "workflow_run": {"head_branch": branch, "status": "completed", "conclusion": conclusion,
                         "path": workflow, "display_title": "fix the thing",
                         "html_url": "https://forge.example.com/acme/demo/actions/runs/7"},
        "repository": {"full_name": "acme/demo", "default_branch": "main"},
    })
}

#[test]
fn main_turning_red_and_green_again_is_two_lines() {
    let s = store();
    let ci = ".gitea/workflows/ci.yaml";
    s.gitea_event("workflow_run", "w1", &run("main", "success", ci), T0)
        .unwrap();
    assert!(lines(&s, "demo").is_empty(), "green stays quiet");
    s.gitea_event("workflow_run", "w2", &run("main", "failure", ci), T0 + 10)
        .unwrap();
    s.gitea_event("workflow_run", "w3", &run("main", "failure", ci), T0 + 20)
        .unwrap();
    let l = lines(&s, "demo");
    assert_eq!(l.len(), 1, "red once: {l:?}");
    assert!(
        l[0].starts_with("main turned red: acme/demo, fix the thing."),
        "{}",
        l[0]
    );
    // A branch other than main and a cancelled run say nothing about main.
    s.gitea_event(
        "workflow_run",
        "w4",
        &run("feature", "success", ci),
        T0 + 30,
    )
    .unwrap();
    s.gitea_event("workflow_run", "w5", &run("main", "cancelled", ci), T0 + 40)
        .unwrap();
    assert_eq!(lines(&s, "demo").len(), 1);
    s.gitea_event("workflow_run", "w6", &run("main", "success", ci), T0 + 50)
        .unwrap();
    s.gitea_event("workflow_run", "w7", &run("main", "success", ci), T0 + 60)
        .unwrap();
    let l = lines(&s, "demo");
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(
        l[1].starts_with("main is green again: acme/demo."),
        "{}",
        l[1]
    );
}

#[test]
fn main_is_red_while_any_workflow_of_it_is() {
    let s = store();
    let (a, b) = ("a.yaml", "b.yaml");
    s.gitea_event("workflow_run", "1", &run("main", "failure", a), T0)
        .unwrap();
    s.gitea_event("workflow_run", "2", &run("main", "failure", b), T0 + 1)
        .unwrap();
    s.gitea_event("workflow_run", "3", &run("main", "success", a), T0 + 2)
        .unwrap();
    assert_eq!(lines(&s, "demo").len(), 1, "still red: b has not recovered");
    s.gitea_event("workflow_run", "4", &run("main", "success", b), T0 + 3)
        .unwrap();
    assert_eq!(lines(&s, "demo").len(), 2);
}

#[test]
fn unknown_events_and_repositories_change_nothing() {
    let s = store();
    assert!(
        s.gitea_event(
            "push",
            "x1",
            &json!({"repository": {"full_name": "acme/demo"}}),
            T0
        )
        .unwrap()
        .is_empty()
    );
    assert!(
        s.gitea_event("issue_comment", "x2", &json!({}), T0)
            .unwrap()
            .is_empty()
    );
    assert!(lines(&s, "demo").is_empty());
}

#[test]
fn a_project_that_was_not_there_gets_its_events_topic() {
    let s = Store::open_in_memory().unwrap();
    s.gitea_event("issue_comment", "d1", &comment("created", 1, EXCLUSIVE), T0)
        .unwrap();
    let nav = s.nav().unwrap();
    let studio = nav.iter().find(|p| p.name == "demo").unwrap();
    assert!(studio.topics.iter().any(|t| t.name == EVENTS));
    assert_eq!(studio.topics[0].name, "general", "general stays first");
}

#[test]
fn the_lines_are_not_agent_messages() {
    let s = store();
    s.gitea_event("issue_comment", "d1", &comment("created", 1, EXCLUSIVE), T0)
        .unwrap();
    let m = &s.recent_messages("demo", EVENTS, 10).unwrap()[0];
    assert_eq!(m.sender, "forge");
    assert!(m.target.is_none());
    // No delivery, no noise number, no wake.
    let c = |sql: &str| -> i64 { s.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(c("SELECT COUNT(*) FROM deliveries"), 0);
    assert_eq!(c("SELECT COUNT(*) FROM wakes"), 0);
    assert!(s.counters(T0 + 1).unwrap().is_empty());
}

#[test]
fn a_line_is_one_short_line_of_plain_text() {
    let hostile = format!("a\u{7}b\nc\t<script>{}", "x".repeat(900));
    let l = line(&hostile);
    assert!(!l.contains('\n') && !l.contains('\u{7}') && l.chars().count() <= MAX_LINE_CHARS);
    assert!(l.starts_with("a b c <script>"), "{l}");
    assert!(l.ends_with('…'));
}

#[test]
fn old_records_are_forgotten() {
    let s = store();
    s.gitea_event("issue_comment", "d1", &comment("created", 1, EXCLUSIVE), T0)
        .unwrap();
    let later = T0 + 40 * DAY;
    s.gitea_event(
        "push",
        "d2",
        &json!({"repository": {"full_name": "acme/demo"}}),
        later,
    )
    .unwrap();
    let c = |sql: &str| -> i64 { s.conn().query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(c("SELECT COUNT(*) FROM leases"), 0);
    assert_eq!(
        c("SELECT COUNT(*) FROM forge_deliveries"),
        1,
        "only the new delivery"
    );
}

const CLAIM: &str = "<!-- agent-lock: claimed -->\n**Taken.**\n\n- agent: `claude@devbox`\n- worktree: `/work/worktrees/core-claude-x`\n- branch: `claude/x`\n- time (UTC): 2026-11-14T07:16Z";

#[test]
fn a_claim_record_is_read_from_the_comment_in_either_language() {
    let c = parse_claim(CLAIM).unwrap();
    assert!(!c.released);
    assert_eq!(c.agent.as_deref(), Some("claude@devbox"));
    assert_eq!(c.worktree.as_deref(), Some("/work/worktrees/core-claude-x"));
    assert_eq!(c.branch.as_deref(), Some("claude/x"));
    let en = parse_claim(
        "<!-- agent-lock: claimed -->\nagent: codex@mac\nBranch: fix/y\nWorktree: /w/y\n",
    )
    .unwrap();
    assert_eq!(
        (en.branch.as_deref(), en.worktree.as_deref()),
        (Some("fix/y"), Some("/w/y"))
    );
    let rel = parse_claim("<!-- agent-lock: released -->\nreason: done").unwrap();
    assert!(rel.released && rel.branch.is_none());
    assert_eq!(parse_claim("claimed, honest"), None);
    assert_eq!(parse_claim("text\n<!-- agent-lock: claimed -->"), None);
}

fn claim_comment(action: &str, id: i64, body: &str) -> Value {
    json!({
        "action": action,
        "comment": {"id": id, "body": body},
        "issue": {"number": 618, "title": "the board shows the task itself"},
        "repository": {"full_name": "acme/core"},
    })
}

#[test]
fn a_claim_names_the_issue_of_its_branch_until_it_is_released() {
    let s = store();
    s.gitea_event(
        "issue_comment",
        "c1",
        &claim_comment("created", 1, CLAIM),
        T0,
    )
    .unwrap();
    let claims = s.claims().unwrap();
    let found = claim_of(Some("claude/x"), None, &claims).unwrap();
    assert_eq!((found.issue, found.merged), (618, false));
    assert_eq!(found.title, "the board shows the task itself");
    assert!(claim_of(None, Some("/work/worktrees/core-claude-x"), &claims).is_some());
    assert!(claim_of(Some("claude/other"), Some("/elsewhere"), &claims).is_none());
    assert!(claim_of(None, None, &claims).is_none());
    assert!(lines(&s, "core").is_empty(), "a claim is not posted");
    // Released by another agent: still held. By its own agent: gone.
    let other = "<!-- agent-lock: released -->\nagent: `codex@mac`";
    s.gitea_event(
        "issue_comment",
        "c2",
        &claim_comment("created", 2, other),
        T0 + 1,
    )
    .unwrap();
    assert_eq!(s.claims().unwrap().len(), 1);
    let own = "<!-- agent-lock: released -->\nagent: `claude@devbox`\nreason: stop";
    s.gitea_event(
        "issue_comment",
        "c3",
        &claim_comment("created", 3, own),
        T0 + 2,
    )
    .unwrap();
    assert!(s.claims().unwrap().is_empty());
}

#[test]
fn a_merged_pull_request_marks_the_claim_of_its_branch() {
    let s = store();
    s.gitea_event(
        "issue_comment",
        "c1",
        &claim_comment("created", 1, CLAIM),
        T0,
    )
    .unwrap();
    let pr = |branch: &str, merged: bool| {
        json!({"action": "closed", "pull_request": {"number": 9, "merged": merged, "head": {"ref": branch}},
               "repository": {"full_name": "acme/core"}})
    };
    s.gitea_event("pull_request", "p1", &pr("claude/x", false), T0 + 1)
        .unwrap();
    s.gitea_event("pull_request", "p2", &pr("claude/other", true), T0 + 2)
        .unwrap();
    assert!(!s.claims().unwrap()[0].merged);
    let refresh = s
        .gitea_event("pull_request", "p3", &pr("claude/x", true), T0 + 3)
        .unwrap();
    assert_eq!(refresh, ["board"]);
    assert!(s.claims().unwrap()[0].merged);
    // The same branch claimed again later is a new claim, and the latest one counts.
    s.gitea_event(
        "issue_comment",
        "c4",
        &claim_comment("created", 4, CLAIM),
        T0 + 10,
    )
    .unwrap();
    let claims = s.claims().unwrap();
    assert!(!claim_of(Some("claude/x"), None, &claims).unwrap().merged);
}

#[test]
fn a_deleted_or_edited_claim_follows_the_comment() {
    let s = store();
    s.gitea_event(
        "issue_comment",
        "c1",
        &claim_comment("created", 1, CLAIM),
        T0,
    )
    .unwrap();
    let edited = CLAIM.replace("claude/x", "claude/y");
    s.gitea_event(
        "issue_comment",
        "c2",
        &claim_comment("edited", 1, &edited),
        T0 + 1,
    )
    .unwrap();
    let claims = s.claims().unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].branch.as_deref(), Some("claude/y"));
    s.gitea_event(
        "issue_comment",
        "c3",
        &claim_comment("deleted", 1, ""),
        T0 + 2,
    )
    .unwrap();
    assert!(s.claims().unwrap().is_empty());
}
