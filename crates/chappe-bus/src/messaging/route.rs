//! Routing a message to a session or topic, and the budgets that keep the chat quiet.

use super::*;

/// Where a message goes and who gets it.
pub(super) struct Route {
    pub(super) project: String,
    pub(super) topic: String,
    pub(super) target: String,
    pub(super) to_handle: Option<String>,
    pub(super) answers: Option<i64>,
    pub(super) receivers: Vec<Session>,
}

pub(super) fn topic_exists(
    conn: &Connection,
    project: &str,
    topic: &str,
) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM topics WHERE project = ?1 AND name = ?2",
            params![project, topic],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(super) fn receiver_of(s: &Session, now: i64) -> Receiver {
    Receiver {
        handle: s.handle.clone(),
        agent: s.agent(),
        roles: s.roles.clone(),
        issue: s.issue.clone(),
        active: s.state(now) == State::Active,
    }
}

pub(super) fn route(
    conn: &Connection,
    kind: Kind,
    me: &Session,
    f: &Fields,
    now: i64,
) -> Result<Route> {
    let ensure_topic = |project: &str| -> Result<()> {
        if topic_exists(conn, project, &f.topic)? {
            Ok(())
        } else {
            invalid(format!(
                "no topic {} in {project}: make it with `chappe topic create`, or use general",
                f.topic
            ))
        }
    };
    match kind {
        Kind::Announce => {
            ensure_topic("all")?;
            let mut receivers = live_sessions(conn, Filter::default(), now)?;
            receivers.retain(|s| s.handle != me.handle);
            Ok(Route {
                project: "all".into(),
                topic: f.topic.clone(),
                target: "all".into(),
                to_handle: None,
                answers: None,
                receivers,
            })
        }
        Kind::Owner => {
            ensure_topic(&me.project)?;
            Ok(Route {
                project: me.project.clone(),
                topic: f.topic.clone(),
                target: OWNER.into(),
                to_handle: None,
                answers: None,
                receivers: vec![],
            })
        }
        Kind::Ask | Kind::Notice => {
            ensure_topic(&me.project)?;
            let to = f.to.as_deref().unwrap_or_default();
            let (target, receivers) = resolve(conn, kind, me, to, now)?;
            let to_handle = (receivers.len() == 1).then(|| receivers[0].handle.clone());
            Ok(Route {
                project: me.project.clone(),
                topic: f.topic.clone(),
                target,
                to_handle,
                answers: None,
                receivers,
            })
        }
        Kind::Answer => {
            let id = f.ask.unwrap_or_default();
            let ask = conn
                .query_row(
                    "SELECT kind, sender, to_handle, project, topic, reply_by FROM messages WHERE id = ?1",
                    [id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                        ))
                    },
                )
                .optional()?;
            let Some((ask_kind, asker, to_handle, project, topic, reply_by)) = ask else {
                return Err(SendError::Unavailable(format!("there is no message #{id}")));
            };
            if ask_kind != "ask" {
                return Err(SendError::Unavailable(format!(
                    "message #{id} is not an ask"
                )));
            }
            if to_handle.as_deref() != Some(me.handle.as_str()) {
                return Err(SendError::Unavailable(
                    "that ask was not addressed to you".into(),
                ));
            }
            let answered = conn
                .query_row(
                    "SELECT 1 FROM messages WHERE answers = ?1",
                    [id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if answered {
                return Err(SendError::Unavailable(
                    "that ask is already answered; one answer, final".into(),
                ));
            }
            if let Some(by) = reply_by.filter(|by| *by < now) {
                return Err(SendError::Unavailable(format!(
                    "the asker stopped waiting at {} UTC",
                    format_hm(by)
                )));
            }
            let asker_session = load(conn, &asker)?.filter(|s| !s.ended(now));
            let Some(asker_session) = asker_session else {
                return Err(SendError::Unavailable(
                    "the asker's session has ended".into(),
                ));
            };
            Ok(Route {
                project,
                topic,
                target: asker,
                to_handle: Some(asker_session.handle.clone()),
                answers: Some(id),
                receivers: vec![asker_session],
            })
        }
    }
}

