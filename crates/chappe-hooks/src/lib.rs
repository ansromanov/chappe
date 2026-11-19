//! `chappe`: the command agents use to tell the bus what they work on.
//!
//! It talks only to the service, one request per command. If the service
//! cannot be reached the command says so and exits cleanly: an agent must not
//! stop its work because the bus is down.

pub mod args;
pub mod doctor;
pub mod hook;

use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use args::{Command, KINDS, Message, Register, USAGE};

const TIMEOUT: Duration = Duration::from_secs(10);

/// The environment `run` reads, so a test can supply its own.
pub struct Env {
    pub vars: HashMap<String, String>,
    pub cwd: Option<String>,
}

impl Env {
    pub fn from_process() -> Env {
        Env {
            vars: std::env::vars().collect(),
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string()),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// The id the tool gives its own session in the environment of its commands.
    /// A Codex started from a Claude Code session inherits its variables, so the
    /// ones of the tool that runs the command come first.
    pub fn tool_session_id(&self) -> Option<&str> {
        let claude = ["CLAUDE_CODE_SESSION_ID"];
        let codex = ["CODEX_THREAD_ID", "CODEX_SESSION_ID"];
        let (first, second) = if self.running_in() == Some("codex") {
            (&codex[..], &claude[..])
        } else {
            (&claude[..], &codex[..])
        };
        first
            .iter()
            .chain(second)
            .find_map(|name| self.get(name))
            .filter(|id| safe_name(id))
    }

    /// The name of the session as the owner knows it: `CHAPPE_NAME`, or the
    /// title Claude Code keeps for the session in `~/.claude/sessions`.
    pub fn session_name(&self, session_id: Option<&str>) -> Option<String> {
        let name = self
            .get("CHAPPE_NAME")
            .map(str::to_string)
            .or_else(|| self.claude_session_name(session_id?))?;
        // The service refuses what it cannot show; a title that is too long
        // or has odd characters must not cost the session its registration.
        let name: String = name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect();
        (!name.is_empty()).then_some(name)
    }

    fn claude_session_name(&self, session_id: &str) -> Option<String> {
        let dir = match self.get("CLAUDE_CONFIG_DIR") {
            Some(d) => std::path::PathBuf::from(d),
            None => std::path::PathBuf::from(self.get("HOME").or(self.get("USERPROFILE"))?)
                .join(".claude"),
        }
        .join("sessions");
        let mut found: Option<(i64, String)> = None;
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if v["sessionId"].as_str() != Some(session_id) {
                continue;
            }
            let Some(name) = v["name"].as_str().filter(|n| !n.trim().is_empty()) else {
                continue;
            };
            let at = v["updatedAt"].as_i64().unwrap_or(0);
            if found.as_ref().is_none_or(|(best, _)| at >= *best) {
                found = Some((at, name.trim().to_string()));
            }
        }
        found.map(|(_, name)| name)
    }

    /// Where handles are remembered: `~/.local/state/chappe/handles/` (`%LOCALAPPDATA%` on Windows).
    pub(crate) fn handles_dir(&self) -> Option<std::path::PathBuf> {
        let base = self
            .get("XDG_STATE_HOME")
            .map(str::to_string)
            .or_else(|| self.get("LOCALAPPDATA").map(str::to_string))
            .or_else(|| self.get("HOME").map(|h| format!("{h}/.local/state")))?;
        Some(
            std::path::PathBuf::from(base)
                .join("chappe")
                .join("handles"),
        )
    }

    /// The handle `register` printed for the tool session with this id.
    pub fn remembered_handle(&self, session_id: &str) -> Option<String> {
        if !safe_name(session_id) {
            return None;
        }
        let text = std::fs::read_to_string(self.handles_dir()?.join(session_id)).ok()?;
        Some(text.trim().to_string()).filter(|h| !h.is_empty())
    }

