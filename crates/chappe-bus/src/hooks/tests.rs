use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tokio::sync::watch;
use tower::ServiceExt;

use crate::forge::EVENTS;
use crate::store::Store;
use crate::web::{AppState, Shared, router};

const SECRET: &str = "webhook-secret-0123456789";

fn state(secret: Option<&str>) -> Shared {
    let store = Store::open_in_memory().unwrap();
    store
        .ensure_projects(&["all".into(), "demo".into()])
        .unwrap();
    AppState::with_webhook_secret(store, watch::channel(false).1, secret.map(str::to_string))
}

fn sign(secret: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut block = [0u8; 64];
    block[..secret.len()].copy_from_slice(secret.as_bytes());
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(body);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn send(
    state: &Shared,
    signature: Option<&str>,
    event: Option<&str>,
    delivery: &str,
    body: &[u8],
) -> StatusCode {
    let mut req = Request::builder().method("POST").uri("/hooks/gitea");
    if let Some(s) = signature {
        req = req.header("x-gitea-signature", s);
    }
    if let Some(e) = event {
        req = req.header("x-gitea-event", e);
    }
    let req = req
        .header("x-gitea-delivery", delivery)
        .body(Body::from(body.to_vec()))
        .unwrap();
    let res = router(state.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let _ = res.into_body().collect().await;
    status
}

fn lease_body() -> Vec<u8> {
    serde_json::json!({
        "action": "created",
        "comment": {"id": 1, "body": "<!-- resource-lease: exclusive -->\nagent: claude@host-a\naction: reboot\nuntil: 2099-01-01 00:00 UTC"},
        "issue": {"number": 480, "title": "Resource"},
        "repository": {"full_name": "acme/demo"},
    })
    .to_string()
    .into_bytes()
}

/// A POST to `path` with the given headers.
async fn post_to(state: &Shared, path: &str, headers: &[(&str, &str)], body: &[u8]) -> StatusCode {
    let mut req = Request::builder().method("POST").uri(path);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let req = req.body(Body::from(body.to_vec())).unwrap();
    let res = router(state.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let _ = res.into_body().collect().await;
    status
}

fn gitlab_lease_body() -> Vec<u8> {
    serde_json::json!({
        "object_kind": "note",
        "project": {"path_with_namespace": "acme/demo", "default_branch": "main"},
        "object_attributes": {
            "id": 1,
            "note": "<!-- resource-lease: exclusive -->\nagent: claude@host-a\naction: reboot\nuntil: 2099-01-01 00:00 UTC",
            "noteable_type": "Issue",
            "action": "create",
        },
        "issue": {"iid": 480, "title": "Resource"},
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn github_signs_with_a_prefixed_digest_and_is_read_the_same_way() {
    let st = state(Some(SECRET));
    let body = lease_body();
    let sig = format!("sha256={}", sign(SECRET, &body));
    let ok = [
        ("x-hub-signature-256", sig.as_str()),
        ("x-github-event", "issue_comment"),
        ("x-github-delivery", "g1"),
    ];
    assert_eq!(
        post_to(&st, "/hooks/github", &ok, &body).await,
        StatusCode::OK
    );
    assert_eq!(
        st.store.recent_messages("demo", EVENTS, 10).unwrap().len(),
        1
    );
    // The same delivery again, and a digest without the prefix.
    assert_eq!(
        post_to(&st, "/hooks/github", &ok, &body).await,
        StatusCode::OK
    );
    assert_eq!(
        st.store.recent_messages("demo", EVENTS, 10).unwrap().len(),
        1
    );
    let bare = sign(SECRET, &body);
    let refused = [
        ("x-hub-signature-256", bare.as_str()),
        ("x-github-event", "issue_comment"),
        ("x-github-delivery", "g2"),
    ];
    assert_eq!(
        post_to(&st, "/hooks/github", &refused, &body).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn gitlab_sends_the_secret_and_its_events_have_other_names() {
    let st = state(Some(SECRET));
    let body = gitlab_lease_body();
    let ok = [
        ("x-gitlab-token", SECRET),
        ("x-gitlab-event", "Note Hook"),
        ("webhook-id", "w1"),
    ];
    assert_eq!(
        post_to(&st, "/hooks/gitlab", &ok, &body).await,
        StatusCode::OK
    );
    assert_eq!(
        st.store.recent_messages("demo", EVENTS, 10).unwrap().len(),
        1
    );
    assert_eq!(st.store.leases(0).unwrap().len(), 1);
    // A retry carries the same id and changes nothing.
    assert_eq!(
        post_to(&st, "/hooks/gitlab", &ok, &body).await,
        StatusCode::OK
    );
    assert_eq!(
        st.store.recent_messages("demo", EVENTS, 10).unwrap().len(),
        1
    );
    let wrong = [
        ("x-gitlab-token", "not-the-secret"),
        ("x-gitlab-event", "Note Hook"),
        ("webhook-id", "w2"),
    ];
    assert_eq!(
        post_to(&st, "/hooks/gitlab", &wrong, &body).await,
        StatusCode::UNAUTHORIZED
    );
    // An event the service does not use is taken and changes nothing.
    let push = [
        ("x-gitlab-token", SECRET),
        ("x-gitlab-event", "Push Hook"),
        ("webhook-id", "w3"),
    ];
    assert_eq!(
        post_to(&st, "/hooks/gitlab", &push, b"{}").await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_forge_does_not_accept_the_proof_of_another() {
    let st = state(Some(SECRET));
    let body = lease_body();
    let sig = sign(SECRET, &body);
    // A Gitea signature at the GitHub and GitLab paths, and a GitLab token at Gitea's.
    let gitea_style = [
        ("x-gitea-signature", sig.as_str()),
        ("x-github-event", "issue_comment"),
        ("x-github-delivery", "x1"),
    ];
    assert_eq!(
        post_to(&st, "/hooks/github", &gitea_style, &body).await,
        StatusCode::UNAUTHORIZED
    );
    let token = [("x-gitlab-token", SECRET), ("x-gitlab-event", "Note Hook")];
    assert_eq!(
        post_to(&st, "/hooks/gitea", &token, &body).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post_to(&st, "/hooks/gitlab", &gitea_style, &body).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn without_a_secret_the_path_is_not_there() {
    let st = state(None);
    let body = lease_body();
    let sig = sign(SECRET, &body);
    assert_eq!(
        send(&st, Some(&sig), Some("issue_comment"), "d1", &body).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_request_without_the_right_signature_is_refused_and_changes_nothing() {
    let st = state(Some(SECRET));
    let body = lease_body();
    let wrong = sign("another-secret-0123456789", &body);
    for sig in [None, Some(""), Some("deadbeef"), Some(wrong.as_str())] {
        assert_eq!(
            send(&st, sig, Some("issue_comment"), "d1", &body).await,
            StatusCode::UNAUTHORIZED,
            "{sig:?}"
        );
    }
    assert!(
        st.store
            .recent_messages("demo", EVENTS, 10)
            .unwrap()
            .is_empty()
    );
    assert!(st.store.leases(0).unwrap().is_empty());
}

#[tokio::test]
async fn a_signed_event_is_taken_once() {
    let st = state(Some(SECRET));
    let mut stream = st.events.subscribe();
    let body = lease_body();
    let sig = sign(SECRET, &body);
    assert_eq!(
        send(&st, Some(&sig), Some("issue_comment"), "d1", &body).await,
        StatusCode::OK
    );
    assert_eq!(
        st.store.recent_messages("demo", EVENTS, 10).unwrap().len(),
        1
    );
    assert_eq!(
        st.store.recent_messages("all", EVENTS, 10).unwrap().len(),
        1
    );
    let heard: Vec<_> = std::iter::from_fn(|| stream.try_recv().ok()).collect();
    assert!(heard.contains(&"msg:demo/events".to_string()) && heard.contains(&"board".to_string()));
    // Gitea redelivers the same delivery.
    assert_eq!(
        send(&st, Some(&sig), Some("issue_comment"), "d1", &body).await,
        StatusCode::OK
    );
    assert_eq!(
        st.store.recent_messages("demo", EVENTS, 10).unwrap().len(),
        1
    );
}

#[tokio::test]
async fn a_signed_request_that_is_not_an_event_is_a_bad_request() {
    let st = state(Some(SECRET));
    let body = lease_body();
    let sig = sign(SECRET, &body);
    assert_eq!(
        send(&st, Some(&sig), None, "d1", &body).await,
        StatusCode::BAD_REQUEST,
        "no event header"
    );
    let junk = b"not json";
    assert_eq!(
        send(
            &st,
            Some(&sign(SECRET, junk)),
            Some("issue_comment"),
            "d2",
            junk
        )
        .await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn a_payload_of_a_pull_request_size_is_accepted() {
    let st = state(Some(SECRET));
    let body = serde_json::json!({
        "action": "closed",
        "pull_request": {"number": 1, "merged": true, "body": "x".repeat(200_000)},
        "repository": {"full_name": "acme/demo"},
    })
    .to_string()
    .into_bytes();
    assert!(body.len() > 100_000);
    let sig = sign(SECRET, &body);
    assert_eq!(
        send(&st, Some(&sig), Some("pull_request"), "d1", &body).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_hook_is_post_only() {
    let st = state(Some(SECRET));
    let res = router(st)
        .oneshot(
            Request::builder()
                .uri("/hooks/gitea")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
}
