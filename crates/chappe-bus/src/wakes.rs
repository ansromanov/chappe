//! Waking idle sessions.
//!
//! Only an `ask` wakes a session (an `answer` too, if the ask said `--wait`),
//! and a session is woken at most twice an hour: more asks wait in its queue and
//! the sender is told. The service never reaches a machine: a daemon on the
//! machine (`chappe-host`) asks for wakes over HTTP, does the waking with
//! the tool's own command, and reports the outcome. A wake that is not done in
//! two hours expires and the sender hears that it was not delivered.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::messaging::{InboxItem, expire_queue, queued_for, render};
use crate::registry::{Session, SessionError, State, load};
use crate::store::{Agent, Store};

/// Wakes of one session in an hour.
pub const MAX_WAKES_PER_HOUR: i64 = 2;
/// A wake nobody carried out for this long expires.
const WAKE_EXPIRY: i64 = 2 * 3600;
/// A host that has not asked for work for this long is taken to be down.
const HOST_ALIVE: i64 = 120;
/// A host that claimed a wake has this long to report.
const LEASE: i64 = 10 * 60;
/// Failed attempts before the wake is given up.
const MAX_FAILURES: i64 = 3;
/// The text of the woken session: its messages, one block each.
const SEPARATOR: &str = "\n\n---\n\n";

/// What became of the wish to wake the receiver of an ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeOutcome {
    /// The session is working: it reads the message at its next checkpoint.
    NotNeeded,
    /// The machine's host was asked to wake it.
    Requested,
    /// A wake is already waiting; it will carry this message too.
    Pending,
    /// Woken twice in the last hour: the message waits in its queue.
    Limit,
    /// No host daemon is running on its machine: the message waits in its queue.
    NoHost,
}

impl WakeOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            WakeOutcome::NotNeeded => "not_needed",
            WakeOutcome::Requested => "requested",
            WakeOutcome::Pending => "pending",
            WakeOutcome::Limit => "limit",
            WakeOutcome::NoHost => "no_host",
        }
    }
}

/// Decides, and unless it is a dry run asks for, the wake of an idle receiver.
pub(crate) fn decide_wake(
    conn: &Connection,
    receiver: &Session,
    now: i64,
    dry_run: bool,
) -> rusqlite::Result<WakeOutcome> {
    if receiver.state(now) == State::Active {
        return Ok(WakeOutcome::NotNeeded);
    }
    let handle = receiver.handle.as_str();
    let last_poll: Option<i64> = conn
        .query_row(
            "SELECT last_poll FROM hosts WHERE machine = ?1",
            [&receiver.machine],
            |r| r.get(0),
        )
        .optional()?;
    if !last_poll.is_some_and(|t| now - t < HOST_ALIVE) {
        return Ok(WakeOutcome::NoHost);
    }
    let waiting = conn
        .query_row(
            "SELECT 1 FROM wakes WHERE handle = ?1 AND state IN ('pending', 'claimed')",
            [handle],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if waiting {
        return Ok(WakeOutcome::Pending);
    }
    let lately: i64 = conn.query_row(
        "SELECT COUNT(*) FROM wakes WHERE handle = ?1 AND created_at > ?2 AND state IN ('pending', 'claimed', 'done')",
        params![handle, now - 3600],
        |r| r.get(0),
    )?;
    if lately >= MAX_WAKES_PER_HOUR {
        return Ok(WakeOutcome::Limit);
    }
    if !dry_run {
        conn.execute(
            "INSERT INTO wakes (handle, created_at, not_before, state) VALUES (?1, ?2, ?2, 'pending')",
            params![handle, now],
        )?;
    }
    Ok(WakeOutcome::Requested)
}

/// Whether a session needs waking was decided once, when the ask reached it. A
/// session that was working then (its last signal under ten minutes old) and has
/// since gone quiet, for example because it waits for its user, would never be
/// woken: its ask would wait in the queue until the time ran out. The host of
/// the machine asks every few seconds, so the question is put again here for the
/// sessions of its machine that have an ask waiting (or an answer to an ask sent
/// with `--wait`); the rules of [`decide_wake`] (idle only, two an hour, one
/// wake at a time) are the same.
fn request_missed_wakes(conn: &Connection, machine: &str, now: i64) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.handle FROM deliveries d
         JOIN messages m ON m.id = d.message_id
         JOIN sessions s ON s.handle = d.handle
         WHERE d.state = 'queued' AND s.machine = ?1 AND s.left_at IS NULL
           AND m.created_at > ?2
           AND ((m.kind = 'ask' AND (m.reply_by IS NULL OR m.reply_by > ?3))
                OR (m.kind = 'answer' AND EXISTS (SELECT 1 FROM messages q WHERE q.id = m.answers AND q.wait = 1)))",
    )?;
    let handles = stmt
        .query_map(params![machine, now - 86_400, now], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    for handle in handles {
        if let Some(session) = load(conn, &handle)? {
            decide_wake(conn, &session, now, false)?;
        }
    }
    Ok(())
}

