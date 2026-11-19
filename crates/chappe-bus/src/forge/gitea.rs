//! Gitea: `POST /hooks/gitea`, signed with `X-Gitea-Signature`.

use axum::http::HeaderMap;
use serde_json::Value;

use super::{Delivery, Forge, ForgeEvent, header, hubstyle, signature_matches};

pub struct Gitea;

impl Forge for Gitea {
    fn name(&self) -> &'static str {
        "gitea"
    }

    /// `X-Gitea-Signature` is the HMAC-SHA256 of the body in hex. Gitea can also be
    /// set to send the GitHub-style `X-Hub-Signature-256`.
    fn verify(&self, secret: &str, headers: &HeaderMap, body: &[u8]) -> bool {
        let signature = header(headers, "x-gitea-signature")
            .or_else(|| header(headers, "x-hub-signature-256"))
            .unwrap_or("");
        signature_matches(secret, body, signature)
    }

    fn delivery(&self, headers: &HeaderMap) -> Option<Delivery> {
        Some(Delivery {
            id: header(headers, "x-gitea-delivery")?.to_string(),
            event: header(headers, "x-gitea-event")?.to_string(),
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
    use crate::forge::{hex, hmac_sha256};

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
    fn the_signature_may_come_in_the_gitea_or_the_github_header() {
        let sig = hex(&hmac_sha256(b"s3cret", b"{}"));
        assert!(Gitea.verify("s3cret", &headers(&[("x-gitea-signature", &sig)]), b"{}"));
        let prefixed = format!("sha256={sig}");
        assert!(Gitea.verify(
            "s3cret",
            &headers(&[("x-hub-signature-256", &prefixed)]),
            b"{}"
        ));
        assert!(!Gitea.verify("other", &headers(&[("x-gitea-signature", &sig)]), b"{}"));
        assert!(!Gitea.verify("s3cret", &headers(&[]), b"{}"));
    }

    #[test]
    fn the_delivery_needs_an_id_and_an_event() {
        let h = headers(&[("x-gitea-delivery", "d-1"), ("x-gitea-event", "release")]);
        assert_eq!(
            Gitea.delivery(&h),
            Some(Delivery {
                id: "d-1".into(),
                event: "release".into()
            })
        );
        assert_eq!(
            Gitea.delivery(&headers(&[("x-gitea-event", "release")])),
            None
        );
    }

    #[test]
    fn a_payload_without_a_repository_is_not_an_event() {
        assert_eq!(
            Gitea.parse("release", &json!({"action": "published"})),
            None
        );
    }
}
