//! GitHub: `POST /hooks/github`, signed with `X-Hub-Signature-256`.

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Delivery, Forge, ForgeEvent, header, hubstyle, signature_matches};

pub struct GitHub;

impl Forge for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    /// `X-Hub-Signature-256` is `sha256=` and the HMAC-SHA256 of the body in hex.
    fn verify(&self, secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
        match header(headers, "x-hub-signature-256") {
            Some(signature) if signature.trim().starts_with("sha256=") => {
                signature_matches(secret, body, signature)
            }
            _ => false,
        }
    }

    fn delivery(&self, headers: &HeaderMap) -> Option<Delivery> {
        Some(Delivery {
            id: header(headers, "x-github-delivery")?.to_string(),
            event: header(headers, "x-github-event")?.to_string(),
        })
    }

    fn parse(&self, event: &str, payload: &Value) -> Option<ForgeEvent> {
        hubstyle::parse(event, payload)
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderName, HeaderValue};
    use serde_json::json;

    use super::*;
    use crate::forge::{Comment, CommentAction, hex, hmac_sha256};

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
    fn only_a_prefixed_signature_of_the_right_secret_is_accepted() {
        let sig = format!("sha256={}", hex(&hmac_sha256(b"s3cret", b"{}")));
        let ok = headers(&[("x-hub-signature-256", &sig)]);
        assert!(GitHub.verify("s3cret", &ok, b"{}"));
        assert!(!GitHub.verify("other", &ok, b"{}"));
        assert!(!GitHub.verify("s3cret", &ok, b"{ }"));
        // GitHub always sends the prefix; a bare hex digest is not GitHub.
        let bare = hex(&hmac_sha256(b"s3cret", b"{}"));
        assert!(!GitHub.verify("s3cret", &headers(&[("x-hub-signature-256", &bare)]), b"{}"));
        // The Gitea header is not read here.
        assert!(!GitHub.verify("s3cret", &headers(&[("x-gitea-signature", &bare)]), b"{}"));
    }

    #[test]
    fn the_delivery_comes_from_the_github_headers() {
        let h = headers(&[
            ("x-github-delivery", "72d3162e-cc78-11e3-81ab-4c9367dc0958"),
            ("x-github-event", "issue_comment"),
        ]);
        let d = GitHub.delivery(&h).unwrap();
        assert_eq!(d.id, "72d3162e-cc78-11e3-81ab-4c9367dc0958");
        assert_eq!(d.event, "issue_comment");
        assert_eq!(GitHub.delivery(&headers(&[("x-gitea-event", "x")])), None);
    }

    #[test]
    fn a_comment_of_github_is_read_like_one_of_gitea() {
        let payload = json!({
            "action": "created",
            "comment": {"id": 7, "body": "hello"},
            "issue": {"number": 12, "title": "A resource"},
            "repository": {"full_name": "acme/demo"},
        });
        assert_eq!(
            GitHub.parse("issue_comment", &payload),
            Some(ForgeEvent::Comment(Comment {
                repo: "acme/demo".into(),
                issue: 12,
                title: "A resource".into(),
                comment_id: 7,
                action: CommentAction::Created,
                body: "hello".into(),
            }))
        );
        assert_eq!(GitHub.parse("star", &payload), None);
    }
}
