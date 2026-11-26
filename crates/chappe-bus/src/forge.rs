//! Events from the forge (Gitea, GitHub, GitLab): leases on shared resources, a
//! red `main`, releases and the digest of the day.
//!
//! The forge stays the record; the service reads it and shows it. A webhook says
//! that something happened; the service stores what the page needs (the leases
//! in force, the merged pull requests, the state of `main`) and writes a few
//! lines to the `events` topic of the project. Agents never receive these
//! lines. Merged pull requests and claimed issues are not written one by one:
//! they are counted, and one digest per project per day sums them up.
//!
//! A lease is a comment in the issue of a resource that begins with
//! `<!-- resource-lease: using|exclusive|released -->`, followed by `key: value`
//! lines (`agent`, `action`, `result`, `starts`, `until`), as the `AGENTS.md`
//! of the project says. A lease past its `until` is not in force.

use std::collections::HashMap;

use axum::http::HeaderMap;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::message::{FORGE, day_of, format_date};
use crate::store::{Result, Store};

mod lease;
pub use lease::*;

/// The topic of every project where the lines from the forge are written.
pub const EVENTS: &str = "events";

mod gitea;
mod github;
mod gitlab;
mod hubstyle;

pub use gitea::Gitea;
pub use github::GitHub;
pub use gitlab::GitLab;

/// The forges the service can take webhooks from; each is served at `/hooks/<name>`.
pub fn forges() -> [&'static dyn Forge; 3] {
    [&Gitea, &GitHub, &GitLab]
}

// ------------------------------------------------------------------- forges

/// One forge: how to trust its webhooks and how to read them.
///
/// An adapter turns what a forge sends into [`ForgeEvent`]s; everything after that
/// is the same for every forge.
pub trait Forge: Sync {
    /// `gitea`, `github`, `gitlab`: the path of the webhook is `/hooks/<name>`.
    fn name(&self) -> &'static str;
    /// Whether the request comes from the forge that knows `secret`.
    fn verify(&self, secret: &str, headers: &HeaderMap, body: &[u8]) -> bool;
    /// The unique id of the delivery and the name of the event, from the headers.
    fn delivery(&self, headers: &HeaderMap) -> Option<Delivery>;
    /// The event in the terms of the service; `None` for what the service does not use.
    fn parse(&self, event: &str, payload: &Value) -> Option<ForgeEvent>;
}

/// Which delivery of which event a webhook is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// Unique per delivery: a delivery seen before changes nothing.
    pub id: String,
    pub event: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentAction {
    Created,
    Edited,
    Deleted,
}

/// How a run on the default branch ended. Cancelled, skipped and unfinished runs
/// say nothing about the branch and are `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Passed,
    Failed,
    Other,
}

/// A comment on an issue, a pull request or a merge request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Comment {
    /// `owner/name`.
    pub repo: String,
    pub issue: i64,
    pub title: String,
    pub comment_id: i64,
    pub action: CommentAction,
    pub body: String,
}

/// A merged pull request (merge request).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Merged {
    pub repo: String,
    pub number: Option<i64>,
    /// The branch the pull request came from.
    pub branch: Option<String>,
}

/// A published release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub repo: String,
    pub tag: String,
    pub url: String,
}

/// A finished run of a pipeline or a workflow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub repo: String,
    pub branch: String,
    pub default_branch: String,
    pub outcome: RunOutcome,
    /// What tells one workflow of the repository from another.
    pub workflow: String,
    pub title: Option<String>,
    pub url: String,
}

/// What a webhook says, in the terms of the service and not of one forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeEvent {
    Comment(Comment),
    Merged(Merged),
    Release(Release),
    Run(Run),
}

impl ForgeEvent {
    pub fn repo(&self) -> &str {
        match self {
            ForgeEvent::Comment(e) => &e.repo,
            ForgeEvent::Merged(e) => &e.repo,
            ForgeEvent::Release(e) => &e.repo,
            ForgeEvent::Run(e) => &e.repo,
        }
    }
}
/// A lease without `until` holds for this long.
const DEFAULT_LEASE_SECS: i64 = 2 * 3600;
const KEEP_DELIVERIES_SECS: i64 = 7 * 24 * 3600;
/// A lease stays this long after it ran out (a release finds its lease in it).
const KEEP_LEASES_SECS: i64 = 30 * 24 * 3600;
const KEEP_ACTIVITY_SECS: i64 = 60 * 24 * 3600;
/// Digests are written for days up to this long ago.
const DIGEST_DAYS: i64 = 7;
const MAX_LINE_CHARS: usize = 560;
/// Pull requests named in a digest.
const DIGEST_LISTED: usize = 8;

