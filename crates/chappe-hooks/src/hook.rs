//! `chappe hook claude <Event>`: what the hooks of Claude Code run.
//!
//! The hook gets the event as JSON on standard input. `SessionStart` registers
//! the session (when the project's settings name a project and roles in the
//! environment) and tells the agent its handle; `PostToolBatch` and
//! `UserPromptSubmit` hand the agent what is waiting for it, as extra context
//! before its next step. There is no hook for `SessionEnd` on purpose: a session
//! whose terminal was closed is not finished, it is idle, and a request for it
//! may wake it (`claude --resume`). It ends with `chappe leave`, which the
//! project's AGENTS.md asks for when the work is done, or after a day of silence.
//!
//! A hook must never get in the agent's way: whatever happens, it exits with 0
//! and, if it has nothing to say, prints nothing. (A hook that exits with 2
//! blocks the action it hooks.)

use std::io::{Read, Write};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::{Client, Env, Handle, InboxAnswer, InboxBody, RegisterBody, Reply, safe_name};
/// `PostToolBatch` fires after every round of tool calls: ask the service at
/// most this often.
const MIN_GAP: Duration = Duration::from_secs(20);

/// Runs one hook; the exit status is always 0.
pub fn run_hook(
    args: &[String],
    env: &Env,
    input: &mut dyn Read,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let [_, tool, event] = args else {
        let _ = writeln!(err, "chappe hook: usage: chappe hook claude <Event>");
        return 0;
    };
    if tool != "claude" {
        let _ = writeln!(err, "chappe hook: no hooks for {tool:?}");
        return 0;
    }
    let mut text = String::new();
    let _ = input.take(1 << 20).read_to_string(&mut text);
    let payload: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let Some(session_id) = payload["session_id"].as_str().filter(|s| safe_name(s)) else {
        return 0;
    };
    let Ok(token) = env.token() else { return 0 };
    let Ok(url) = env.url() else { return 0 };
    let client = Client::new(&url, token);

    let context = match event.as_str() {
        "SessionStart" => start(&client, env, session_id, &payload, err),
        "PostToolBatch" => inbox(&client, env, session_id, &payload, true),
        "UserPromptSubmit" => inbox(&client, env, session_id, &payload, false),
        other => {
            let _ = writeln!(err, "chappe hook: no handler for {other:?}");
            None
        }
    };
    if let Some(text) = context {
        let reply =
            json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}});
        let _ = writeln!(out, "{reply}");
    }
    0
}

/// Where a session belongs on the bus, and why.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Placement {
    pub project: String,
    pub roles: Vec<String>,
    /// What decided it, for `chappe doctor`.
    pub why: String,
}

/// The project and roles of a session started in `worktree`. A project's settings
/// name them (`CHAPPE_PROJECT` and `CHAPPE_ROLES`); failing that, a
/// machine-wide `CHAPPE_PROJECTS_ROOT` makes every project below it a project
/// of the bus, with the role `dev`, and `CHAPPE_PROJECT_MAP` gathers several
/// directories into one project. The error says why the session is not registered.
pub(crate) fn placement(env: &Env, worktree: Option<&str>) -> Result<Placement, String> {
    let (derived, derived_roles, why);
    let (project, roles_text) = match (env.get("CHAPPE_PROJECT"), env.get("CHAPPE_ROLES")) {
        (Some(project), Some(roles)) => {
            why = "CHAPPE_PROJECT and CHAPPE_ROLES".to_string();
            (project, roles)
        }
        _ => {
            let root = env.get("CHAPPE_PROJECTS_ROOT").ok_or(
                "CHAPPE_PROJECTS_ROOT is not set, and there is no CHAPPE_PROJECT with CHAPPE_ROLES",
            )?;
            let worktree = worktree.ok_or("the working directory is unknown")?;
            let dir = project_below_root(root, worktree).ok_or_else(|| {
                format!(
                    "{worktree} is not inside a project directory below {root} (the root itself and other places are not registered)"
                )
            })?;
            let map = env.get("CHAPPE_PROJECT_MAP").unwrap_or("");
            derived = map_project(&dir, map);
            // The area of the directory first (`wifi`), then `dev`, which every session has.
            derived_roles = match (env.get("CHAPPE_ROLES"), role_of(&dir, map)) {
                (Some(roles), _) => roles.to_string(),
                (None, Some(area)) => format!("{area},dev"),
                (None, None) => "dev".to_string(),
            };
            why = match map_entry(&dir, map) {
                Some((pattern, target)) => {
                    format!(
                        "directory {dir} below {root}, CHAPPE_PROJECT_MAP entry {pattern}={target}"
                    )
                }
                None => {
                    format!("directory {dir} below {root}, no CHAPPE_PROJECT_MAP entry matches it")
                }
            };
            (derived.as_str(), derived_roles.as_str())
        }
    };
    let roles: Vec<String> = roles_text
        .split(',')
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .collect();
    Ok(Placement {
        project: project.to_string(),
        roles,
        why,
    })
}

