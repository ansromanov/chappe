//! Leases on shared resources and claims of issues, read from comments of the forge.

use super::*;

// ------------------------------------------------------------------- leases

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseKind {
    Using,
    Exclusive,
    Released,
}

impl LeaseKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            LeaseKind::Using => "using",
            LeaseKind::Exclusive => "exclusive",
            LeaseKind::Released => "released",
        }
    }

    pub(super) fn parse(s: &str) -> Option<LeaseKind> {
        match s {
            "using" => Some(LeaseKind::Using),
            "exclusive" => Some(LeaseKind::Exclusive),
            "released" => Some(LeaseKind::Released),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRecord {
    pub kind: LeaseKind,
    pub agent: String,
    pub action: Option<String>,
    pub result: Option<String>,
    pub starts: Option<i64>,
    pub until: Option<i64>,
}

/// Reads a comment as a lease record; `None` when it is not one.
pub fn parse_lease(body: &str) -> Option<LeaseRecord> {
    let mut lines = body
        .trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
        .lines();
    let marker = lines.next()?.trim();
    let kind = marker
        .strip_prefix("<!-- resource-lease:")?
        .strip_suffix("-->")?
        .trim();
    let kind = LeaseKind::parse(kind)?;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            if !key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                fields.entry(key).or_insert_with(|| value.trim());
            }
        }
    }
    let text = |key: &str| {
        fields
            .get(key)
            .filter(|v| !v.is_empty())
            .map(|v| (*v).to_string())
    };
    Some(LeaseRecord {
        kind,
        agent: text("agent")?,
        action: text("action"),
        result: text("result"),
        starts: text("starts").and_then(|t| parse_utc(&t)),
        until: text("until").and_then(|t| parse_utc(&t)),
    })
}

/// `2026-11-13 08:29 UTC` (or `2026-11-13T08:29Z`) as a Unix time.
pub fn parse_utc(text: &str) -> Option<i64> {
    let mut nums = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<i64>().ok());
    let (y, m, d) = (nums.next()??, nums.next()??, nums.next()??);
    let (h, min) = (nums.next()??, nums.next()??);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 || min > 59 || y < 1970 {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400 + h * 3600 + min * 60)
}

/// A calendar date to days since 1970-01-01 (Howard Hinnant's algorithm).
pub(super) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A lease in force, for the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lease {
    pub repo: String,
    pub issue: i64,
    pub title: String,
    pub kind: LeaseKind,
    pub agent: String,
    pub action: Option<String>,
    pub until: i64,
}

// ------------------------------------------------------------------- claims

/// An `agent-lock` comment of an issue: a claim or its release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimRecord {
    pub released: bool,
    pub agent: Option<String>,
    pub worktree: Option<String>,
    pub branch: Option<String>,
}

/// Reads a comment as a claim record; `None` when it is not one. The fields are
/// free-form list lines (`- branch: `x``, `branch: x`), as agents write them.
pub fn parse_claim(body: &str) -> Option<ClaimRecord> {
    let text = body.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace());
    let released = if text.starts_with("<!-- agent-lock: claimed -->") {
        false
    } else if text.starts_with("<!-- agent-lock: released -->") {
        true
    } else {
        return None;
    };
    let mut record = ClaimRecord {
        released,
        agent: None,
        worktree: None,
        branch: None,
    };
    for line in text.lines().skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key
            .trim_matches(|c: char| c.is_whitespace() || "-*•`".contains(c))
            .to_lowercase();
        let value = value
            .trim_matches(|c: char| c.is_whitespace() || "*`".contains(c))
            .to_string();
        if value.is_empty() || value.chars().count() > 300 || value.chars().any(char::is_control) {
            continue;
        }
        let slot = match key.as_str() {
            "agent" => &mut record.agent,
            "worktree" => &mut record.worktree,
            "branch" => &mut record.branch,
            _ => continue,
        };
        slot.get_or_insert(value);
    }
    Some(record)
}

/// The issue a branch or a worktree works on, for the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Claim {
    pub repo: String,
    pub issue: i64,
    pub title: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// The pull request of the branch was merged.
    pub merged: bool,
}

/// The latest claim that names the branch or the worktree of a session. The
/// default branches name no claim: every checkout is on them.
pub fn claim_of<'a>(
    branch: Option<&str>,
    worktree: Option<&str>,
    claims: &'a [Claim],
) -> Option<&'a Claim> {
    let branch = branch.filter(|b| !matches!(*b, "main" | "master" | "HEAD"));
    claims.iter().rev().find(|c| {
        (branch.is_some() && c.branch.as_deref() == branch)
            || (worktree.is_some() && c.worktree.as_deref() == worktree)
    })
}

pub(super) fn record_claim(
    conn: &Connection,
    comment: &Comment,
    record: &ClaimRecord,
    now: i64,
) -> rusqlite::Result<()> {
    let (repo, comment_id, issue) = (comment.repo.as_str(), comment.comment_id, comment.issue);
    if record.released {
        conn.execute(
            "UPDATE claims SET state = 'released' WHERE repo = ?1 AND issue = ?2 AND state = 'claimed'
               AND (?3 IS NULL OR agent IS NULL OR agent = ?3)",
            params![repo, issue, record.agent],
        )?;
        return Ok(());
    }
    let title = line(&comment.title);
    conn.execute(
        "INSERT INTO claims (repo, issue, comment_id, title, agent, worktree, branch, state, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'claimed', ?8)
         ON CONFLICT (repo, comment_id) DO UPDATE SET title = ?4, agent = ?5, worktree = ?6, branch = ?7",
        params![repo, issue, comment_id, title, record.agent, record.worktree, record.branch, now],
    )?;
    Ok(())
}
