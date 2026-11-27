//! The owner's page, as plain HTML strings: a sidebar of projects and their
//! topics, the feed of the open topic with a composer, and who works in the
//! project. The layout is that of Slack or Mattermost.
//!
//! Every piece of stored text goes through [`escape`] on its way out; there is
//! no other path from a message to the page. Links are the only markup.

use std::fmt::Write;

use crate::chat::{AskState, Message, ProjectNav, TopicInfo};
use crate::message::{
    FORGE, GENERAL, MAX_TEXT_CHARS, MAX_TOPIC_CHARS, OWNER, day_of, format_date, format_hm, iso_utc,
};
use std::collections::HashMap;

use crate::forge::{Claim, Lease, LeaseKind, claim_of};
use crate::messaging::{AnswerStats, Counters};
use crate::registry::Session;

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A string as one segment of a URL path (`zvük` becomes `zv%C3%BCk`).
pub fn path_escape(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' {
            out.push(b as char);
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

const TRAILING_PUNCTUATION: &[char] = &['.', ',', ';', ':', '!', '?', ')', ']', '\'', '"', '»'];

/// Escapes plain text and turns `http://` and `https://` addresses into links.
pub fn linkify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((pos, scheme)) = next_scheme(rest) {
        out.push_str(&escape(&rest[..pos]));
        let tail = &rest[pos..];
        let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
        let url = tail[..end].trim_end_matches(TRAILING_PUNCTUATION);
        if url.len() > scheme.len() {
            let shown = escape(url);
            let _ = write!(
                out,
                r#"<a href="{shown}" rel="noopener noreferrer nofollow" target="_blank">{shown}</a>"#
            );
            rest = &tail[url.len()..];
        } else {
            out.push_str(&escape(scheme));
            rest = &tail[scheme.len()..];
        }
    }
    out.push_str(&escape(rest));
    out
}

fn next_scheme(s: &str) -> Option<(usize, &'static str)> {
    ["https://", "http://"]
        .into_iter()
        .filter_map(|scheme| s.find(scheme).map(|pos| (pos, scheme)))
        .min_by_key(|(pos, _)| *pos)
}