/// Registers the session, if the settings of the project say what it is.
fn start(
    client: &Client,
    env: &Env,
    session_id: &str,
    payload: &Value,
    err: &mut dyn Write,
) -> Option<String> {
    let worktree = payload["cwd"].as_str().or(env.cwd.as_deref());
    let Ok(place) = placement(env, worktree) else {
        return None;
    };
    let (project, roles) = (place.project.as_str(), place.roles);
    let name = env.session_name(Some(session_id));
    let branch = worktree.and_then(git_branch);
    let body = RegisterBody {
        roles: &roles,
        project,
        issue: env.get("CHAPPE_ISSUE"),
        branch: branch.as_deref(),
        worktree,
        session_id: Some(session_id),
        transcript: payload["transcript_path"].as_str(),
        name: name.as_deref(),
    };
    match client.post_json("/v1/sessions", &body) {
        Reply::Ok(reply) => {
            let registered = serde_json::from_str::<Handle>(&reply).ok()?;
            let handle = registered.handle;
            env.remember_handle(session_id, &handle);
            let briefing = registered
                .briefing
                .map_or(String::new(), |b| format!("\n{b}"));
            Some(format!(
                "[chappe] You are registered as {handle} (project {project}, roles {}). Requests from other agents will arrive here as a block before your next step. Do not send greetings, acknowledgements or progress reports; the `chappe` commands fill in your name themselves.{briefing}",
                roles.join(", ")
            ))
        }
        Reply::Refused { reason, .. } => {
            let _ = writeln!(err, "chappe hook: not registered: {reason}");
            None
        }
        Reply::Down(_) => None,
    }
}

/// The project a working directory below `root` belongs to: the first
/// directory below the root, or, for a worktree kept in `<root>/worktrees/`, the
/// repository it belongs to. Named as the service wants a project named.
fn project_below_root(root: &str, cwd: &str) -> Option<String> {
    let root = std::fs::canonicalize(root).ok()?;
    let cwd = std::fs::canonicalize(cwd).ok()?;
    let first = cwd.strip_prefix(&root).ok()?.components().next()?;
    let first = std::path::PathBuf::from(first.as_os_str());
    let dir = if first.as_os_str() == "worktrees" {
        // `git` knows the repository of a worktree: the parent of its common directory.
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&cwd)
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let common = std::path::PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
        let repo = common.parent()?.to_path_buf();
        let repo = std::fs::canonicalize(repo).ok()?;
        if repo.parent()? != root {
            return None;
        }
        repo.file_name()?.to_string_lossy().into_owned()
    } else {
        first.to_string_lossy().into_owned()
    };
    project_name(&dir)
}

/// The branch checked out in a directory; none outside a repository or on a detached head.
fn git_branch(dir: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let branch = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!branch.is_empty() && branch != "HEAD" && branch.len() <= 100).then_some(branch)
}

/// A directory name as the service wants a project named: lowercase letters,
/// digits and `-`, at most 32 characters. `None` when nothing is left.
fn project_name(dir: &str) -> Option<String> {
    let name: String = dir
        .to_lowercase()
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

/// The first entry of `CHAPPE_PROJECT_MAP` (`demo-*=demo,lab=demo`)
/// that matches the name of a directory: its pattern and the project it names.
/// A pattern ending in `*` is a prefix; anything else must match whole. An
/// entry whose target is not a project name is skipped.
fn map_entry<'a>(name: &str, map: &'a str) -> Option<(&'a str, String)> {
    for entry in map.split(',') {
        let Some((pattern, target)) = entry.split_once('=') else {
            continue;
        };
        let (pattern, target) = (pattern.trim(), target.trim());
        let hit = match pattern.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == pattern,
        };
        if !hit || pattern.is_empty() {
            continue;
        }
        if let Some(target) = project_name(target).filter(|t| t == target) {
            return Some((pattern, target));
        }
    }
    None
}

/// The project a directory would be, with `CHAPPE_PROJECT_MAP` applied.
fn map_project(name: &str, map: &str) -> String {
    map_entry(name, map).map_or_else(|| name.to_string(), |(_, project)| project)
}

