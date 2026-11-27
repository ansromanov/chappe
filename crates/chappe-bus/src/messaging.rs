//! Messages between agents: the closed list of occasions, the fixed format,
//! the budgets, routing to a session, and delivery at checkpoints.
//!
//! This is the "Keeping the chat quiet" part of the design. The cheapest thing
//! for an agent to do is to say nothing: there is no way to post a greeting, an
//! acknowledgement or a progress report, a message is built from fields, and the
//! service refuses what is over a budget. Nothing here wakes a session; a
//! session reads its inbox at a checkpoint.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::fmt;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::chat::Message;
use crate::message::{
    GENERAL, Kind, MAX_TEXT_CHARS, OWNER, clean_topic_name, format_hm, looks_like_secret,
};
use crate::registry::{Filter, Session, SessionError, State, live_sessions, load, owned};
use crate::store::{Agent, Store, StoreError};
use crate::wakes::{WakeOutcome, decide_wake, expire_wakes};

mod inbox;
pub use inbox::*;

mod route;
use route::*;

mod compose;
use compose::*;

/// Messages per session per hour.
pub const MAX_PER_HOUR: i64 = 6;
/// Asks per session per hour.
pub const MAX_ASKS_PER_HOUR: i64 = 3;
/// Messages in one topic between two agents, counted from the last message of the owner.
pub const MAX_PAIR: i64 = 4;
/// Announcements per sender and topic in a day. The design counts them per
/// lease in the forge; until leases arrive (stage 4) this is the stand-in.
pub const MAX_ANNOUNCE_PER_TOPIC: i64 = 2;
const HOUR: i64 = 3600;
const DAY: i64 = 86_400;
/// The same text to the same receiver within this long is dropped.
const DUPLICATE_WINDOW: i64 = 30 * 60;
/// A session gets its inbox at most once in this long.
pub const INBOX_INTERVAL: i64 = 5 * 60;
/// A message nobody read for this long is no longer delivered.
const QUEUE_TTL: i64 = DAY;
const MIN_BY: i64 = 60;
const MAX_BY: i64 = DAY;

/// What an agent sends. The service builds the text from the fields.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outgoing {
    /// The sending session.
    pub handle: String,
    pub kind: String,
    /// `@role` (in the sender's project) or a session handle.
    pub to: Option<String>,
    /// The topic; `general` when not given.
    pub topic: Option<String>,
    pub what: Option<String>,
    pub why: Option<String>,
    pub expect: Option<String>,
    /// How long the sender will wait for an answer: `30m`, `2h`.
    pub by: Option<String>,
    pub link: Option<String>,
    /// The message the answer is for.
    pub ask: Option<i64>,
    pub result: Option<String>,
    /// `ask` only: the sender waits for the answer, so the answer may wake it.
    #[serde(default)]
    pub wait: bool,
    /// Check everything and say who would get it, but send nothing.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug)]
pub enum SendError {
    /// The request is wrong; the text says how.
    Invalid(String),
    /// A rule or a budget turned the message away.
    Refused(String),
    /// Nothing to send to, or the moment has passed.
    Unavailable(String),
    Session(SessionError),
    Store(StoreError),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Invalid(s) | SendError::Refused(s) | SendError::Unavailable(s) => {
                f.write_str(s)
            }
            SendError::Session(e) => write!(f, "{e}"),
            SendError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SendError {}

impl From<SessionError> for SendError {
    fn from(e: SessionError) -> Self {
        SendError::Session(e)
    }
}

impl From<rusqlite::Error> for SendError {
    fn from(e: rusqlite::Error) -> Self {
        SendError::Store(e.into())
    }
}

pub type Result<T> = std::result::Result<T, SendError>;

fn invalid<T>(why: impl Into<String>) -> Result<T> {
    Err(SendError::Invalid(why.into()))
}

/// A session that gets the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receiver {
    pub handle: String,
    pub agent: String,
    pub roles: Vec<String>,
    pub issue: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct Sent {
    /// `None` for a dry run.
    pub id: Option<i64>,
    pub project: String,
    pub topic: String,
    pub kind: Kind,
    pub text: String,
    pub receivers: Vec<Receiver>,
    /// Whether an idle receiver was asked to be woken (`ask` only).
    pub wake: Option<WakeOutcome>,
    pub dry_run: bool,
    /// For an ask that only looks: earlier asks about the same, with their answers.
    pub similar: Vec<Similar>,
}

/// An earlier ask and the answer it got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Similar {
    pub id: i64,
    pub question: String,
    pub answer: String,
}

/// The words of a text that carry its subject, cut to a stem: five characters
/// are enough to match "token" with "tokens".
fn stems(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.chars().count() < 4 {
            continue;
        }
        let stem: String = word.to_lowercase().chars().take(5).collect();
        if !out.contains(&stem) {
            out.push(stem);
        }
    }
    out
}