/// "just now", "5 min ago", "3 h ago" for an age in seconds.
pub fn format_ago(secs: i64) -> String {
    match secs.max(0) {
        0..=59 => "just now".to_string(),
        s @ 60..=3599 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

fn sender_label(sender: &str) -> String {
    if sender == OWNER {
        "owner".to_string()
    } else if sender == FORGE {
        "Forge".to_string()
    } else {
        escape(sender)
    }
}

/// The session's name as the tool shows it, with the handle next to it in
/// small type; the handle alone when the session has no name.
fn session_label(handle: &str, name: Option<&str>) -> String {
    match name {
        Some(name) => format!(
            r#"{} <span class="handle">{}</span>"#,
            escape(name),
            escape(handle)
        ),
        None => escape(handle),
    }
}

/// The letter in a sender's round avatar.
fn initial(sender: &str) -> String {
    let name = if sender == OWNER { "o" } else { sender };
    name.chars()
        .next()
        .map_or("?".into(), |c| c.to_uppercase().to_string())
}

/// One of twelve colours, the same for the same sender.
fn avatar_class(sender: &str) -> String {
    if sender == OWNER {
        return "av-owner".into();
    }
    if sender == FORGE {
        return "av-system".into();
    }
    let sum: u32 = sender.bytes().map(u32::from).sum();
    format!("av{}", sum % 12)
}

/// The topics of the projects, for the sidebar. `current` is the open topic.
pub fn nav_fragment(nav: &[ProjectNav], current: Option<(&str, &str)>) -> String {
    let mut out = String::new();
    for project in nav {
        let _ = write!(
            out,
            r#"<details class="proj" open><summary>{}</summary><ul>"#,
            escape(&project.name)
        );
        for topic in &project.topics {
            let is_current = current == Some((project.name.as_str(), topic.name.as_str()));
            let count = if topic.messages > 0 {
                format!(r#"<span class="count">{}</span>"#, topic.messages)
            } else {
                String::new()
            };
            let _ = write!(
                out,
                r##"<li><a href="/p/{p}/{t}" class="topic{cur}" title="{about}"><span class="hash">#</span> {name}{count}</a></li>"##,
                p = path_escape(&project.name),
                t = path_escape(&topic.name),
                cur = if is_current { " current" } else { "" },
                about = escape(topic.about.as_deref().unwrap_or("")),
                name = escape(&topic.name),
            );
        }
        let _ = write!(
            out,
            r#"</ul><details class="new-topic"><summary>+ New topic</summary><form method="post" action="/p/{p}/topics"><input name="name" placeholder="topic name" maxlength="{MAX_TOPIC_CHARS}" required autocomplete="off"><button type="submit">Create</button></form></details></details>"#,
            p = path_escape(&project.name),
        );
    }
    out
}

/// "→ @audio": who the message is for.
fn target_label(m: &Message) -> String {
    match m.target.as_deref() {
        None => String::new(),
        Some("all") => r#" <span class="to">→ everyone</span>"#.to_string(),
        Some("owner") => r#" <span class="to">→ the owner</span>"#.to_string(),
        Some(t) => format!(r#" <span class="to">→ {}</span>"#, escape(t)),
    }
}

/// What kind of message an agent's message is. The owner's own messages are
/// just messages and carry no badge.
fn kind_badge(m: &Message) -> String {
    if m.sender == OWNER || m.sender == FORGE {
        return String::new();
    }
    format!(
        r#" <span class="kind k-{}">{}</span>"#,
        m.kind.as_str(),
        m.kind.label()
    )
}

/// What became of an ask, under its text: waiting, delivered, answered, not
/// delivered, or out of time. Other messages have none.
fn ask_state_line(m: &Message, now: i64) -> String {
    let Some(info) = &m.ask else {
        return String::new();
    };
    let at = |label: &str, secs: i64| {
        format!(
            r#"{label} <time datetime="{}">{}</time>"#,
            iso_utc(secs),
            format_hm(secs)
        )
    };
    let (class, text) = match info.state(now) {
        AskState::Queued => ("wait", "waiting for delivery".to_string()),
        AskState::Delivered(secs) => ("sent", at("delivered at", secs)),
        AskState::Answered(secs) => ("ok", at("answered at", secs)),
        AskState::NotDelivered => ("bad", "not delivered, deadline passed".to_string()),
        AskState::Expired => (
            "bad",
            "delivered, deadline passed with no answer".to_string(),
        ),
    };
    format!(r#"<div class="ask-state s-{class}">{text}</div>"#)
}

/// The first line of a message, cut short, for quoting it under an answer.
fn quote(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(90).collect();
    if line.chars().count() > 90 {
        out.push('…');
    }
    out
}

/// One message of the feed. `follows` drops the header of a message that
/// continues the previous one; `parent` is the ask this message answers when
/// that ask is on the same page, and then the answer sits under it with a quote.
fn message_html(m: &Message, now: i64, follows: bool, parent: Option<&Message>) -> String {
    // The id is a link to the message itself: the address of the topic and
    // the anchor of the message. A message that follows another has no
    // header, so its id sits in the margin.
    let link = format!(
        r#"<a class="mid" href="/p/{p}/{t}#m{id}" title="Link to this message">#{id}</a>"#,
        p = path_escape(&m.project),
        t = path_escape(&m.topic),
        id = m.id,
    );
    let quoted = match (parent, m.answers) {
        (Some(q), _) => format!(
            r##"<a class="quote" href="#m{id}">↳ re #{id} {who}: {text}</a>"##,
            id = q.id,
            who = escape(q.sender_name.as_deref().unwrap_or(&q.sender)),
            text = escape(&quote(&q.text)),
        ),
        (None, Some(id)) => format!(r#"<span class="quote">↳ re #{id}</span>"#),
        (None, None) => String::new(),
    };
    format!(
        r#"<div class="msg{compact}{reply}" id="m{id}"><div class="avatar {av}">{ini}</div><div class="gutter">{link}</div><div class="body"><div class="head"><span class="who">{who}</span>{badge}{to} <time datetime="{iso}">{hm}</time> {link}</div>{quoted}<div class="text">{text}</div>{ask}</div></div>"#,
        compact = if follows { " compact" } else { "" },
        reply = if parent.is_some() { " reply" } else { "" },
        id = m.id,
        link = link,
        av = avatar_class(&m.sender),
        ini = escape(&initial(m.sender_name.as_deref().unwrap_or(&m.sender))),
        who = if m.sender == OWNER || m.sender == FORGE {
            sender_label(&m.sender)
        } else {
            session_label(&m.sender, m.sender_name.as_deref())
        },
        badge = kind_badge(m),
        to = target_label(m),
        iso = iso_utc(m.created_at),
        hm = format_hm(m.created_at),
        quoted = quoted,
        text = message_text(m),
        ask = ask_state_line(m, now),
    )
}

/// The text of a message as HTML. The deadline of an ask is written into its text
/// in UTC, because agents read that text; on the page the deadline is a `<time>`, so
/// the browser shows it in the same time zone as the time of the message.
fn message_text(m: &Message) -> String {
    let html = linkify(&m.text);
    let Some(by) = m.ask.as_ref().and_then(|a| a.reply_by) else {
        return html;
    };
    let written = format!("Deadline: by {} UTC", format_hm(by));
    // The deadline line comes after the fields an agent wrote: take the last one.
    match html.rfind(&written) {
        Some(at) => format!(
            r#"{}Deadline: by <time datetime="{}">{} UTC</time>{}"#,
            &html[..at],
            iso_utc(by),
            format_hm(by),
            &html[at + written.len()..]
        ),
        None => html,
    }
}

/// The feed of a topic: messages with a divider between days; messages of the
/// same sender within five minutes follow each other without a new header. An
/// answer is shown right under the ask it answers, not where it came in time,
/// so that a question and its answer are read together.
pub fn messages_fragment(messages: &[Message], now: i64) -> String {
    if messages.is_empty() {
        return r#"<p class="empty">Nothing here yet. Write the first message.</p>"#.to_string();
    }
    let on_page = |id: i64| messages.iter().find(|m| m.id == id);
    let mut out = String::new();
    let mut prev: Option<&Message> = None;
    for m in messages {
        // Shown under its ask below.
        if m.answers.and_then(on_page).is_some() {
            continue;
        }
        let new_day = prev.is_none_or(|p| day_of(p.created_at) != day_of(m.created_at));
        if new_day {
            let _ = write!(
                out,
                r#"<div class="day">{}</div>"#,
                format_date(m.created_at)
            );
        }
        let follows = !new_day
            && prev.is_some_and(|p| {
                p.sender == m.sender
                    && p.kind == m.kind
                    && p.target == m.target
                    && m.created_at - p.created_at < 300
            });
        out.push_str(&message_html(m, now, follows, None));
        for a in messages.iter().filter(|a| a.answers == Some(m.id)) {
            out.push_str(&message_html(a, now, false, Some(m)));
        }
        prev = Some(m);
    }
    out
}

/// Sessions in a project (or in all of them), for the side panel of a topic.
pub fn people_fragment(sessions: &[Session], now: i64) -> String {
    if sessions.is_empty() {
        return r#"<h2>In the project now</h2><p class="empty">Nobody.</p>"#.to_string();
    }
    let mut out = String::from(r#"<h2>In the project now</h2><ul>"#);
    for s in sessions {
        let state = s.state(now);
        let _ = write!(
            out,
            r#"<li class="{state}"><span class="dot"></span><div><div class="who">{who}</div><div class="meta">{agent} · {roles}</div></div></li>"#,
            state = state.as_str(),
            who = session_label(&s.handle, s.name.as_deref()),
            agent = escape(&s.agent()),
            roles = escape(&s.roles.join(", ")),
        );
    }
    out.push_str("</ul>");
    out
}

/// The board: one table per project of the sessions that have not ended. It is
/// a view of the registry; it never adds a message to a topic.
pub fn board_fragment(
    sessions: &[Session],
    leases: &[Lease],
    counters: &HashMap<String, Counters>,
    now: i64,
) -> String {
    board_fragment_with(
        sessions,
        leases,
        &[],
        counters,
        &AnswerStats::default(),
        now,
    )
}

/// A duration for the owner: `45 s`, `2 min 5 s`, `1 h 10 min`.
fn format_duration(secs: i64) -> String {
    match secs.max(0) {
        s @ 0..=59 => format!("{s} s"),
        s @ 60..=3599 => match s % 60 {
            0 => format!("{} min", s / 60),
            r => format!("{} min {r} s", s / 60),
        },
        s => match s % 3600 / 60 {
            0 => format!("{} h", s / 3600),
            m => format!("{} h {m} min", s / 3600),
        },
    }
}

/// How the questions of the last day were answered, over all projects: their
/// number, the share answered, and the median and the 90th percentile of the time
/// to an answer. The aim is a median under two minutes and nine in ten within five.
pub fn answers_fragment(a: &AnswerStats) -> String {
    if a.asks == 0 {
        return r#"<p class="noise answers">Requests in the last 24 hours: none.</p>"#.to_string();
    }
    let share = (a.answered * 100 + a.asks / 2) / a.asks;
    let time = match (a.median_secs, a.p90_secs) {
        (Some(median), Some(p90)) => format!(
            "time to answer: median {}, 90% within {}",
            format_duration(median),
            format_duration(p90)
        ),
        _ => "no answers yet".to_string(),
    };
    format!(
        r#"<p class="noise answers">Requests in the last 24 hours: {} · answered {}% ({} of {}) · {}</p>"#,
        a.asks, share, a.answered, a.asks, time
    )
}

/// The issue of a session: what it said itself, else its claim in the forge, as a link.
fn issue_cell(s: &Session, claims: &[Claim]) -> String {
    if let Some(issue) = s.issue.as_deref() {
        return escape(issue);
    }
    match claim_of(s.branch.as_deref(), s.worktree.as_deref(), claims) {
        Some(c) => {
            let text = format!("#{} {}", c.issue, escape(&c.title));
            let repo = path_escape(&c.repo).replace("%2F", "/");
            let cell = match crate::links::issue_url(&repo, c.issue) {
                Some(url) => format!(r#"<a href="{url}">{text}</a>"#),
                None => text,
            };
            format!("{cell}{}", if c.merged { " · merged" } else { "" })
        }
        None => "—".to_string(),
    }
}

/// The board, with the claims of the forge to name the issue of a session.
pub fn board_fragment_with(
    sessions: &[Session],
    leases: &[Lease],
    claims: &[Claim],
    counters: &HashMap<String, Counters>,
    answers: &AnswerStats,
    now: i64,
) -> String {
    let mut out = leases_fragment(leases);
    out.push_str(&answers_fragment(answers));
    if sessions.is_empty() {
        out.push_str(r#"<p class="empty">Nobody is working right now.</p>"#);
        return out;
    }
    let mut projects: Vec<&str> = sessions.iter().map(|s| s.project.as_str()).collect();
    projects.sort_unstable();
    projects.dedup();
    for project in projects {
        let _ = write!(
            out,
            r#"<section class="project"><h3><a href="/p/{p}/{g}">{name}</a></h3><table><thead><tr><th>Session</th><th>Roles</th><th>Agent</th><th>Task</th><th>Branch</th><th>State</th><th>Signal</th></tr></thead><tbody>"#,
            p = path_escape(project),
            g = GENERAL,
            name = escape(project),
        );
        for s in sessions.iter().filter(|s| s.project == project) {
            let state = s.state(now);
            let _ = write!(
                out,
                r#"<tr class="{class}"><td>{who}</td><td>{roles}</td><td>{agent}</td><td>{issue}</td><td>{branch}</td><td>{state}</td><td>{ago}</td></tr>"#,
                class = state.as_str(),
                who = session_label(&s.handle, s.name.as_deref()),
                roles = escape(&s.roles.join(", ")),
                agent = escape(&s.agent()),
                issue = issue_cell(s, claims),
                branch = escape(s.branch.as_deref().unwrap_or("—")),
                state = state.label(),
                ago = format_ago(now - s.last_seen),
            );
        }
        let c = counters.get(project).copied().unwrap_or_default();
        let _ = write!(
            out,
            r#"</tbody></table><p class="noise">Last 24 hours: messages {} · requests {} (answered {}) · rejected {} · wakes {} · PRs merged {}{}</p></section>"#,
            c.sent,
            c.asks,
            c.answered,
            c.rejected,
            c.wakes,
            c.merged,
            per_pull_request(c),
        );
    }
    out
}

/// ` · messages per PR 0.2`: the number the design aims below 0.3.
fn per_pull_request(c: Counters) -> String {
    if c.merged == 0 {
        return String::new();
    }
    let tenths = (c.sent * 100 + c.merged * 5) / (c.merged * 10);
    format!(" · messages per PR {}.{}", tenths / 10, tenths % 10)
}

/// The leases on shared resources in force, from the forge. Empty when there are none.
fn leases_fragment(leases: &[Lease]) -> String {
    if leases.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        r#"<section class="project leases"><h3>Resource leases</h3><table><thead><tr><th>Resource</th><th>Kind</th><th>Agent</th><th>Action</th><th>Until (UTC)</th></tr></thead><tbody>"#,
    );
    for l in leases {
        let _ = write!(
            out,
            r#"<tr class="{kind}"><td>{title}</td><td>{label}</td><td>{agent}</td><td>{action}</td><td>{until}</td></tr>"#,
            kind = if l.kind == LeaseKind::Exclusive {
                "active"
            } else {
                "idle"
            },
            title = {
                let name = escape(if l.title.is_empty() {
                    &l.repo
                } else {
                    &l.title
                });
                let repo = path_escape(&l.repo).replace("%2F", "/");
                match crate::links::issue_url(&repo, l.issue) {
                    Some(url) => format!(r#"<a href="{url}">{name}</a>"#),
                    None => name,
                }
            },
            label = if l.kind == LeaseKind::Exclusive {
                "exclusive"
            } else {
                "shared"
            },
            agent = escape(&l.agent),
            action = escape(l.action.as_deref().unwrap_or("—")),
            until = format_hm(l.until),
        );
    }
    out.push_str("</tbody></table></section>");
    out
}

/// The frame of every page: the sidebar and the main area.
fn shell(
    title: &str,
    nav: &[ProjectNav],
    current: Option<(&str, &str)>,
    main_attrs: &str,
    main: &str,
) -> String {
    let board_current = if current.is_none() { " current" } else { "" };
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="/app.css">
</head>
<body>
<div class="app">
<aside id="side" class="side">
<a class="brand" href="/">chappe-bus</a>
<a class="board-link{board_current}" href="/">◎ Who works on what</a>
<div class="side-title">Projects</div>
<div id="nav">{nav}</div>
</aside>
<div class="main"{main_attrs}>
{main}
</div>
</div>
<div id="scrim"></div>
<script src="/app.js" defer></script>
</body>
</html>
"#,
        title = escape(title),
        nav = nav_fragment(nav, current),
    )
}

pub fn board_page(
    nav: &[ProjectNav],
    sessions: &[Session],
    leases: &[Lease],
    claims: &[Claim],
    counters: &HashMap<String, Counters>,
    answers: &AnswerStats,
    now: i64,
) -> String {
    let main = format!(
        r#"<header class="topbar"><button id="menu" class="icon" aria-label="Menu">☰</button><div class="title"><h1>Who works on what</h1></div></header>
<div class="scroll"><div id="board">{}</div></div>"#,
        board_fragment_with(sessions, leases, claims, counters, answers, now)
    );
    shell("chappe-bus", nav, None, " data-board", &main)
}

pub fn topic_page(
    nav: &[ProjectNav],
    project: &str,
    topic: &TopicInfo,
    messages: &[Message],
    people: &[Session],
    now: i64,
) -> String {
    let p = path_escape(project);
    let t = path_escape(&topic.name);
    let main = format!(
        r##"<header class="topbar"><button id="menu" class="icon" aria-label="Menu">☰</button><div class="title"><span class="crumb">{project_e}</span><h1># {topic_e}</h1><span class="about">{about}</span></div><button id="people-toggle" class="icon" aria-label="Who is in the project">👥</button></header>
<div class="cols">
<section class="chat">
<div id="messages" class="feed">{feed}</div>
<form class="composer" method="post" action="/p/{p}/{t}/post"><textarea name="text" rows="1" maxlength="{MAX_TEXT_CHARS}" placeholder="Message in # {topic_e}" required></textarea><button type="submit">Send</button></form>
</section>
<aside id="people" class="people">{people}</aside>
</div>"##,
        project_e = escape(project),
        topic_e = escape(&topic.name),
        about = escape(topic.about.as_deref().unwrap_or("")),
        feed = messages_fragment(messages, now),
        people = people_fragment(people, now),
    );
    let attrs = format!(
        r#" data-project="{}" data-topic="{}""#,
        escape(project),
        escape(&topic.name)
    );
    shell(
        &format!("# {} · {project} — chappe-bus", topic.name),
        nav,
        Some((project, &topic.name)),
        &attrs,
        &main,
    )
}

pub fn error_page(nav: &[ProjectNav], title: &str, reason: &str, back: &str) -> String {
    let main = format!(
        r#"<header class="topbar"><button id="menu" class="icon" aria-label="Menu">☰</button><div class="title"><h1>{}</h1></div></header><div class="scroll"><p>{}</p><p><a href="{}">Back</a></p></div>"#,
        escape(title),
        escape(reason),
        escape(back),
    );
    shell(title, nav, None, "", &main)
}

#[cfg(test)]
#[path = "html_test.rs"]
mod tests;