/// `@role` (in the sender's project) or a handle, to the sessions that get it.
pub(super) fn resolve(
    conn: &Connection,
    kind: Kind,
    me: &Session,
    to: &str,
    now: i64,
) -> Result<(String, Vec<Session>)> {
    if let Some(role) = to.strip_prefix('@') {
        let valid = role.chars().next().is_some_and(|c| c.is_ascii_lowercase())
            && role
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !valid {
            return invalid("--to: @role (lowercase letters, digits, '-') or a session handle");
        }
        let mut holders = live_sessions(
            conn,
            Filter {
                project: Some(&me.project),
                role: Some(role),
            },
            now,
        )?;
        holders.retain(|s| s.handle != me.handle);
        if holders.is_empty() {
            return Err(SendError::Unavailable(format!(
                "nobody is working on @{role} in {}; nothing was posted",
                me.project
            )));
        }
        if kind == Kind::Ask {
            // Two sessions hold the role: the one with the latest signal gets the ask.
            holders.sort_by_key(|s| (Reverse(s.last_seen), s.handle.clone()));
            holders.truncate(1);
        }
        return Ok((format!("@{role}"), holders));
    }
    if let Some(project) = to.strip_prefix("project:") {
        // Something only the people of another project know: one session of it,
        // a working one before an idle one, the latest signal first.
        let mut there = live_sessions(
            conn,
            Filter {
                project: Some(project),
                role: None,
            },
            now,
        )?;
        there.retain(|s| s.handle != me.handle);
        if there.is_empty() {
            return Err(SendError::Unavailable(format!(
                "nobody is working in the project {project}; nothing was posted"
            )));
        }
        if kind == Kind::Ask {
            there.truncate(1);
        }
        return Ok((format!("project:{project}"), there));
    }
    if to == me.handle {
        return invalid("a message to yourself is not sent");
    }
    match load(conn, to)? {
        None => Err(SendError::Unavailable(format!("there is no session {to}"))),
        Some(s) if s.ended(now) => Err(SendError::Unavailable(format!(
            "{to} has ended; nothing was posted"
        ))),
        Some(s) => Ok((s.handle.clone(), vec![s])),
    }
}

pub(super) fn count(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> rusqlite::Result<i64> {
    conn.query_row(sql, args, |r| r.get(0))
}

/// The first budget or rule the message breaks: a short code for the noise
/// numbers and the reason for the agent.
pub(super) fn check_budgets(
    conn: &Connection,
    me: &Session,
    kind: Kind,
    r: &Route,
    text: &str,
    now: i64,
) -> rusqlite::Result<Option<(&'static str, String)>> {
    let sender = me.handle.as_str();
    let dup = count(
        conn,
        "SELECT COUNT(*) FROM messages WHERE sender = ?1 AND target = ?2 AND text = ?3 AND created_at > ?4",
        &[&sender, &r.target, &text, &(now - DUPLICATE_WINDOW)],
    )?;
    if dup > 0 {
        return Ok(Some((
            "duplicate",
            "the same text was already sent to the same receiver in the last 30 minutes".into(),
        )));
    }
    let hour_start = now - HOUR;
    let sent: i64 = count(
        conn,
        "SELECT COUNT(*) FROM messages WHERE sender = ?1 AND created_at > ?2",
        &[&sender, &hour_start],
    )?;
    if sent >= MAX_PER_HOUR {
        let oldest: i64 = count(
            conn,
            "SELECT MIN(created_at) FROM messages WHERE sender = ?1 AND created_at > ?2",
            &[&sender, &hour_start],
        )?;
        return Ok(Some((
            "hour",
            format!(
                "budget: {MAX_PER_HOUR} messages per hour per session, all used; the next slot opens at {} UTC",
                format_hm(oldest + HOUR)
            ),
        )));
    }
    if kind == Kind::Ask {
        let asks = count(
            conn,
            "SELECT COUNT(*) FROM messages WHERE sender = ?1 AND kind = 'ask' AND created_at > ?2",
            &[&sender, &hour_start],
        )?;
        if asks >= MAX_ASKS_PER_HOUR {
            return Ok(Some((
                "asks",
                format!(
                    "budget: {MAX_ASKS_PER_HOUR} asks per hour per session, all used; look at the board and the issue before asking"
                ),
            )));
        }
    }
    if kind == Kind::Announce {
        let announced = count(
            conn,
            "SELECT COUNT(*) FROM messages WHERE sender = ?1 AND kind = 'announce' AND project = ?2 AND topic = ?3 AND created_at > ?4",
            &[&sender, &r.project, &r.topic, &(now - DAY)],
        )?;
        if announced >= MAX_ANNOUNCE_PER_TOPIC {
            return Ok(Some((
                "announce",
                format!(
                    "budget: {MAX_ANNOUNCE_PER_TOPIC} announcements per topic: one when you start, one with the result"
                ),
            )));
        }
    }
    if let (Some(peer), true) = (
        &r.to_handle,
        matches!(kind, Kind::Ask | Kind::Answer | Kind::Notice),
    ) {
        let pair = count(
            conn,
            "SELECT COUNT(*) FROM messages
             WHERE project = ?1 AND topic = ?2 AND kind IN ('ask', 'answer', 'notice')
               AND ((sender = ?3 AND to_handle = ?4) OR (sender = ?4 AND to_handle = ?3))
               AND id > COALESCE((SELECT MAX(id) FROM messages WHERE project = ?1 AND topic = ?2 AND sender = 'owner'), 0)",
            &[&r.project, &r.topic, &sender, peer],
        )?;
        if pair >= MAX_PAIR {
            return Ok(Some((
                "pair",
                format!(
                    "budget: {MAX_PAIR} messages between you and {peer} in this topic without the owner; use `chappe owner` if you are stuck"
                ),
            )));
        }
    }
    Ok(None)
}