// ---------------------------------------------------------------- signature

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether two byte strings are equal, without an early exit on the first
/// difference.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A header as text.
pub(crate) fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Whether `signature` (hex, with or without a `sha256=` prefix, as Gitea and
/// GitHub send it) is the HMAC-SHA256 of `body` under `secret`.
pub fn signature_matches(secret: &str, body: &[u8], signature: &str) -> bool {
    let expected = hex(&hmac_sha256(secret.as_bytes(), body));
    let given = signature
        .trim()
        .strip_prefix("sha256=")
        .unwrap_or(signature.trim());
    constant_time_eq(expected.as_bytes(), given.as_bytes())
}

// ------------------------------------------------------------------ helpers

/// The project of a repository: its name, lowercase, other characters as `-`.
pub fn project_of(full_name: &str) -> Option<String> {
    let name = full_name.rsplit('/').next()?.to_lowercase();
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name: String = name.trim_matches('-').chars().take(32).collect();
    let name = name.trim_end_matches('-').to_string();
    (!name.is_empty()).then_some(name)
}

/// One line for the page: no control characters, not too long.
fn line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > MAX_LINE_CHARS {
        let cut: String = flat.chars().take(MAX_LINE_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

fn ensure_events_topic(conn: &Connection, project: &str) -> rusqlite::Result<()> {
    crate::chat::ensure_project(conn, project)
}

/// Writes a line to the `events` topic of `project`. It is nobody's message:
/// it has no receiver and counts toward no noise number.
fn post_event(conn: &Connection, project: &str, text: &str, now: i64) -> rusqlite::Result<()> {
    ensure_events_topic(conn, project)?;
    conn.execute(
        "INSERT INTO messages (project, topic, sender, kind, text, created_at)
         VALUES (?1, ?2, ?3, 'notice', ?4, ?5)",
        params![project, EVENTS, FORGE, line(text), now],
    )?;
    Ok(())
}

// ------------------------------------------------------------------- events

/// Pages to refresh after an event: `msg:<project>/events`, `board`.
pub type Refresh = Vec<String>;

impl Store {
    /// Takes an event of a forge. `delivery` is the unique id of the webhook: a
    /// delivery seen before (forges redeliver) changes nothing.
    pub fn forge_event(
        &self,
        delivery: &str,
        event: Option<&ForgeEvent>,
        now: i64,
    ) -> Result<Refresh> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM forge_deliveries WHERE at < ?1",
            [now - KEEP_DELIVERIES_SECS],
        )?;
        conn.execute(
            "DELETE FROM leases WHERE until < ?1",
            [now - KEEP_LEASES_SECS],
        )?;
        conn.execute("DELETE FROM claims WHERE at < ?1", [now - KEEP_LEASES_SECS])?;
        conn.execute(
            "DELETE FROM activity WHERE at < ?1",
            [now - KEEP_ACTIVITY_SECS],
        )?;
        let fresh = conn.execute(
            "INSERT OR IGNORE INTO forge_deliveries (id, at) VALUES (?1, ?2)",
            params![delivery, now],
        )?;
        if fresh == 0 {
            return Ok(vec![]);
        }
        let Some(event) = event else {
            return Ok(vec![]);
        };
        let Some(project) = project_of(event.repo()) else {
            return Ok(vec![]);
        };
        let mut out = Posted::default();
        match event {
            ForgeEvent::Comment(e) => issue_comment(&conn, e, &project, now, &mut out)?,
            ForgeEvent::Merged(e) => pull_request(&conn, e, &project, now, &mut out)?,
            ForgeEvent::Release(e) => release(&conn, e, &project, now, &mut out)?,
            ForgeEvent::Run(e) => workflow_run(&conn, e, &project, now, &mut out)?,
        }
        Ok(out.refresh())
    }

    /// The leases in force at `now`: the last record of an agent in a resource
    /// issue, if it is not a release and has not run out.
    pub fn leases(&self, now: i64) -> Result<Vec<Lease>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT l.repo, l.issue, l.title, l.kind, l.agent, l.action, l.until FROM leases l
             WHERE l.kind <> 'released' AND l.until > ?1
               AND l.comment_id = (SELECT MAX(o.comment_id) FROM leases o
                                   WHERE o.repo = l.repo AND o.issue = l.issue AND o.agent = l.agent)
             ORDER BY l.until, l.repo, l.issue",
        )?;
        let rows = stmt.query_map([now], |r| {
            let kind: String = r.get(3)?;
            Ok(Lease {
                repo: r.get(0)?,
                issue: r.get(1)?,
                title: r.get(2)?,
                kind: LeaseKind::parse(&kind).unwrap_or(LeaseKind::Using),
                agent: r.get(4)?,
                action: r.get(5)?,
                until: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The claims of issues that are held or whose pull request was merged, oldest first.
    pub fn claims(&self) -> Result<Vec<Claim>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT repo, issue, title, worktree, branch, state = 'merged' FROM claims
             WHERE state IN ('claimed', 'merged') ORDER BY at, comment_id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Claim {
                repo: r.get(0)?,
                issue: r.get(1)?,
                title: r.get(2)?,
                worktree: r.get(3)?,
                branch: r.get(4)?,
                merged: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Writes the digest of every finished day that has activity and no digest
    /// yet, one line per project and day. Returns the projects that got one.
    pub fn post_digests(&self, now: i64) -> Result<Refresh> {
        let conn = self.conn();
        let today = day_of(now);
        let mut stmt = conn.prepare(
            "SELECT DISTINCT a.project, a.at / 86400 FROM activity a
             WHERE a.at / 86400 < ?1 AND a.at / 86400 >= ?2
               AND NOT EXISTS (SELECT 1 FROM digests d WHERE d.project = a.project AND d.day = a.at / 86400)
             ORDER BY 2, 1",
        )?;
        let due: Vec<(String, i64)> = stmt
            .query_map(params![today, today - DIGEST_DAYS], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut out = Posted::default();
        for (project, day) in due {
            let merged = activity_refs(&conn, &project, day, "merged")?;
            let claimed = activity_refs(&conn, &project, day, "claimed")?.len();
            let mut parts = Vec::new();
            if !merged.is_empty() {
                let listed: Vec<String> = merged
                    .iter()
                    .take(DIGEST_LISTED)
                    .map(|n| format!("#{n}"))
                    .collect();
                let more = merged.len().saturating_sub(DIGEST_LISTED);
                let more = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                parts.push(format!(
                    "PRs merged — {} ({}{more})",
                    merged.len(),
                    listed.join(", ")
                ));
            }
            if claimed > 0 {
                parts.push(format!("issues claimed — {claimed}"));
            }
            let text = format!(
                "Summary for {}: {}.",
                format_date(day * 86_400),
                parts.join("; ")
            );
            // Stamped at the end of that day, so that it sits among the day's lines.
            post_event(&conn, &project, &text, (day + 1) * 86_400 - 1)?;
            conn.execute(
                "INSERT INTO digests (project, day) VALUES (?1, ?2)",
                params![project, day],
            )?;
            out.add(&project);
        }
        Ok(out.refresh())
    }
}

/// Merged pull requests and wakes of the last day, by project, for the noise
/// numbers of the board.
pub(crate) fn activity_counts(
    conn: &Connection,
    now: i64,
) -> rusqlite::Result<HashMap<String, (i64, i64)>> {
    let since = now - 86_400;
    let mut out: HashMap<String, (i64, i64)> = HashMap::new();
    let mut stmt = conn.prepare(
        "SELECT project, COUNT(*) FROM activity WHERE kind = 'merged' AND at > ?1 GROUP BY project",
    )?;
    for row in stmt.query_map([since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (project, merged) = row?;
        out.entry(project).or_default().0 = merged;
    }
    let mut stmt = conn.prepare(
        "SELECT s.project, COUNT(*) FROM wakes w JOIN sessions s ON s.handle = w.handle
         WHERE w.created_at > ?1 GROUP BY s.project",
    )?;
    for row in stmt.query_map([since], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
    })? {
        let (project, wakes) = row?;
        out.entry(project).or_default().1 = wakes;
    }
    Ok(out)
}

/// The projects whose `events` topic got a line.
#[derive(Default)]
struct Posted {
    projects: Vec<String>,
    board: bool,
}

impl Posted {
    fn add(&mut self, project: &str) {
        if !self.projects.iter().any(|p| p == project) {
            self.projects.push(project.to_string());
        }
    }

    fn refresh(self) -> Refresh {
        let mut out: Refresh = self
            .projects
            .iter()
            .map(|p| format!("msg:{p}/{EVENTS}"))
            .collect();
        if self.board {
            out.push("board".into());
        }
        out
    }
}

fn activity_refs(
    conn: &Connection,
    project: &str,
    day: i64,
    kind: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT ref FROM activity WHERE project = ?1 AND kind = ?2 AND at / 86400 = ?3
         ORDER BY CAST(ref AS INTEGER)",
    )?;
    let rows = stmt.query_map(params![project, kind, day], |r| r.get(0))?;
    rows.collect()
}

fn count_activity(
    conn: &Connection,
    kind: &str,
    repo: &str,
    reference: &str,
    project: &str,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO activity (kind, repo, ref, project, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![kind, repo, reference, project, now],
    )?;
    Ok(())
}

fn issue_comment(
    conn: &Connection,
    comment: &Comment,
    project: &str,
    now: i64,
    out: &mut Posted,
) -> Result<()> {
    let (repo, comment_id, issue) = (comment.repo.as_str(), comment.comment_id, comment.issue);
    if comment.action == CommentAction::Deleted {
        conn.execute(
            "DELETE FROM leases WHERE repo = ?1 AND comment_id = ?2",
            params![repo, comment_id],
        )?;
        conn.execute(
            "DELETE FROM claims WHERE repo = ?1 AND comment_id = ?2",
            params![repo, comment_id],
        )?;
        out.board = true;
        return Ok(());
    }
    let body = comment.body.as_str();
    if let Some(claim) = parse_claim(body) {
        record_claim(conn, comment, &claim, now)?;
        out.board = true;
    }
    if comment.action == CommentAction::Created
        && body
            .trim_start()
            .starts_with("<!-- agent-lock: claimed -->")
    {
        count_activity(conn, "claimed", repo, &comment_id.to_string(), project, now)?;
    }
    let Some(record) = parse_lease(body) else {
        return Ok(());
    };
    let title = comment.title.as_str();
    let until = record.until.unwrap_or_else(|| {
        record
            .starts
            .unwrap_or(now)
            .saturating_add(DEFAULT_LEASE_SECS)
    });
    let seen: Option<String> = conn
        .query_row(
            "SELECT kind FROM leases WHERE repo = ?1 AND comment_id = ?2",
            params![repo, comment_id],
            |r| r.get(0),
        )
        .optional()?;
    // What the agent held before this record, to tell a result from a plain release.
    let before: Option<String> = conn
        .query_row(
            "SELECT kind FROM leases WHERE repo = ?1 AND issue = ?2 AND agent = ?3 AND comment_id < ?4
             ORDER BY comment_id DESC LIMIT 1",
            params![repo, issue, record.agent, comment_id],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT INTO leases (repo, issue, comment_id, title, kind, agent, action, result, starts, until, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT (repo, comment_id) DO UPDATE SET
             title = ?4, kind = ?5, agent = ?6, action = ?7, result = ?8, starts = ?9, until = ?10",
        params![
            repo,
            issue,
            comment_id,
            title,
            record.kind.as_str(),
            record.agent,
            record.action,
            record.result,
            record.starts,
            until,
            now
        ],
    )?;
    out.board = true;
    if seen.is_some() {
        // An edit of a record that was already announced is not announced again.
        return Ok(());
    }
    let url = crate::links::issue_url(repo, issue).unwrap_or_default();
    let resource = if title.is_empty() {
        format!("{repo}#{issue}")
    } else {
        format!("{title} ({repo}#{issue})")
    };
    let until_text = crate::message::format_hm(until);
    let (text, to_all) = match record.kind {
        LeaseKind::Exclusive => (
            format!(
                "Lease taken: {resource}. {} is changing the resource: {}. Until {until_text} UTC. {url}",
                record.agent,
                record.action.as_deref().unwrap_or("action not specified")
            ),
            true,
        ),
        LeaseKind::Using => (
            format!(
                "Resource in use: {resource}. {}: {}. Until {until_text} UTC. {url}",
                record.agent,
                record.action.as_deref().unwrap_or("action not specified")
            ),
            false,
        ),
        LeaseKind::Released => (
            format!(
                "Lease released: {resource}. {}. Result: {}. {url}",
                record.agent,
                record.result.as_deref().unwrap_or("not specified")
            ),
            before.as_deref() == Some("exclusive"),
        ),
    };
    post_event(conn, project, &text, now)?;
    out.add(project);
    if to_all && project != "all" {
        post_event(conn, "all", &text, now)?;
        out.add("all");
    }
    Ok(())
}

fn pull_request(
    conn: &Connection,
    merged: &Merged,
    project: &str,
    now: i64,
    out: &mut Posted,
) -> Result<()> {
    let repo = merged.repo.as_str();
    if let Some(number) = merged.number {
        count_activity(conn, "merged", repo, &number.to_string(), project, now)?;
    }
    // The work of the branch is done: the board says so next to its issue.
    if let Some(branch) = merged.branch.as_deref() {
        let changed = conn.execute(
            "UPDATE claims SET state = 'merged' WHERE repo = ?1 AND branch = ?2 AND state = 'claimed'",
            params![repo, branch],
        )?;
        out.board |= changed > 0;
    }
    Ok(())
}

fn release(
    conn: &Connection,
    release: &Release,
    project: &str,
    now: i64,
    out: &mut Posted,
) -> Result<()> {
    post_event(
        conn,
        project,
        &format!(
            "Release published: {} {}. {}",
            release.repo, release.tag, release.url
        ),
        now,
    )?;
    out.add(project);
    Ok(())
}

fn workflow_run(
    conn: &Connection,
    run: &Run,
    project: &str,
    now: i64,
    out: &mut Posted,
) -> Result<()> {
    let (repo, main) = (run.repo.as_str(), run.default_branch.as_str());
    if run.branch != run.default_branch {
        return Ok(());
    }
    let red = match run.outcome {
        RunOutcome::Passed => false,
        RunOutcome::Failed => true,
        // Cancelled, skipped: says nothing about `main`.
        RunOutcome::Other => return Ok(()),
    };
    let workflow = run.workflow.as_str();
    let was_red: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM main_state WHERE repo = ?1 AND red = 1)",
        [repo],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT INTO main_state (repo, workflow, red, since) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (repo, workflow) DO UPDATE SET red = ?3, since = ?4",
        params![repo, workflow, red, now],
    )?;
    let is_red: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM main_state WHERE repo = ?1 AND red = 1)",
        [repo],
        |r| r.get(0),
    )?;
    if was_red == is_red {
        return Ok(());
    }
    let url = run.url.as_str();
    let text = if is_red {
        let title = run.title.as_deref().unwrap_or(workflow);
        format!("{main} turned red: {repo}, {title}. {url}")
    } else {
        format!("{main} is green again: {repo}. {url}")
    };
    post_event(conn, project, &text, now)?;
    out.add(project);
    Ok(())
}

/// Feeds a Gitea payload to a store as the webhook receiver would; for tests.
#[cfg(test)]
pub(crate) trait GiteaFeed {
    fn gitea_event(
        &self,
        event: &str,
        delivery: &str,
        payload: &Value,
        now: i64,
    ) -> Result<Refresh>;
}

#[cfg(test)]
impl GiteaFeed for Store {
    fn gitea_event(
        &self,
        event: &str,
        delivery: &str,
        payload: &Value,
        now: i64,
    ) -> Result<Refresh> {
        self.forge_event(delivery, Gitea.parse(event, payload).as_ref(), now)
    }
}

#[cfg(test)]
mod tests;
