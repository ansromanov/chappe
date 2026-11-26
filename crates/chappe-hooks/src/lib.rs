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

mod client;
use client::*;

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
#[path = "lib_test.rs"]
mod tests;
