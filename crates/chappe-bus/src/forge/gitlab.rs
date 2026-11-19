//! GitLab: `POST /hooks/gitlab`, with the secret token in `X-Gitlab-Token`.
//!
//! GitLab sends the secret itself, not a signature of the body, so anyone who can
//! read the request on the way can replay it: put the service behind TLS. (GitLab
//! also has signing tokens in the Standard Webhooks format, `webhook-signature`;
//! they are not read yet.)

use axum::http::HeaderMap;
use serde_json::Value;

use super::{
    Comment, CommentAction, Delivery, Forge, ForgeEvent, Merged, Release, Run, RunOutcome,
    constant_time_eq, header,
};

pub struct GitLab;

fn path<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for key in keys {
        cur = cur.get(key)?;
    }
    Some(cur)
}

fn text<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    path(v, keys)?.as_str()
}

impl Forge for GitLab {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn verify(&self, secret: &str, headers: &HeaderMap, _body: &[u8]) -> bool {
        header(headers, "x-gitlab-token")
            .is_some_and(|token| constant_time_eq(token.trim().as_bytes(), secret.as_bytes()))
    }

    /// `webhook-id` (the same for every retry), else `Idempotency-Key`, else
    /// `X-Gitlab-Event-UUID`, which webhooks triggered by webhooks share.
    fn delivery(&self, headers: &HeaderMap) -> Option<Delivery> {
        let id = header(headers, "webhook-id")
            .or_else(|| header(headers, "idempotency-key"))
            .or_else(|| header(headers, "x-gitlab-event-uuid"))?;
        Some(Delivery {
            id: id.to_string(),
            event: header(headers, "x-gitlab-event")?.to_string(),
        })
    }

    fn parse(&self, event: &str, payload: &Value) -> Option<ForgeEvent> {
        match event {
            "Note Hook" => note(payload),
            "Merge Request Hook" => merged(payload),
            "Pipeline Hook" => pipeline(payload),
            "Release Hook" => release(payload),
            _ => None,
        }
    }
}

fn repo(payload: &Value) -> Option<String> {
    Some(text(payload, &["project", "path_with_namespace"])?.to_string())
}

/// A comment on an issue or a merge request. GitLab says nothing when a comment is
/// deleted, so a lease or a claim lives until it runs out.
fn note(payload: &Value) -> Option<ForgeEvent> {
    let note = payload.get("object_attributes")?;
    let (issue, title) = match note["noteable_type"].as_str()? {
        "Issue" => (&payload["issue"], &payload["issue"]),
        "MergeRequest" => (&payload["merge_request"], &payload["merge_request"]),
        _ => return None,
    };
    let action = match note["action"].as_str()? {
        "create" => CommentAction::Created,
        "update" => CommentAction::Edited,
        _ => return None,
    };
    Some(ForgeEvent::Comment(Comment {
        repo: repo(payload)?,
        issue: issue["iid"].as_i64().unwrap_or(0),
        title: title["title"].as_str().unwrap_or("").to_string(),
        comment_id: note["id"].as_i64()?,
        action,
        body: note["note"].as_str().unwrap_or("").to_string(),
    }))
}

fn merged(payload: &Value) -> Option<ForgeEvent> {
    let attrs = payload.get("object_attributes")?;
    (attrs["action"].as_str() == Some("merge")).then(|| {
        ForgeEvent::Merged(Merged {
            repo: repo(payload).unwrap_or_default(),
            number: attrs["iid"].as_i64(),
            branch: attrs["source_branch"].as_str().map(str::to_string),
        })
    })
}

/// A pipeline of a branch. GitLab pipelines have no stable name, so all the
/// pipelines of a project count as one workflow.
fn pipeline(payload: &Value) -> Option<ForgeEvent> {
    let attrs = payload.get("object_attributes")?;
    let outcome = match attrs["status"].as_str()? {
        "success" => RunOutcome::Passed,
        "failed" => RunOutcome::Failed,
        _ => RunOutcome::Other,
    };
    let web_url = text(payload, &["project", "web_url"]).unwrap_or("");
    let url = match attrs["id"].as_i64() {
        Some(id) if !web_url.is_empty() => format!("{web_url}/-/pipelines/{id}"),
        _ => web_url.to_string(),
    };
    Some(ForgeEvent::Run(Run {
        repo: repo(payload)?,
        branch: attrs["ref"].as_str()?.to_string(),
        default_branch: text(payload, &["project", "default_branch"])
            .unwrap_or("main")
            .to_string(),
        outcome,
        workflow: "pipeline".to_string(),
        title: text(payload, &["commit", "title"]).map(str::to_string),
        url,
    }))
}