/// The area a directory stands for inside its project: for a directory that a
/// prefix pattern gathers (`demo-*`), what follows the prefix (`demo-wifi`
/// is `wifi`), as a role name; none otherwise.
fn role_of(name: &str, map: &str) -> Option<String> {
    let (pattern, _) = map_entry(name, map)?;
    let rest = name.strip_prefix(pattern.strip_suffix('*')?)?;
    let role: String = rest.trim_matches('-').chars().take(24).collect();
    let role = role.trim_end_matches('-').to_string();
    let valid = role.starts_with(|c: char| c.is_ascii_lowercase())
        && role
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    valid.then_some(role)
}

/// What is waiting for the session, as context for its next step.
fn inbox(
    client: &Client,
    env: &Env,
    session_id: &str,
    payload: &Value,
    rate_limited: bool,
) -> Option<String> {
    let handle = env.remembered_handle(session_id)?;
    if rate_limited && !due(env, session_id) {
        return None;
    }
    let name = env.session_name(Some(session_id));
    // The session moves between worktrees and branches as it works; the board
    // finds its issue by them.
    let cwd = payload["cwd"].as_str();
    let branch = cwd.and_then(git_branch);
    match client.post_json(
        "/v1/inbox",
        &InboxBody {
            handle: &handle,
            name: name.as_deref(),
            worktree: cwd,
            branch: branch.as_deref(),
        },
    ) {
        Reply::Ok(reply) => {
            let answer = serde_json::from_str::<InboxAnswer>(&reply).ok()?;
            let mut parts: Vec<&str> = answer.notes.iter().map(String::as_str).collect();
            parts.extend(answer.messages.iter().map(|m| m.rendered.as_str()));
            (!parts.is_empty()).then(|| parts.join("\n\n---\n\n"))
        }
        _ => None,
    }
}