fn host_seen(conn: &Connection, machine: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO hosts (machine, last_poll) VALUES (?1, ?2)
         ON CONFLICT (machine) DO UPDATE SET last_poll = excluded.last_poll",
        params![machine, now],
    )?;
    Ok(())
}

/// A note for every session that sent an ask still waiting in `handle`'s queue.
fn tell_senders(conn: &Connection, handle: &str, text: &str, now: i64) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT m.sender, m.id FROM deliveries d JOIN messages m ON m.id = d.message_id
         WHERE d.handle = ?1 AND d.state = 'queued' AND m.kind = 'ask' AND m.sender <> 'owner'",
    )?;
    let asks = stmt
        .query_map([handle], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut told: Vec<&str> = Vec::new();
    for (sender, id) in &asks {
        if !told.contains(&sender.as_str()) {
            told.push(sender);
            conn.execute(
                "INSERT INTO notes (handle, text, created_at) VALUES (?1, ?2, ?3)",
                params![sender, text, now],
            )?;
        }
        // The sender has heard about this ask: its time running out need not say it again.
        conn.execute(
            "INSERT OR IGNORE INTO ask_closed (message_id, at) VALUES (?1, ?2)",
            params![id, now],
        )?;
    }
    Ok(())
}

/// Gives up wakes that nobody carried out, and tells the senders.
pub(crate) fn expire_wakes(conn: &Connection, now: i64) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, handle FROM wakes WHERE state IN ('pending', 'claimed') AND created_at <= ?1",
    )?;
    let old = stmt
        .query_map([now - WAKE_EXPIRY], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);
    for (id, handle) in old {
        tell_senders(
            conn,
            &handle,
            &format!(
                "[chappe] Not delivered: session {handle} was not woken within 2 hours (the machine is asleep, the service on it is not responding or cannot wake this session). The request stays in its queue; if it is urgent, write to another addressee."
            ),
            now,
        )?;
        conn.execute(
            "UPDATE wakes SET state = 'expired', note = 'not woken in 2 hours' WHERE id = ?1",
            [id],
        )?;
    }
    Ok(())
}

/// A session to wake, with what the host needs to do it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeJob {
    pub id: i64,
    pub handle: String,
    pub tool: String,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub transcript: Option<String>,
    /// The waiting messages, each under its fixed header.
    pub text: String,
    pub message_ids: Vec<i64>,
}

/// How the host reports a wake.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Finish {
    /// The session was woken and has the messages.
    Delivered { message_ids: Vec<i64> },
    /// Not now: the session is busy, waits for the owner, or may be open elsewhere.
    Deferred { reason: String },
    /// This machine cannot wake sessions of this tool.
    Unsupported { reason: String },
    /// The attempt failed.
    Failed { reason: String },
}

fn session_of(conn: &Connection, handle: &str) -> rusqlite::Result<Option<Session>> {
    load(conn, handle)
}

impl Store {
    /// Notes that the host daemon of `machine` is up (tests; a poll does it too).
    #[cfg(test)]
    pub(crate) fn host_is_up(&self, machine: &str, now: i64) {
        host_seen(&self.conn(), machine, now).unwrap();
    }

