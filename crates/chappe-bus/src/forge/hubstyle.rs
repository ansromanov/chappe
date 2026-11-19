//! The payloads of Gitea and GitHub share one shape (Gitea took it from GitHub),
//! so one reader serves both; the adapters differ in their headers.

use serde_json::Value;

use super::{Comment, CommentAction, ForgeEvent, Merged, Release, Run, RunOutcome};

fn text<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for key in path {
        cur = cur.get(key)?;
    }
    cur.as_str()
}

pub(super) fn parse(event: &str, payload: &Value) -> Option<ForgeEvent> {
    let repo = text(payload, &["repository", "full_name"])?.to_string();
    match event {
        "issue_comment" => comment(payload, repo),
        "pull_request" => merged(payload, repo),
        "release" => release(payload, repo),
        "workflow_run" => run(payload, repo),
        _ => None,
    }
}

fn comment(payload: &Value, repo: String) -> Option<ForgeEvent> {
    let action = match payload["action"].as_str()? {
        "created" => CommentAction::Created,
        "edited" => CommentAction::Edited,
        "deleted" => CommentAction::Deleted,
        _ => return None,
    };
    Some(ForgeEvent::Comment(Comment {
        repo,
        issue: payload["issue"]["number"].as_i64().unwrap_or(0),
        title: payload["issue"]["title"].as_str().unwrap_or("").to_string(),
        comment_id: payload["comment"]["id"].as_i64()?,
        action,
        body: payload["comment"]["body"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    }))
}

fn merged(payload: &Value, repo: String) -> Option<ForgeEvent> {
    let merged = payload["action"].as_str() == Some("closed")
        && payload["pull_request"]["merged"].as_bool() == Some(true);
    merged.then(|| {
        ForgeEvent::Merged(Merged {
            repo,
            number: payload["pull_request"]["number"].as_i64(),
            branch: text(payload, &["pull_request", "head", "ref"]).map(str::to_string),
        })
    })
}

fn release(payload: &Value, repo: String) -> Option<ForgeEvent> {
    let release = &payload["release"];
    if payload["action"].as_str() != Some("published") || release["draft"].as_bool() == Some(true) {
        return None;
    }
    Some(ForgeEvent::Release(Release {
        repo,
        tag: release["tag_name"].as_str().unwrap_or("?").to_string(),
        url: release["html_url"].as_str().unwrap_or("").to_string(),
    }))
}

fn run(payload: &Value, repo: String) -> Option<ForgeEvent> {
    let run = &payload["workflow_run"];
    let outcome = if run["status"].as_str() != Some("completed") {
        RunOutcome::Other
    } else {
        match run["conclusion"].as_str() {
            Some("success") => RunOutcome::Passed,
            Some("failure") | Some("timed_out") => RunOutcome::Failed,
            _ => RunOutcome::Other,
        }
    };
    Some(ForgeEvent::Run(Run {
        repo,
        branch: run["head_branch"].as_str()?.to_string(),
        default_branch: text(payload, &["repository", "default_branch"])
            .unwrap_or("main")
            .to_string(),
        outcome,
        workflow: run["path"]
            .as_str()
            .or_else(|| run["name"].as_str())
            .unwrap_or("workflow")
            .to_string(),
        title: run["display_title"].as_str().map(str::to_string),
        url: run["html_url"].as_str().unwrap_or("").to_string(),
    }))
}