/// Earlier asks that share enough words with `what` and were answered with
/// something other than a refusal: the answer may be there already.
pub(crate) fn similar_asks(
    conn: &Connection,
    what: &str,
    now: i64,
) -> rusqlite::Result<Vec<Similar>> {
    let query = stems(what);
    let need = if query.len() < 4 {
        2.min(query.len())
    } else {
        query.len().div_ceil(3).max(2)
    };
    if need == 0 {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare(
        "SELECT q.id, q.text, a.text FROM messages q JOIN messages a ON a.answers = q.id
         WHERE q.kind = 'ask' AND q.created_at > ?1 ORDER BY q.id DESC LIMIT 300",
    )?;
    let rows = stmt
        .query_map([now - 180 * DAY], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut scored: Vec<(usize, Similar)> = Vec::new();
    for (id, question, answer) in rows {
        let said = answer.trim_start_matches("Answer:").trim();
        if said.to_lowercase().starts_with("can't") {
            continue;
        }
        // Only what was asked, not why or by when.
        let asked = question.lines().next().unwrap_or("");
        let theirs = stems(asked);
        let shared = query.iter().filter(|s| theirs.contains(s)).count();
        if shared >= need {
            let cut = |s: &str, n: usize| -> String {
                let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
                if flat.chars().count() > n {
                    format!("{}…", flat.chars().take(n).collect::<String>())
                } else {
                    flat
                }
            };
            scored.push((
                shared,
                Similar {
                    id,
                    question: cut(asked.trim_start_matches("Need:").trim(), 160),
                    answer: cut(said, 400),
                },
            ));
        }
    }
    scored.sort_by_key(|(shared, s)| (Reverse(*shared), Reverse(s.id)));
    Ok(scored.into_iter().take(3).map(|(_, s)| s).collect())
}

impl Store {
    /// Sends a message, or says why not. A dry run checks everything and names
    /// the receivers but stores nothing.
    pub fn send(&self, agent: &Agent, o: Outgoing, now: i64) -> Result<Sent> {
        let kind = Kind::parse(&o.kind)
            .ok_or_else(|| SendError::Invalid(format!("unknown kind {:?}", o.kind)))?;
        let conn = self.conn();
        let me = owned(&conn, agent, &o.handle)?;
        if me.ended(now) {
            return Err(SessionError::Gone.into());
        }
        let fields = Fields::clean(kind, &o)?;
        let (text, reply_by) = compose(kind, &fields, now);
        let length = text.chars().count();
        if length > MAX_TEXT_CHARS {
            return invalid(format!(
                "the message is {length} characters, the limit is {MAX_TEXT_CHARS}: shorten it or give a link"
            ));
        }
        let refuse = |code: &str, reason: String| -> Result<Sent> {
            if !o.dry_run {
                conn.execute(
                    "INSERT INTO rejections (at, sender, project, kind, reason) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![now, me.handle, me.project, kind.as_str(), code],
                )?;
            }
            Err(SendError::Refused(reason))
        };
        if looks_like_secret(&text) {
            return refuse(
                "secret",
                "the message looks like it holds a token or a private key and is not sent".into(),
            );
        }
        expire_wakes(&conn, now)?;
        let route = route(&conn, kind, &me, &fields, now)?;
        if let Some((code, reason)) = check_budgets(&conn, &me, kind, &route, &text, now)? {
            return refuse(code, reason);
        }
        // Does the receiver have to be woken? An ask wakes; an answer only if
        // the ask said --wait.
        let wakes = match kind {
            Kind::Ask => true,
            Kind::Answer => {
                let asked_to_wait: Option<i64> = conn
                    .query_row(
                        "SELECT wait FROM messages WHERE id = ?1",
                        [route.answers],
                        |r| r.get(0),
                    )
                    .optional()?;
                asked_to_wait == Some(1)
            }
            _ => false,
        };
        let receivers: Vec<Receiver> = route
            .receivers
            .iter()
            .map(|s| receiver_of(s, now))
            .collect();
        let wake = match (wakes, route.receivers.as_slice()) {
            (true, [one]) => Some(decide_wake(&conn, one, now, o.dry_run)?),
            _ => None,
        };
        if o.dry_run {
            let similar = match (kind, o.what.as_deref()) {
                (Kind::Ask, Some(what)) => similar_asks(&conn, what, now)?,
                _ => vec![],
            };
            return Ok(Sent {
                id: None,
                project: route.project,
                topic: route.topic,
                kind,
                text,
                receivers,
                wake,
                dry_run: true,
                similar,
            });
        }
        conn.execute(
            "INSERT INTO messages (project, topic, sender, kind, text, created_at, target, to_handle, reply_by, answers, from_project, wait, sender_name)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                route.project, route.topic, me.handle, kind.as_str(), text, now, route.target, route.to_handle,
                reply_by, route.answers, me.project, o.wait, me.name
            ],
        )?;
        let id = conn.last_insert_rowid();
        for s in &route.receivers {
            conn.execute(
                "INSERT INTO deliveries (message_id, handle, state) VALUES (?1, ?2, 'queued')",
                params![id, s.handle],
            )?;
        }
        // Sending is a sign of life.
        conn.execute(
            "UPDATE sessions SET last_seen = ?2, process_gone_at = NULL WHERE handle = ?1",
            params![me.handle, now],
        )?;
        Ok(Sent {
            id: Some(id),
            project: route.project,
            topic: route.topic,
            kind,
            text,
            receivers,
            wake,
            dry_run: false,
            similar: vec![],
        })
    }

    /// Queues a message of the owner's page for the sessions it concerns: the
    /// sessions of its project, or every session for the project `all`.
    pub fn deliver_owner_message(&self, message: &Message, now: i64) -> Result<usize> {
        let conn = self.conn();
        let filter = Filter {
            project: (message.project != "all").then_some(message.project.as_str()),
            role: None,
        };
        let sessions = live_sessions(&conn, filter, now).map_err(SendError::Session)?;
        for s in &sessions {
            conn.execute(
                "INSERT OR IGNORE INTO deliveries (message_id, handle, state) VALUES (?1, ?2, 'queued')",
                params![message.id, s.handle],
            )?;
        }
        Ok(sessions.len())
    }

    /// What is waiting for the session, all in one block, at most once in five
    /// minutes. Messages that were not read in time are dropped first. Notes of
    /// the service itself (an ask that could not be delivered) come at once.
    pub fn inbox(&self, agent: &Agent, handle: &str, now: i64) -> Result<Inbox> {
        let conn = self.conn();
        let me = owned(&conn, agent, handle)?;
        if me.ended(now) {
            return Err(SessionError::Gone.into());
        }
        conn.execute(
            "UPDATE sessions SET last_seen = ?2, process_gone_at = NULL WHERE handle = ?1",
            params![handle, now],
        )?;
        expire_wakes(&conn, now)?;
        expire_queue(&conn, handle, now)?;
        close_unanswered(&conn, handle, now)?;
        let notes = take_notes(&conn, handle, now)?;
        let mut waiting = queued_for(&conn, handle)?;
        if waiting.is_empty() {
            return Ok(Inbox {
                messages: vec![],
                waiting: 0,
                next_in_secs: None,
                notes,
            });
        }
        let last: Option<i64> = conn.query_row(
            "SELECT MAX(delivered_at) FROM deliveries WHERE handle = ?1",
            [handle],
            |r| r.get(0),
        )?;
        if let Some(last) = last.filter(|l| now - l < INBOX_INTERVAL) {
            return Ok(Inbox {
                messages: vec![],
                waiting: waiting.len(),
                next_in_secs: Some(last + INBOX_INTERVAL - now),
                notes,
            });
        }
        for item in &mut waiting {
            item.rendered = render(item);
            conn.execute(
                "UPDATE deliveries SET state = 'delivered', delivered_at = ?3 WHERE message_id = ?1 AND handle = ?2",
                params![item.id, handle, now],
            )?;
        }
        Ok(Inbox {
            messages: waiting,
            waiting: 0,
            next_in_secs: None,
            notes,
        })
    }

    /// How the asks of the last day were answered: how many, which share, and how
    /// long the answers took.
    pub fn answer_stats(&self, now: i64) -> Result<AnswerStats> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT (SELECT MIN(a.created_at) FROM messages a WHERE a.answers = m.id) - m.created_at
             FROM messages m WHERE m.kind = 'ask' AND m.from_project IS NOT NULL AND m.created_at > ?1",
        )?;
        let rows = stmt.query_map([now - DAY], |r| r.get::<_, Option<i64>>(0))?;
        let mut taken: Vec<i64> = Vec::new();
        let mut asks = 0;
        for row in rows {
            asks += 1;
            if let Some(secs) = row? {
                taken.push(secs.max(0));
            }
        }
        taken.sort_unstable();
        Ok(AnswerStats {
            asks,
            answered: taken.len() as i64,
            median_secs: percentile(&taken, 50),
            p90_secs: percentile(&taken, 90),
        })
    }

    /// The noise numbers per project over the last day, by the sender's project.
    pub fn counters(&self, now: i64) -> Result<HashMap<String, Counters>> {
        let conn = self.conn();
        let since = now - DAY;
        let mut out: HashMap<String, Counters> = HashMap::new();
        let mut stmt = conn.prepare(
            "SELECT m.from_project, COUNT(*), SUM(m.kind = 'ask'),
                    SUM(m.kind = 'ask' AND EXISTS (SELECT 1 FROM messages a WHERE a.answers = m.id))
             FROM messages m WHERE m.from_project IS NOT NULL AND m.created_at > ?1 GROUP BY m.from_project",
        )?;
        let rows = stmt.query_map([since], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (project, sent, asks, answered) = row?;
            let c = out.entry(project).or_default();
            (c.sent, c.asks, c.answered) = (sent, asks, answered);
        }
        let mut stmt = conn
            .prepare("SELECT project, COUNT(*) FROM rejections WHERE at > ?1 GROUP BY project")?;
        let rows = stmt.query_map([since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (project, rejected) = row?;
            out.entry(project).or_default().rejected = rejected;
        }
        for (project, (merged, wakes)) in crate::forge::activity_counts(&conn, now)? {
            let c = out.entry(project).or_default();
            (c.merged, c.wakes) = (merged, wakes);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
