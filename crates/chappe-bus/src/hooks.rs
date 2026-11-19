//! The receiver of forge webhooks: `POST /hooks/gitea`, `/hooks/github` and
//! `/hooks/gitlab`.
//!
//! A forge cannot sign in to the page, so the ingress routes these paths around the
//! single sign-on, and the request proves itself instead (how is the business of the
//! adapter of the forge: an HMAC of the body, or the secret token itself). The secret
//! is the same for all forges. Without it in the settings the paths are not there at
//! all.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;

use crate::forge::{Forge, forges};
use crate::store::now;
use crate::web::Shared;

/// Webhook bodies are bigger than a form post: a pull request is tens of kilobytes.
const MAX_BODY: usize = 2 * 1024 * 1024;

pub fn routes() -> Router<Shared> {
    let mut router = Router::new();
    for forge in forges() {
        router = router.route(
            &format!("/hooks/{}", forge.name()),
            post(
                move |state: State<Shared>, headers: HeaderMap, body: Bytes| {
                    receive(forge, state, headers, body)
                },
            )
            .layer(DefaultBodyLimit::max(MAX_BODY)),
        );
    }
    router
}

async fn receive(
    forge: &'static dyn Forge,
    State(state): State<Shared>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let Some(secret) = state.webhook_secret.as_deref() else {
        return StatusCode::NOT_FOUND;
    };
    if !forge.verify(secret, &headers, &body) {
        return StatusCode::UNAUTHORIZED;
    }
    let Some(delivery) = forge.delivery(&headers) else {
        return StatusCode::BAD_REQUEST;
    };
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return StatusCode::BAD_REQUEST;
    };
    let event = forge.parse(&delivery.event, &payload);
    match state.store.forge_event(&delivery.id, event.as_ref(), now()) {
        Ok(refresh) => {
            // What came, never what it says.
            let repo = event.as_ref().map_or("?", |e| e.repo());
            let posted = refresh.iter().filter(|p| p.starts_with("msg:")).count();
            eprintln!(
                "chappe-bus: {} {} {repo}: {posted} lines",
                forge.name(),
                delivery.event
            );
            for page in refresh {
                state.notify(page);
            }
            StatusCode::OK
        }
        Err(e) => {
            eprintln!(
                "chappe-bus: webhook {} {}: {e}",
                forge.name(),
                delivery.event
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[cfg(test)]
mod tests;
