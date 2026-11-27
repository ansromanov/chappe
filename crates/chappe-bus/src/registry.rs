//! The registry of agent sessions: who works on what, and whether they are
//! still there.
//!
//! A session registers when it starts work and leaves when it ends. Its state
//! is never stored: it follows from the time of the last signal, so there is
//! nothing to expire and no background task. `agent` and `host` come from the
//! token, never from the client, so a session cannot claim another machine.

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};

use rusqlite::{OptionalExtension, Row, params};
use serde::Deserialize;

use crate::message::looks_like_secret;
use crate::store::{Agent, Store, StoreError};

/// A signal this recent makes a session `active`.
pub const ACTIVE_WINDOW_SECS: i64 = 10 * 60;
/// Without a signal for this long a session is `gone`.
pub const GONE_AFTER_SECS: i64 = 24 * 60 * 60;
/// A session silent for this long whose process is gone (the host daemon says so) is
/// `gone` too: it is not picked as the receiver of a question to a role or a project.
pub const PROCESS_GONE_AFTER_SECS: i64 = 2 * 60 * 60;
/// Records of finished sessions are deleted after this long.
const KEEP_GONE_SECS: i64 = 7 * 24 * 60 * 60;

const MAX_ROLES: usize = 8;
/// Characters in a session name.
pub const MAX_NAME: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    Idle,
    Gone,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Idle => "idle",
            State::Gone => "gone",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Idle => "idle",
            State::Gone => "ended",
        }
    }
}

/// The state of a session at `now`.
pub fn state(last_seen: i64, left_at: Option<i64>, now: i64) -> State {
    let age = now - last_seen;
    if left_at.is_some() || age >= GONE_AFTER_SECS {
        State::Gone
    } else if age < ACTIVE_WINDOW_SECS {
        State::Active
    } else {
        State::Idle
    }
}

/// Whether the session has ended for good: it left, or was silent for a day. Such a
/// session cannot send, read or be written to; a session that is only `gone` because
/// its process disappeared can (its next signal brings it back).
pub fn ended(last_seen: i64, left_at: Option<i64>, now: i64) -> bool {
    left_at.is_some() || now - last_seen >= GONE_AFTER_SECS
}

/// What a session tells about itself when it registers.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSession {
    pub roles: Vec<String>,
    pub project: String,
    pub issue: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub session_id: Option<String>,
    pub transcript: Option<String>,
    /// The name the tool shows for the session, as the owner knows it.
    pub name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub handle: String,
    pub machine: String,
    pub tool: String,
    pub roles: Vec<String>,
    pub project: String,
    pub issue: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub session_id: Option<String>,
    pub transcript: Option<String>,
    pub name: Option<String>,
    pub registered_at: i64,
    pub last_seen: i64,
    pub left_at: Option<i64>,
    /// When the host daemon last found no process for the session.
    pub process_gone_at: Option<i64>,
}

impl Session {
    /// `gone` also for a session silent for two hours whose process the host daemon
    /// did not find.
    pub fn state(&self, now: i64) -> State {
        if self.process_gone_at.is_some() && now - self.last_seen >= PROCESS_GONE_AFTER_SECS {
            return State::Gone;
        }
        state(self.last_seen, self.left_at, now)
    }

    /// See [`ended`].
    pub fn ended(&self, now: i64) -> bool {
        ended(self.last_seen, self.left_at, now)
    }

    /// `claude@host-a`.
    pub fn agent(&self) -> String {
        format!("{}@{}", self.tool, self.machine)
    }
}

