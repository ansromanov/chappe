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
mod tests {
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
            load(&s.conn(), &left).unwrap().is_none()
                && load(&s.conn(), &expired).unwrap().is_none()
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
}