    pub fn remember_handle(&self, session_id: &str, handle: &str) {
        if !safe_name(session_id) {
            return;
        }
        if let Some(dir) = self.handles_dir() {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join(session_id), format!("{handle}\n"));
        }
    }

    pub fn forget_handle(&self, session_id: &str) {
        if let (true, Some(dir)) = (safe_name(session_id), self.handles_dir()) {
            let _ = std::fs::remove_file(dir.join(session_id));
        }
    }

    /// The tool whose session runs this command, from what the tool puts in the
    /// environment of its commands and hooks.
    fn running_in(&self) -> Option<&'static str> {
        if self.get("CODEX_THREAD_ID").is_some() || self.get("CODEX_SESSION_ID").is_some() {
            Some("codex")
        } else if self.get("CLAUDECODE").is_some() || self.get("CLAUDE_CODE_SESSION_ID").is_some() {
            Some("claude")
        } else {
            None
        }
    }

    /// `~/.config/chappe/token.<tool>` for the tool that runs the command, else
    /// `~/.config/chappe/token` (`%APPDATA%\chappe` on Windows). A machine has
    /// one token per tool, so one file cannot serve Claude Code and Codex both.
    fn default_token_file(&self) -> Option<String> {
        let dir = self
            .get("XDG_CONFIG_HOME")
            .map(str::to_string)
            .or_else(|| self.get("APPDATA").map(str::to_string))
            .or_else(|| self.get("HOME").map(|h| format!("{h}/.config")))?;
        let plain = format!("{dir}/chappe/token");
        let Some(tool) = self.running_in() else {
            return Some(plain);
        };
        let per_tool = format!("{plain}.{tool}");
        Some(if std::path::Path::new(&per_tool).exists() {
            per_tool
        } else {
            plain
        })
    }

    /// The address of the service: `CHAPPE_URL`. There is no built-in default, so
    /// a command never talks to a server its owner did not choose.
    pub(crate) fn url(&self) -> Result<String, String> {
        self.get("CHAPPE_URL")
            .map(|u| u.trim().to_string())
            .ok_or_else(|| "no service address: set CHAPPE_URL".to_string())
    }

    /// The token, from the environment or its file. Never from the command line.
    pub(crate) fn token(&self) -> Result<String, String> {
        if let Some(t) = self.get("CHAPPE_TOKEN") {
            return Ok(t.trim().to_string());
        }
        let file = self
            .get("CHAPPE_TOKEN_FILE")
            .map(str::to_string)
            .or_else(|| self.default_token_file())
            .ok_or("no token: set CHAPPE_TOKEN or CHAPPE_TOKEN_FILE")?;
        let text = std::fs::read_to_string(&file).map_err(|e| {
            format!("no token: cannot read {file} ({e}); set CHAPPE_TOKEN or CHAPPE_TOKEN_FILE")
        })?;
        match text.trim() {
            "" => Err(format!("no token: {file} is empty")),
            token => Ok(token.to_string()),
        }
    }
}

/// An id that is safe as a file name: letters, digits, `-`, `_`, `.`, no dots first.
pub(crate) fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// What came back from the service.
pub(crate) enum Reply {
    Ok(String),
    /// The service answered with an error the agent can act on.
    Refused {
        status: u16,
        reason: String,
    },
    /// No answer, or the service itself is failing.
    Down(String),
}

pub(crate) struct Client {
    base: String,
    token: String,
    agent: ureq::Agent,
}

#[derive(Serialize)]
pub(crate) struct RegisterBody<'a> {
    roles: &'a [String],
    project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcript: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

#[derive(Deserialize)]
pub(crate) struct Handle {
    pub(crate) handle: String,
    /// Who else works in the project and which resources are held.
    #[serde(default)]
    pub(crate) briefing: Option<String>,
}

#[derive(Deserialize)]
struct Row {
    handle: String,
    #[serde(default)]
    name: Option<String>,
    roles: Vec<String>,
    agent: String,
    project: String,
    issue: Option<String>,
    state: String,
    last_seen_secs_ago: i64,
}

#[derive(Deserialize)]
struct TopicRow {
    project: String,
    name: String,
    about: Option<String>,
    messages: i64,
}

#[derive(Serialize)]
struct TopicBody<'a> {
    handle: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    about: Option<&'a str>,
}

#[derive(Serialize)]
struct MessageBody<'a> {
    handle: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    topic: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    what: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    why: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    expect: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    by: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ask: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    wait: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    dry_run: bool,
}

#[derive(Deserialize)]
struct ReceiverRow {
    handle: String,
    agent: String,
    roles: Vec<String>,
    issue: Option<String>,
    active: bool,
}

#[derive(Deserialize)]
struct SentRow {
    id: Option<i64>,
    project: String,
    topic: String,
    text: String,
    receivers: Vec<ReceiverRow>,
    wake: Option<String>,
    dry_run: bool,
    #[serde(default)]
    similar: Vec<SimilarRow>,
}

#[derive(Deserialize)]
struct SimilarRow {
    id: i64,
    question: String,
    answer: String,
}

#[derive(Serialize)]
pub(crate) struct InboxBody<'a> {
    handle: &'a str,
    /// The tool's current name for the session, so that a rename reaches the page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<&'a str>,
    /// Where the session is now, for the board to find its issue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<&'a str>,
}