#[derive(Debug)]
pub enum SessionError {
    /// The request is wrong; the text says how, for the agent to read.
    Invalid(String),
    NotFound,
    /// The session belongs to another machine or tool.
    Forbidden,
    /// The session has left or expired; register again.
    Gone,
    Store(StoreError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Invalid(why) => f.write_str(why),
            SessionError::NotFound => f.write_str("no such session"),
            SessionError::Forbidden => {
                f.write_str("the session belongs to another machine or tool")
            }
            SessionError::Gone => f.write_str("the session has ended; register again"),
            SessionError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<rusqlite::Error> for SessionError {
    fn from(e: rusqlite::Error) -> Self {
        SessionError::Store(e.into())
    }
}

pub type Result<T> = std::result::Result<T, SessionError>;

fn invalid<T>(why: impl Into<String>) -> Result<T> {
    Err(SessionError::Invalid(why.into()))
}

fn valid_name(s: &str, first: fn(char) -> bool, max: usize) -> bool {
    s.chars().next().is_some_and(first)
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn valid_role(s: &str) -> bool {
    valid_name(s, |c| c.is_ascii_lowercase(), 24)
}

fn valid_project(s: &str) -> bool {
    valid_name(s, |c| c.is_ascii_lowercase() || c.is_ascii_digit(), 32)
}

/// A single-line optional text: trimmed, empty means absent, no control
/// characters, no secrets, at most `max` characters.
fn clean_field(name: &str, value: Option<String>, max: usize) -> Result<Option<String>> {
    let Some(value) = value else { return Ok(None) };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.chars().any(char::is_control) {
        return invalid(format!("{name}: control characters are not allowed"));
    }
    if looks_like_secret(value) {
        return invalid(format!("{name}: looks like a secret and is not stored"));
    }
    if value.chars().count() > max {
        return invalid(format!("{name}: longer than {max} characters"));
    }
    Ok(Some(value.to_string()))
}

impl NewSession {
    /// Checks and normalises the fields.
    pub fn clean(self) -> Result<NewSession> {
        let mut roles: Vec<String> = Vec::new();
        for role in self.roles {
            let role = role.trim().to_string();
            if !valid_role(&role) {
                return invalid(format!(
                    "role {role:?}: lowercase letters, digits and '-', starting with a letter, up to 24 characters"
                ));
            }
            if !roles.contains(&role) {
                roles.push(role);
            }
        }
        if roles.is_empty() {
            return invalid("at least one role is required (browser, audio, kernel, ...)");
        }
        if roles.len() > MAX_ROLES {
            return invalid(format!("at most {MAX_ROLES} roles"));
        }
        let project = self.project.trim().to_string();
        if !valid_project(&project) {
            return invalid("project: lowercase letters, digits and '-', up to 32 characters");
        }
        Ok(NewSession {
            roles,
            project,
            issue: clean_field("issue", self.issue, 40)?,
            branch: clean_field("branch", self.branch, 100)?,
            worktree: clean_field("worktree", self.worktree, 200)?,
            session_id: clean_field("session_id", self.session_id, 100)?,
            transcript: clean_field("transcript", self.transcript, 300)?,
            name: clean_field("name", self.name, MAX_NAME)?,
        })
    }
}

/// Four random hex digits; collisions are handled by the caller.
fn short_id(extra: usize) -> String {
    let n = RandomState::new().build_hasher().finish();
    format!(
        "{:0width$x}",
        n & ((1u64 << (16 + 4 * extra)) - 1),
        width = 4 + extra
    )
}

const COLUMNS: &str = "handle, machine, tool, roles, project, issue, branch, worktree, session_id, transcript, registered_at, last_seen, left_at, name, process_gone_at";

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<Session> {
    let roles: String = row.get(3)?;
    Ok(Session {
        handle: row.get(0)?,
        machine: row.get(1)?,
        tool: row.get(2)?,
        roles: roles.split(',').map(str::to_string).collect(),
        project: row.get(4)?,
        issue: row.get(5)?,
        branch: row.get(6)?,
        worktree: row.get(7)?,
        session_id: row.get(8)?,
        transcript: row.get(9)?,
        registered_at: row.get(10)?,
        last_seen: row.get(11)?,
        left_at: row.get(12)?,
        name: row.get(13)?,
        process_gone_at: row.get(14)?,
    })
}

/// Which sessions `who` lists.
#[derive(Clone, Copy, Debug, Default)]
pub struct Filter<'a> {
    pub project: Option<&'a str>,
    pub role: Option<&'a str>,
}

impl Store {
    /// Registers a session for `agent`. A session that registers again with a
    /// `session_id` it already registered under (a tool firing its start hook
    /// twice) gets its record back, updated, not a second one.
    pub fn register(&self, agent: &Agent, new: NewSession, now: i64) -> Result<Session> {
        let new = new.clean()?;
        let conn = self.conn();
        conn.execute(
            "DELETE FROM sessions WHERE last_seen < ?1 AND (left_at IS NOT NULL OR last_seen < ?2)",
            params![now - KEEP_GONE_SECS, now - GONE_AFTER_SECS],
        )?;
        let roles = new.roles.join(",");
        crate::chat::ensure_project(&conn, &new.project)?;

        if let Some(session_id) = &new.session_id {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT handle FROM sessions
                     WHERE machine = ?1 AND tool = ?2 AND session_id = ?3
                       AND left_at IS NULL AND last_seen > ?4",
                    params![agent.machine, agent.tool, session_id, now - GONE_AFTER_SECS],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(handle) = existing {
                conn.execute(
                    "UPDATE sessions SET roles = ?2, project = ?3, issue = ?4, branch = ?5,
                         worktree = ?6, transcript = ?7, last_seen = ?8, name = COALESCE(?9, name),
                         process_gone_at = NULL
                     WHERE handle = ?1",
                    params![
                        handle,
                        roles,
                        new.project,
                        new.issue,
                        new.branch,
                        new.worktree,
                        new.transcript,
                        now,
                        new.name
                    ],
                )?;
                return load(&conn, &handle)?.ok_or(SessionError::NotFound);
            }
        }

        let mut extra = 0;
        let handle = loop {
            let candidate = format!("{}-{}", new.roles[0], short_id(extra));
            let taken = conn
                .query_row(
                    "SELECT 1 FROM sessions WHERE handle = ?1",
                    [&candidate],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !taken {
                break candidate;
            }
            extra = (extra + 1).min(4);
        };
        conn.execute(
            "INSERT INTO sessions (handle, machine, tool, roles, project, issue, branch, worktree,
                                   session_id, transcript, registered_at, last_seen, name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?12)",
            params![
                handle,
                agent.machine,
                agent.tool,
                roles,
                new.project,
                new.issue,
                new.branch,
                new.worktree,
                new.session_id,
                new.transcript,
                now,
                new.name
            ],
        )?;
        load(&conn, &handle)?.ok_or(SessionError::NotFound)
    }

    /// Sets the name of the session (the tool's title for it changed). An empty
    /// name keeps the old one.
    pub fn set_name(&self, agent: &Agent, handle: &str, name: &str, now: i64) -> Result<()> {
        let conn = self.conn();
        let session = owned(&conn, agent, handle)?;
        if session.ended(now) {
            return Err(SessionError::Gone);
        }
        if let Some(name) = clean_field("name", Some(name.to_string()), MAX_NAME)? {
            conn.execute(
                "UPDATE sessions SET name = ?2 WHERE handle = ?1",
                params![handle, name],
            )?;
        }
        Ok(())
    }

    /// Where the session is now: its directory and branch, as its hook sees
    /// them. Says whether anything changed. Absent values keep the old ones.
    pub fn set_place(
        &self,
        agent: &Agent,
        handle: &str,
        worktree: Option<&str>,
        branch: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn();
        owned(&conn, agent, handle)?;
        let worktree = clean_field("worktree", worktree.map(str::to_string), 200)?;
        let branch = clean_field("branch", branch.map(str::to_string), 100)?;
        let changed = conn.execute(
            "UPDATE sessions SET worktree = COALESCE(?2, worktree), branch = COALESCE(?3, branch)
             WHERE handle = ?1 AND (COALESCE(?2, worktree) IS NOT worktree OR COALESCE(?3, branch) IS NOT branch)",
            params![handle, worktree, branch],
        )?;
        Ok(changed > 0)
    }

    /// Records a signal from the session; returns its state before the signal.
    pub fn beat(&self, agent: &Agent, handle: &str, now: i64) -> Result<State> {
        let conn = self.conn();
        let session = owned(&conn, agent, handle)?;
        // A session that is `gone` only because its process vanished comes back with its signal.
        if session.ended(now) {
            return Err(SessionError::Gone);
        }
        let before = session.state(now);
        conn.execute(
            "UPDATE sessions SET last_seen = ?2, process_gone_at = NULL WHERE handle = ?1",
            params![handle, now],
        )?;
        Ok(before)
    }

    /// Ends the session. Leaving twice is not an error.
    pub fn leave(&self, agent: &Agent, handle: &str, now: i64) -> Result<()> {
        let conn = self.conn();
        owned(&conn, agent, handle)?;
        conn.execute(
            "UPDATE sessions SET left_at = ?2 WHERE handle = ?1 AND left_at IS NULL",
            params![handle, now],
        )?;
        Ok(())
    }

    /// The session, if it belongs to `agent` and has not ended.
    pub fn live_session(&self, agent: &Agent, handle: &str, now: i64) -> Result<Session> {
        let session = owned(&self.conn(), agent, handle)?;
        if session.ended(now) {
            return Err(SessionError::Gone);
        }
        Ok(session)
    }

    /// Sessions that have not ended: active first, then by latest signal.
    pub fn sessions(&self, filter: Filter<'_>, now: i64) -> Result<Vec<Session>> {
        live_sessions(&self.conn(), filter, now)
    }
}

/// Sessions that have not ended (and are not `gone` for want of a process): active
/// first, then by latest signal.
pub(crate) fn live_sessions(
    conn: &rusqlite::Connection,
    filter: Filter<'_>,
    now: i64,
) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM sessions
         WHERE left_at IS NULL AND last_seen > ?1 AND (?2 IS NULL OR project = ?2)
           AND (process_gone_at IS NULL OR last_seen > ?3)"
    ))?;
    let rows = stmt.query_map(
        params![
            now - GONE_AFTER_SECS,
            filter.project,
            now - PROCESS_GONE_AFTER_SECS
        ],
        row_to_session,
    )?;
    let mut sessions = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    if let Some(role) = filter.role {
        sessions.retain(|s| s.roles.iter().any(|r| r == role));
    }
    sessions.sort_by(|a, b| {
        (
            a.state(now) as u8,
            std::cmp::Reverse(a.last_seen),
            &a.handle,
        )
            .cmp(&(
                b.state(now) as u8,
                std::cmp::Reverse(b.last_seen),
                &b.handle,
            ))
    });
    Ok(sessions)
}

pub(crate) fn load(conn: &rusqlite::Connection, handle: &str) -> rusqlite::Result<Option<Session>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM sessions WHERE handle = ?1"),
        [handle],
        row_to_session,
    )
    .optional()
}

/// The session, if it exists and belongs to `agent`.
pub(crate) fn owned(conn: &rusqlite::Connection, agent: &Agent, handle: &str) -> Result<Session> {
    let session = load(conn, handle)?.ok_or(SessionError::NotFound)?;
    if session.machine != agent.machine || session.tool != agent.tool {
        return Err(SessionError::Forbidden);
    }
    Ok(session)
}

#[cfg(test)]
#[path = "registry_test.rs"]
mod tests;