    /// What the host daemon of a machine says about processes: per tool, the ids of
    /// the sessions that have a live process there. A session of that machine and
    /// tool that is not on the list is marked as having no process (the first time
    /// only); one on the list is cleared. A tool the host did not list is left
    /// alone, and so is a session that never told its session id. Together with two
    /// hours without a signal this makes a session `gone`.
    pub fn note_processes(
        &self,
        host: &Agent,
        running: &std::collections::HashMap<String, Vec<String>>,
        now: i64,
    ) -> crate::store::Result<()> {
        let conn = self.conn();
        for (tool, ids) in running {
            let alive: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
            let mut stmt = conn.prepare(
                "SELECT handle, session_id, process_gone_at FROM sessions
                 WHERE machine = ?1 AND tool = ?2 AND left_at IS NULL AND session_id IS NOT NULL
                   AND last_seen > ?3",
            )?;
            let rows = stmt
                .query_map(
                    params![host.machine, tool, now - crate::registry::GONE_AFTER_SECS],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                        ))
                    },
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);
            for (handle, session_id, gone_at) in rows {
                match (alive.contains(session_id.as_str()), gone_at) {
                    (true, Some(_)) => {
                        conn.execute(
                            "UPDATE sessions SET process_gone_at = NULL WHERE handle = ?1",
                            [&handle],
                        )?;
                    }
                    (false, None) => {
                        conn.execute(
                            "UPDATE sessions SET process_gone_at = ?2 WHERE handle = ?1",
                            params![handle, now],
                        )?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// When the host daemon of `machine` last asked for work, if it ever did.
    pub fn host_last_poll(&self, machine: &str) -> crate::store::Result<Option<i64>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT last_poll FROM hosts WHERE machine = ?1",
                [machine],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Seconds without a poll after which a host is taken to be down.
    pub const HOST_ALIVE_SECS: i64 = HOST_ALIVE;

    /// The wakes the host of a machine should carry out now. A claimed wake is
    /// leased for ten minutes; the host reports with [`Store::finish_wake`].
    pub fn claim_wakes(&self, host: &Agent, now: i64) -> crate::store::Result<Vec<WakeJob>> {
        let conn = self.conn();
        host_seen(&conn, &host.machine, now)?;
        expire_wakes(&conn, now)?;
        request_missed_wakes(&conn, &host.machine, now)?;
        let mut stmt = conn.prepare(
            "SELECT w.id, w.handle FROM wakes w JOIN sessions s ON s.handle = w.handle
             WHERE w.state IN ('pending', 'claimed') AND w.not_before <= ?1 AND s.machine = ?2
             ORDER BY w.id LIMIT 8",
        )?;
        let due = stmt
            .query_map(params![now, host.machine], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(stmt);
        let mut jobs = Vec::new();
        for (id, handle) in due {
            let Some(session) = session_of(&conn, &handle)?.filter(|s| !s.ended(now)) else {
                tell_senders(
                    &conn,
                    &handle,
                    &format!("[chappe] Not delivered: session {handle} has already ended."),
                    now,
                )?;
                conn.execute(
                    "UPDATE wakes SET state = 'expired', note = 'the session ended' WHERE id = ?1",
                    [id],
                )?;
                continue;
            };
            expire_queue(&conn, &handle, now)?;
            let mut waiting = queued_for(&conn, &handle)?;
            if waiting.is_empty() {
                conn.execute("UPDATE wakes SET state = 'done', note = 'nothing left to deliver' WHERE id = ?1", [id])?;
                continue;
            }
            for item in &mut waiting {
                item.rendered = render(item);
            }
            conn.execute(
                "UPDATE wakes SET state = 'claimed', not_before = ?2 WHERE id = ?1",
                params![id, now + LEASE],
            )?;
            jobs.push(job(id, &session, &waiting));
        }
        Ok(jobs)
    }

    /// The outcome of a wake, from the host of the session's machine.
    pub fn finish_wake(
        &self,
        host: &Agent,
        id: i64,
        finish: Finish,
        now: i64,
    ) -> Result<(), SessionError> {
        let conn = self.conn();
        let wake: Option<(String, String, i64)> = conn
            .query_row(
                "SELECT handle, state, attempts FROM wakes WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((handle, state, attempts)) = wake else {
            return Err(SessionError::NotFound);
        };
        let session = session_of(&conn, &handle)?.ok_or(SessionError::NotFound)?;
        if session.machine != host.machine {
            return Err(SessionError::Forbidden);
        }
        if state != "claimed" {
            // Reported twice, or too late: nothing more to do.
            return Ok(());
        }
        let retry = |attempts: i64, note: &str| -> rusqlite::Result<()> {
            let backoff = (60 * attempts).min(300);
            conn.execute(
                "UPDATE wakes SET state = 'pending', attempts = ?2, not_before = ?3, note = ?4 WHERE id = ?1",
                params![id, attempts, now + backoff, note],
            )?;
            Ok(())
        };
        match finish {
            Finish::Delivered { message_ids } => {
                for m in message_ids {
                    conn.execute(
                        "UPDATE deliveries SET state = 'delivered', delivered_at = ?3
                         WHERE message_id = ?1 AND handle = ?2 AND state = 'queued'",
                        params![m, handle, now],
                    )?;
                }
                conn.execute(
                    "UPDATE wakes SET state = 'done', note = 'woken' WHERE id = ?1",
                    [id],
                )?;
            }
            Finish::Deferred { reason } => retry(attempts + 1, &reason)?,
            Finish::Failed { reason } => {
                if attempts + 1 >= MAX_FAILURES {
                    tell_senders(
                        &conn,
                        &handle,
                        &format!(
                            "[chappe] Not delivered: session {handle} could not be woken ({reason}). The request stays in its queue."
                        ),
                        now,
                    )?;
                    conn.execute("UPDATE wakes SET state = 'expired', attempts = ?2, note = ?3 WHERE id = ?1", params![id, attempts + 1, reason])?;
                } else {
                    retry(attempts + 1, &reason)?;
                }
            }
            Finish::Unsupported { reason } => {
                tell_senders(
                    &conn,
                    &handle,
                    &format!(
                        "[chappe] Not woken: the machine of session {handle} has no way to wake sessions of its tool ({reason}). The request is in its queue: it will see it at the nearest checkpoint."
                    ),
                    now,
                )?;
                conn.execute(
                    "UPDATE wakes SET state = 'expired', note = ?2 WHERE id = ?1",
                    params![id, reason],
                )?;
            }
        }
        Ok(())
    }
}

fn job(id: i64, session: &Session, waiting: &[InboxItem]) -> WakeJob {
    WakeJob {
        id,
        handle: session.handle.clone(),
        tool: session.tool.clone(),
        session_id: session.session_id.clone(),
        worktree: session.worktree.clone(),
        transcript: session.transcript.clone(),
        text: waiting
            .iter()
            .map(|i| i.rendered.as_str())
            .collect::<Vec<_>>()
            .join(SEPARATOR),
        message_ids: waiting.iter().map(|i| i.id).collect(),
    }
}

#[cfg(test)]
mod tests;