fn release(payload: &Value) -> Option<ForgeEvent> {
    (payload["action"].as_str() == Some("create")).then(|| {
        ForgeEvent::Release(Release {
            repo: repo(payload).unwrap_or_default(),
            tag: payload["tag"].as_str().unwrap_or("?").to_string(),
            url: payload["url"].as_str().unwrap_or("").to_string(),
        })
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;

    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_static(k),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn the_token_must_be_the_secret() {
        assert!(GitLab.verify("s3cret", &headers(&[("x-gitlab-token", "s3cret")]), b""));
        assert!(!GitLab.verify("s3cret", &headers(&[("x-gitlab-token", "s3cre")]), b""));
        assert!(!GitLab.verify("s3cret", &headers(&[("x-gitlab-token", "S3CRET")]), b""));
        assert!(!GitLab.verify("s3cret", &headers(&[]), b""));
    }

    #[test]
    fn the_delivery_id_prefers_the_one_that_is_the_same_for_every_retry() {
        let all = headers(&[
            ("x-gitlab-event", "Note Hook"),
            ("webhook-id", "w-1"),
            ("idempotency-key", "i-1"),
            ("x-gitlab-event-uuid", "u-1"),
        ]);
        assert_eq!(GitLab.delivery(&all).unwrap().id, "w-1");
        let two = headers(&[
            ("x-gitlab-event", "Note Hook"),
            ("idempotency-key", "i-1"),
            ("x-gitlab-event-uuid", "u-1"),
        ]);
        assert_eq!(GitLab.delivery(&two).unwrap().id, "i-1");
        let one = headers(&[
            ("x-gitlab-event", "Note Hook"),
            ("x-gitlab-event-uuid", "u-1"),
        ]);
        let d = GitLab.delivery(&one).unwrap();
        assert_eq!((d.id.as_str(), d.event.as_str()), ("u-1", "Note Hook"));
        assert_eq!(
            GitLab.delivery(&headers(&[("x-gitlab-event", "Note Hook")])),
            None
        );
    }

    fn project() -> serde_json::Value {
        json!({"path_with_namespace": "acme/demo", "default_branch": "main", "web_url": "https://git.example.com/acme/demo"})
    }

    #[test]
    fn a_comment_on_an_issue_or_a_merge_request_is_a_comment() {
        let issue = json!({
            "project": project(),
            "object_attributes": {"id": 1241, "note": "hello", "noteable_type": "Issue", "action": "create"},
            "issue": {"iid": 17, "title": "test"},
        });
        assert_eq!(
            GitLab.parse("Note Hook", &issue),
            Some(ForgeEvent::Comment(Comment {
                repo: "acme/demo".into(),
                issue: 17,
                title: "test".into(),
                comment_id: 1241,
                action: CommentAction::Created,
                body: "hello".into(),
            }))
        );
        let mr = json!({
            "project": project(),
            "object_attributes": {"id": 2, "note": "again", "noteable_type": "MergeRequest", "action": "update"},
            "merge_request": {"iid": 3, "title": "Fix it"},
        });
        let Some(ForgeEvent::Comment(c)) = GitLab.parse("Note Hook", &mr) else {
            panic!("not a comment")
        };
        assert_eq!(
            (c.issue, c.title.as_str(), c.action),
            (3, "Fix it", CommentAction::Edited)
        );
        // A comment on a commit or a snippet is not about an issue.
        let commit = json!({
            "project": project(),
            "object_attributes": {"id": 3, "note": "x", "noteable_type": "Commit", "action": "create"},
        });
        assert_eq!(GitLab.parse("Note Hook", &commit), None);
    }

    #[test]
    fn only_the_merge_action_of_a_merge_request_counts() {
        let payload = |action: &str| {
            json!({
                "project": project(),
                "object_attributes": {"iid": 16, "action": action, "source_branch": "feature/x", "state": "merged"},
            })
        };
        assert_eq!(
            GitLab.parse("Merge Request Hook", &payload("merge")),
            Some(ForgeEvent::Merged(Merged {
                repo: "acme/demo".into(),
                number: Some(16),
                branch: Some("feature/x".into()),
            }))
        );
        for action in ["open", "close", "update", "approved"] {
            assert_eq!(
                GitLab.parse("Merge Request Hook", &payload(action)),
                None,
                "{action}"
            );
        }
    }

    #[test]
    fn a_pipeline_is_a_run_of_its_branch() {
        let payload = |status: &str, branch: &str| {
            json!({
                "project": project(),
                "object_attributes": {"id": 31, "ref": branch, "status": status},
                "commit": {"title": "Fix the build"},
            })
        };
        let Some(ForgeEvent::Run(run)) = GitLab.parse("Pipeline Hook", &payload("failed", "main"))
        else {
            panic!("not a run")
        };
        assert_eq!(run.outcome, RunOutcome::Failed);
        assert_eq!(
            (run.branch.as_str(), run.default_branch.as_str()),
            ("main", "main")
        );
        assert_eq!(run.title.as_deref(), Some("Fix the build"));
        assert_eq!(run.url, "https://git.example.com/acme/demo/-/pipelines/31");
        let outcome = |status| match GitLab.parse("Pipeline Hook", &payload(status, "main")) {
            Some(ForgeEvent::Run(r)) => r.outcome,
            other => panic!("{other:?}"),
        };
        assert_eq!(outcome("success"), RunOutcome::Passed);
        for status in ["running", "pending", "canceled", "skipped"] {
            assert_eq!(outcome(status), RunOutcome::Other, "{status}");
        }
    }

    #[test]
    fn a_created_release_is_published() {
        let payload = |action: &str| json!({"action": action, "tag": "v1.1", "url": "https://git.example.com/acme/demo/-/releases/v1.1", "project": project()});
        assert_eq!(
            GitLab.parse("Release Hook", &payload("create")),
            Some(ForgeEvent::Release(Release {
                repo: "acme/demo".into(),
                tag: "v1.1".into(),
                url: "https://git.example.com/acme/demo/-/releases/v1.1".into(),
            }))
        );
        assert_eq!(GitLab.parse("Release Hook", &payload("delete")), None);
        assert_eq!(GitLab.parse("Push Hook", &payload("create")), None);
    }
}