#[derive(Deserialize)]
struct InboxRow {
    rendered: String,
}

#[derive(Deserialize)]
pub(crate) struct InboxAnswer {
    #[serde(default)]
    notes: Vec<String>,
    messages: Vec<InboxRow>,
    waiting: usize,
    next_in_secs: Option<i64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
}

impl Client {
    pub(crate) fn new(base: &str, token: String) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Client {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    fn finish(result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Reply {
        let mut response = match result {
            Ok(r) => r,
            Err(e) => return Reply::Down(e.to_string()),
        };
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        match status {
            200..=299 => Reply::Ok(body),
            500..=599 => Reply::Down(format!("the service answered {status}")),
            _ => {
                let reason = serde_json::from_str::<ErrorBody>(&body)
                    .map(|e| e.error)
                    .unwrap_or_else(|_| {
                        if status == 401 {
                            "the token was rejected".into()
                        } else {
                            format!("status {status}")
                        }
                    });
                Reply::Refused { status, reason }
            }
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub(crate) fn get(&self, path: &str) -> Reply {
        Client::finish(
            self.agent
                .get(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .call(),
        )
    }

    pub(crate) fn delete(&self, path: &str) -> Reply {
        Client::finish(
            self.agent
                .delete(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .call(),
        )
    }

    pub(crate) fn post_empty(&self, path: &str) -> Reply {
        Client::finish(
            self.agent
                .post(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .send_empty(),
        )
    }

    pub(crate) fn post_json(&self, path: &str, body: &impl Serialize) -> Reply {
        Client::finish(
            self.agent
                .post(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .send_json(body),
        )
    }
}

/// The handle: `--handle`, `CHAPPE_HANDLE`, or the one `register` remembered
/// for this tool session.
fn handle_of(given: Option<String>, env: &Env) -> Result<String, String> {
    given
        .or_else(|| env.get("CHAPPE_HANDLE").map(str::to_string))
        .or_else(|| {
            env.tool_session_id()
                .and_then(|id| env.remembered_handle(id))
        })
        .ok_or_else(|| {
            "no handle: pass --handle or set CHAPPE_HANDLE (register prints it)".to_string()
        })
}

/// Aligned columns, two spaces apart.
fn table(rows: &[Vec<String>]) -> String {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out
}

fn ago(secs: i64) -> String {
    match secs {
        ..=59 => "now".into(),
        60..=3599 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

fn who_table(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "nobody is registered\n".to_string();
    }
    let mut cells = vec![
        [
            "HANDLE", "NAME", "ROLES", "AGENT", "PROJECT", "ISSUE", "STATE", "SIGNAL",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for r in rows {
        cells.push(vec![
            r.handle.clone(),
            r.name.clone().unwrap_or_else(|| "-".into()),
            r.roles.join(","),
            r.agent.clone(),
            r.project.clone(),
            r.issue.clone().unwrap_or_else(|| "-".into()),
            r.state.clone(),
            ago(r.last_seen_secs_ago),
        ]);
    }
    table(&cells)
}

fn topics_table(rows: &[TopicRow]) -> String {
    let mut cells = vec![
        ["PROJECT", "TOPIC", "MESSAGES", "ABOUT"]
            .map(String::from)
            .to_vec(),
    ];
    for r in rows {
        cells.push(vec![
            r.project.clone(),
            r.name.clone(),
            r.messages.to_string(),
            r.about.clone().unwrap_or_default(),
        ]);
    }
    table(&cells)
}

fn query(project: Option<&str>, role: Option<&str>) -> String {
    let encode = |s: &str| -> String {
        s.bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b == b'-' {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect()
    };
    let parts: Vec<String> = [("project", project), ("role", role)]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| format!("{k}={}", encode(v))))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

fn register(client: &Client, r: Register, env: &Env) -> Result<Reply, String> {
    let project = r.project.as_deref().ok_or("register needs --project")?;
    if r.roles.is_empty() {
        return Err(
            "register needs --role (the areas you answer for: browser, audio, kernel, ...)".into(),
        );
    }
    let worktree = r.worktree.as_deref().or(env.cwd.as_deref());
    // The tool tells its commands the id of the session: the host needs it to wake it.
    let session_id = r.session_id.as_deref().or(env.tool_session_id());
    let name = r.name.clone().or_else(|| env.session_name(session_id));
    let body = RegisterBody {
        roles: &r.roles,
        project,
        issue: r.issue.as_deref(),
        branch: r.branch.as_deref(),
        worktree,
        session_id,
        transcript: r.transcript.as_deref(),
        name: name.as_deref(),
    };
    Ok(client.post_json("/v1/sessions", &body))
}

/// A message of one of the kinds. An `ask` without `--send` only looks: it asks
/// the service who would get it and shows that, so that the agent can see
/// whether the answer is on the board already.
fn message(client: &Client, m: Message, env: &Env) -> Result<Reply, String> {
    let handle = handle_of(m.handle.clone(), env)?;
    let body = MessageBody {
        handle: &handle,
        kind: &m.kind,
        to: m.to.as_deref(),
        topic: m.topic.as_deref(),
        what: m.what.as_deref(),
        why: m.why.as_deref(),
        expect: m.expect.as_deref(),
        by: m.by.as_deref(),
        link: m.link.as_deref(),
        ask: m.ask,
        result: m.result.as_deref(),
        wait: m.wait,
        dry_run: m.kind == "ask" && !m.send,
    };
    Ok(client.post_json("/v1/messages", &body))
}

fn who(receivers: &[ReceiverRow]) -> String {
    match receivers {
        [] => "the owner".to_string(),
        [one] => one.handle.clone(),
        many => format!("{} sessions", many.len()),
    }
}

/// What to print for a message that was sent, or only looked at.
fn sent_report(row: &SentRow, kind: &str) -> String {
    if !row.dry_run {
        let to = if kind == "owner" {
            "the owner".to_string()
        } else {
            who(&row.receivers)
        };
        let wake = match row.wake.as_deref() {
            Some("requested") => {
                "The session is idle: the host of its machine was asked to wake it.\n"
            }
            Some("pending") => {
                "The session is idle: a wake is already on its way and will carry this too.\n"
            }
            Some("limit") => {
                "The session was woken twice in the last hour: this waits for its next checkpoint.\n"
            }
            Some("no_host") => {
                "No host daemon is running on its machine: the session sees this at its next checkpoint.\n"
            }
            _ => "",
        };
        return format!(
            "sent #{} to {} in {}/{}\n{wake}",
            row.id.unwrap_or(0),
            to,
            row.project,
            row.topic
        );
    }
    let mut out = String::from("Not sent yet. Who would get it (the board):\n");
    for r in &row.receivers {
        let issue = r.issue.as_deref().unwrap_or("-");
        let state = if r.active { "active" } else { "idle" };
        out.push_str(&format!(
            "  {}  {}  roles {}  issue {}  {}\n",
            r.handle,
            r.agent,
            r.roles.join(","),
            issue,
            state
        ));
    }
    out.push_str(&format!(
        "\nThe message, in {}/{}:\n{}\n\n",
        row.project, row.topic, row.text
    ));
    if !row.similar.is_empty() {
        out.push_str("This was asked before, and answered:\n");
        for s in &row.similar {
            out.push_str(&format!(
                "  #{} {}\n     -> {}\n",
                s.id, s.question, s.answer
            ));
        }
        out.push_str("If that answers it, do not ask again.\n\n");
    }
    out.push_str(
        "Look at the board and at the issue of that area first. If the answer is there, do not ask.\nIf it is not, send the same command again with --send.\n",
    );
    out
}

/// Runs one command; returns the exit status.
pub fn run(args: &[String], env: &Env, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let command = match args::parse(args) {
        Ok(Command::Help) => {
            let _ = writeln!(out, "{USAGE}");
            return 0;
        }
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(err, "chappe: {e}\n\n{USAGE}");
            return 2;
        }
    };
    // The doctor looks at a missing token too, so it comes before the token is needed.
    if command == Command::Doctor {
        return doctor::run(env, out);
    }
    let token = match env.token() {
        Ok(t) => t,
        Err(e) => {
            let _ = writeln!(err, "chappe: {e}");
            return 1;
        }
    };
    let url = match env.url() {
        Ok(u) => u,
        Err(e) => {
            let _ = writeln!(err, "chappe: {e}");
            return 1;
        }
    };
    let client = Client::new(&url, token);

    let mut registered_session: Option<String> = None;
    let reply = match command {
        Command::Register(r) => {
            registered_session = r
                .session_id
                .clone()
                .or_else(|| env.tool_session_id().map(str::to_string));
            register(&client, r, env)
        }
        Command::Beat { handle } => {
            handle_of(handle, env).map(|h| client.post_empty(&format!("/v1/sessions/{h}/beat")))
        }
        Command::Leave { handle } => {
            handle_of(handle, env).map(|h| client.delete(&format!("/v1/sessions/{h}")))
        }
        Command::Who { project, role } => Ok(client.get(&format!(
            "/v1/sessions{}",
            query(project.as_deref(), role.as_deref())
        ))),
        Command::TopicCreate {
            name,
            about,
            handle,
        } => handle_of(handle, env).map(|h| {
            client.post_json(
                "/v1/topics",
                &TopicBody {
                    handle: &h,
                    name: &name,
                    about: about.as_deref(),
                },
            )
        }),
        Command::Topics { project } => {
            let q = query(project.as_deref(), None);
            Ok(client.get(&format!("/v1/topics{q}")))
        }
        Command::Message(m) => message(&client, m, env),
        Command::Inbox { handle } => handle_of(handle, env).map(|h| {
            client.post_json(
                "/v1/inbox",
                &InboxBody {
                    handle: &h,
                    name: None,
                    worktree: None,
                    branch: None,
                },
            )
        }),
        Command::Help | Command::Doctor => unreachable!("handled above"),
    };
    let reply = match reply {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(err, "chappe: {e}");
            return 2;
        }
    };

    match reply {
        Reply::Ok(body) => {
            let unexpected = |err: &mut dyn Write| {
                let _ = writeln!(err, "chappe: the service sent an unexpected answer");
                1
            };
            match args.first().map(String::as_str) {
                Some("register") => match serde_json::from_str::<Handle>(&body) {
                    Ok(h) => {
                        if let Some(id) = registered_session.as_deref() {
                            env.remember_handle(id, &h.handle);
                        }
                        let _ = writeln!(out, "{}", h.handle);
                        let _ = writeln!(
                            err,
                            "chappe: registered; use --handle {} (or CHAPPE_HANDLE) with beat and leave",
                            h.handle
                        );
                        if let Some(briefing) = &h.briefing {
                            let _ = writeln!(err, "{briefing}");
                        }
                    }
                    Err(_) => return unexpected(err),
                },
                Some("who") => match serde_json::from_str::<Vec<Row>>(&body) {
                    Ok(rows) => {
                        let _ = write!(out, "{}", who_table(&rows));
                    }
                    Err(_) => return unexpected(err),
                },
                Some("topics") => match serde_json::from_str::<Vec<TopicRow>>(&body) {
                    Ok(rows) => {
                        let _ = write!(out, "{}", topics_table(&rows));
                    }
                    Err(_) => return unexpected(err),
                },
                Some("topic") => match serde_json::from_str::<TopicRow>(&body) {
                    Ok(t) => {
                        let _ = writeln!(out, "created #{} in {}", t.name, t.project);
                    }
                    Err(_) => return unexpected(err),
                },
                Some("inbox") => match serde_json::from_str::<InboxAnswer>(&body) {
                    Ok(a) => {
                        for note in &a.notes {
                            let _ = writeln!(out, "{note}\n");
                        }
                        if !a.messages.is_empty() {
                            let blocks: Vec<&str> =
                                a.messages.iter().map(|m| m.rendered.as_str()).collect();
                            let _ = writeln!(out, "{}", blocks.join("\n\n---\n\n"));
                        } else if a.waiting > 0 {
                            let _ = writeln!(
                                out,
                                "{} message(s) are waiting; they come with the next inbox in {} s",
                                a.waiting,
                                a.next_in_secs.unwrap_or(0)
                            );
                        } else if a.notes.is_empty() {
                            let _ = writeln!(out, "no new messages");
                        }
                    }
                    Err(_) => return unexpected(err),
                },
                Some(kind) if KINDS.contains(&kind) => match serde_json::from_str::<SentRow>(&body)
                {
                    Ok(row) => {
                        let _ = write!(out, "{}", sent_report(&row, kind));
                    }
                    Err(_) => return unexpected(err),
                },
                other => {
                    if let (Some("leave"), Some(id)) = (other, env.tool_session_id()) {
                        env.forget_handle(id);
                    }
                    let _ = writeln!(out, "ok");
                }
            }
            0
        }
        Reply::Refused { status: 401, .. } => {
            let _ = writeln!(
                err,
                "chappe: the token was rejected; ask the owner for a token for this machine and tool"
            );
            1
        }
        Reply::Refused {
            status: 429,
            reason,
        } => {
            let _ = writeln!(err, "chappe: refused: {reason}");
            1
        }
        Reply::Refused {
            status: 409,
            reason,
        } => {
            let _ = writeln!(err, "chappe: not sent: {reason}");
            1
        }
        Reply::Refused { reason, .. } => {
            let _ = writeln!(err, "chappe: {reason}");
            1
        }
        Reply::Down(why) => {
            // The bus being down must not stop the agent's work.
            let _ = writeln!(
                err,
                "chappe: chappe-bus is not reachable ({why}); nothing was recorded, carry on"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
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
}