/// Whether the service has not been asked for this session within [`MIN_GAP`];
/// notes the time of this ask.
fn due(env: &Env, session_id: &str) -> bool {
    let Some(dir) = env.handles_dir() else {
        return true;
    };
    let marker = dir.join(format!("{session_id}.checked"));
    let recent = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < MIN_GAP);
    if !recent {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(&marker, b"");
    }
    !recent
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("hook-root-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn at(root: &Path, sub: &str) -> Option<String> {
        project_below_root(root.to_str().unwrap(), root.join(sub).to_str().unwrap())
    }

    fn env_with(root: &Path, map: &str) -> Env {
        let mut vars = std::collections::HashMap::new();
        vars.insert(
            "CHAPPE_PROJECTS_ROOT".to_string(),
            root.display().to_string(),
        );
        if !map.is_empty() {
            vars.insert("CHAPPE_PROJECT_MAP".into(), map.into());
        }
        Env { vars, cwd: None }
    }

    #[test]
    fn the_mac_directory_lab_is_the_project_studio_os_when_the_map_says_so() {
        let root = dir("lab");
        for d in ["lab/src", "lab-perf", "demo-wifi"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let at_dir = |env: &Env, sub: &str| {
            placement(env, Some(root.join(sub).to_str().unwrap())).map(|p| (p.project, p.roles))
        };
        // Without the entry the sessions of the Mac land in a project of their own.
        let unmapped = env_with(&root, "demo-*=demo");
        assert_eq!(
            at_dir(&unmapped, "lab").unwrap(),
            ("lab".to_string(), vec!["dev".to_string()])
        );
        // The map of the machine: the directory itself, and the ones gathered by its prefix.
        let map = "demo-*=demo,lab=demo,lab-*=demo";
        let mapped = env_with(&root, map);
        for sub in ["lab", "lab/src"] {
            assert_eq!(
                at_dir(&mapped, sub).unwrap(),
                ("demo".to_string(), vec!["dev".to_string()]),
                "{sub}"
            );
        }
        assert_eq!(
            at_dir(&mapped, "lab-perf").unwrap(),
            ("demo".to_string(), vec!["perf".into(), "dev".into()])
        );
        // The doctor's explanation names the entry that decided it.
        let why = placement(&mapped, Some(root.join("lab").to_str().unwrap()))
            .unwrap()
            .why;
        assert!(why.contains("lab=demo"), "{why}");
        // Why a session is not registered, in words.
        let none = Env {
            vars: Default::default(),
            cwd: None,
        };
        assert!(
            placement(&none, Some("/x"))
                .unwrap_err()
                .contains("CHAPPE_PROJECTS_ROOT is not set")
        );
        assert!(
            placement(&mapped, Some("/elsewhere"))
                .unwrap_err()
                .contains("not inside a project directory")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_project_is_the_first_directory_below_the_root() {
        let root = dir("plain");
        for d in ["core/docs", "Shop_Frontend", "worktrees"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        assert_eq!(at(&root, "core").as_deref(), Some("core"));
        assert_eq!(at(&root, "core/docs").as_deref(), Some("core"));
        assert_eq!(
            at(&root, "Shop_Frontend").as_deref(),
            Some("shop-frontend"),
            "named as the service wants a project named"
        );
        // Outside the root, at the root itself, or a directory that is not there.
        assert_eq!(project_below_root(root.to_str().unwrap(), "/"), None);
        assert_eq!(
            project_below_root(root.to_str().unwrap(), root.to_str().unwrap()),
            None
        );
        assert_eq!(at(&root, "missing"), None);
        // A worktree that is not a repository belongs to no project.
        assert_eq!(at(&root, "worktrees"), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directories_can_be_gathered_into_one_project() {
        let map = "demo-*=demo, lab=demo,lab-*=demo,core-worktrees=core";
        assert_eq!(map_project("demo-wifi", map), "demo");
        assert_eq!(map_project("demo-iso-laptop", map), "demo");
        assert_eq!(map_project("lab-perf-test", map), "demo");
        assert_eq!(map_project("lab", map), "demo", "the Mac directory itself");
        assert_eq!(map_project("core-worktrees", map), "core");
        // A pattern without `*` is the whole name; a prefix needs its `-`.
        assert_eq!(map_project("core", map), "core");
        assert_eq!(map_project("core-worktrees-2", map), "core-worktrees-2");
        assert_eq!(map_project("demo", map), "demo");
        assert_eq!(map_project("demox", map), "demox");
        // The first entry that matches wins; nothing matching leaves the name alone.
        assert_eq!(map_project("a-b", "a-*=first,a-b=second"), "first");
        assert_eq!(map_project("shop-frontend", map), "shop-frontend");
        // Rubbish entries are skipped, not trusted.
        assert_eq!(
            map_project("x-1", "x-*=Bad Name,=y,novalue,x-*=good"),
            "good"
        );
        assert_eq!(map_project("x-1", ""), "x-1");
    }

    #[test]
    fn a_gathered_directory_gives_its_area_as_a_role() {
        let map = "demo-*=demo,lab=demo,lab-*=demo,core-worktrees=core,x-*=x";
        assert_eq!(role_of("demo-wifi", map).as_deref(), Some("wifi"));
        assert_eq!(
            role_of("demo-iso-laptop", map).as_deref(),
            Some("iso-laptop")
        );
        assert_eq!(role_of("lab-perf-test", map).as_deref(), Some("perf-test"));
        // Not gathered by a prefix, not gathered at all, or no usable name left: no area.
        assert_eq!(role_of("core-worktrees", map), None);
        assert_eq!(role_of("demo", map), None);
        assert_eq!(role_of("shop-frontend", map), None);
        assert_eq!(role_of("x-2024", map), None, "a role starts with a letter");
        assert_eq!(role_of("demo-wifi", ""), None);
        let long = format!("demo-{}", "a".repeat(40));
        assert_eq!(role_of(&long, map).unwrap().len(), 24);
    }

    #[test]
    fn a_worktree_belongs_to_the_repository_it_was_made_from() {
        let root = dir("git");
        let repo = root.join("core");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str], cwd: &Path| {
            Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .current_dir(cwd)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !git(&["init", "-q"], &repo)
            || !git(&["commit", "-q", "--allow-empty", "-m", "x"], &repo)
        {
            eprintln!("git is not available: skipped");
            return;
        }
        std::fs::create_dir_all(root.join("worktrees")).unwrap();
        let wt = root.join("worktrees/core-claude-names");
        assert!(git(
            &["worktree", "add", "-q", "-b", "b", wt.to_str().unwrap()],
            &repo
        ));
        assert_eq!(
            at(&root, "worktrees/core-claude-names").as_deref(),
            Some("core")
        );
        // A repository outside the root does not count.
        let other = dir("elsewhere");
        let foreign = other.join("proj");
        std::fs::create_dir_all(&foreign).unwrap();
        assert!(git(&["init", "-q"], &foreign));
        assert!(git(&["commit", "-q", "--allow-empty", "-m", "x"], &foreign));
        let wt2 = root.join("worktrees/foreign");
        assert!(git(
            &["worktree", "add", "-q", "-b", "c", wt2.to_str().unwrap()],
            &foreign
        ));
        assert_eq!(at(&root, "worktrees/foreign"), None);
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(other).unwrap();
    }
}
