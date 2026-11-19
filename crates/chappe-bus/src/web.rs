//! HTTP interface: the router, the shared state, the headers, the event stream.
//!
//! Two audiences share one listener and are told apart by the ingress, not by
//! the service:
//! - the owner's page (`pages.rs`: `/`, `/p/...`, `/events`, assets) sits behind
//!   the single sign-on, so the service sees only requests that already passed it;
//! - `/v1/*` (`api.rs`) and `/healthz` are routed around the sign-on. `/v1/*`
//!   answers only with a valid agent token;
//! - `/hooks/gitea`, `/hooks/github` and `/hooks/gitlab` (`hooks.rs`) are routed
//!   around it too, for the forges; they answer only to a request that proves it
//!   knows the shared secret.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::http::header::{self, HeaderName, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, extract::Request};
use futures_util::stream::{self, Stream, StreamExt};
use tokio::sync::{broadcast, watch};

use crate::store::Store;
use crate::{api, hooks, pages};

/// A form post is a topic and 600 characters; anything bigger is not ours.
const MAX_FORM_BYTES: usize = 16 * 1024;

pub struct AppState {
    pub store: Store,
    /// Tells the open pages what changed: `msg:<project>/<topic>` for a stored
    /// message, `board` for a change of the registry, `nav` for a new topic.
    pub events: broadcast::Sender<String>,
    /// Turns true when the service is shutting down, to end open streams.
    pub shutdown: watch::Receiver<bool>,
    /// The secret of the forge webhooks; without it `/hooks/*` answers 404.
    pub webhook_secret: Option<String>,
}

impl AppState {
    pub fn new(store: Store, shutdown: watch::Receiver<bool>) -> Arc<AppState> {
        Arc::new(AppState {
            store,
            events: broadcast::channel(64).0,
            shutdown,
            webhook_secret: None,
        })
    }

    /// The state with the webhook secret set.
    pub fn with_webhook_secret(
        store: Store,
        shutdown: watch::Receiver<bool>,
        secret: Option<String>,
    ) -> Arc<AppState> {
        Arc::new(AppState {
            store,
            events: broadcast::channel(64).0,
            shutdown,
            webhook_secret: secret,
        })
    }

    /// Tells the open pages something changed. Nobody listening is the normal case.
    pub fn notify(&self, event: String) {
        let _ = self.events.send(event);
    }
}

pub type Shared = Arc<AppState>;

pub fn router(state: Shared) -> Router {
    Router::new()
        .merge(pages::routes())
        .merge(api::routes())
        .merge(hooks::routes())
        .route("/events", get(events))
        .route("/healthz", get(healthz))
        .route(
            "/app.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("static/app.css")) }),
        )
        .route(
            "/app.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("static/app.js"),
                )
            }),
        )
        .layer(DefaultBodyLimit::max(MAX_FORM_BYTES))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Serves the router on `listener` until `shutdown` completes.
pub async fn serve(
    listener: tokio::net::TcpListener,
    state: Shared,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// Static policy: the page loads only its own script and style, talks only to
/// itself, and cannot be framed.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
        ),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        h.insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    res
}

pub(crate) fn server_error(e: impl std::fmt::Display) -> Response {
    eprintln!("chappe-bus: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}

/// Server-sent events: one event per change (see [`AppState::events`]). An
/// event with empty data tells the page that some were missed.
async fn events(State(state): State<Shared>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();
    let mut shutdown = state.shutdown.clone();
    let stream = stream::unfold(rx, |mut rx| async move {
        let data = match rx.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => String::new(),
            Err(broadcast::error::RecvError::Closed) => return None,
        };
        Some((Ok(Event::default().data(data)), rx))
    })
    .take_until(async move {
        let _ = shutdown.wait_for(|stop| *stop).await;
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn healthz(State(state): State<Shared>) -> Response {
    match state.store.ping() {
        Ok(()) => "ok\n".into_response(),
        Err(e) => {
            eprintln!("chappe-bus: health check failed: {e}");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable\n").into_response()
        }
    }
}

/// Helpers for the tests of this module and of `pages` and `api`.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderMap, Request as HttpRequest};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::auth;
    use crate::store::TokenEntry;

    pub const TOKEN: &str = "a-token-that-is-long-enough-0123456789";
    pub const TOKEN_MAC: &str = "b-token-that-is-long-enough-0123456789";
    pub const TOKEN_HOST: &str = "c-token-that-is-long-enough-0123456789";

    pub fn app() -> (Router, Shared) {
        let store = Store::open_in_memory().unwrap();
        store
            .ensure_projects(&["all".into(), "demo".into()])
            .unwrap();
        let entry = |name: &str, machine: &str, tool: &str, token: &str| TokenEntry {
            name: name.into(),
            machine: machine.into(),
            tool: tool.into(),
            hash: auth::hash_token(token),
        };
        store
            .sync_tokens(&[
                entry("host-a.claude", "host-a", "claude", TOKEN),
                entry("mac.codex", "mac", "codex", TOKEN_MAC),
                entry("mac.host", "mac", "host", TOKEN_HOST),
            ])
            .unwrap();
        let state = AppState::new(store, watch::channel(false).1);
        (router(state.clone()), state)
    }

    pub async fn call(app: &Router, req: HttpRequest<Body>) -> (StatusCode, HeaderMap, String) {
        let res = app.clone().oneshot(req).await.unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let body = res.into_body().collect().await.unwrap().to_bytes();
        (status, headers, String::from_utf8(body.to_vec()).unwrap())
    }

    pub fn get_req(uri: &str) -> HttpRequest<Body> {
        HttpRequest::get(uri).body(Body::empty()).unwrap()
    }

    pub fn form_req(uri: &str, body: &str, origin: Option<&str>) -> HttpRequest<Body> {
        let mut req = HttpRequest::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::HOST, "chappe-bus.test");
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        req.body(Body::from(body.to_string())).unwrap()
    }

    pub fn api(
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> HttpRequest<Body> {
        let mut req = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        if body.is_some() {
            req = req.header(header::CONTENT_TYPE, "application/json");
        }
        req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_reports_ok() {
        let (app, _) = app();
        let (status, _, body) = call(&app, get_req("/healthz")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "ok\n"));
    }

    #[tokio::test]
    async fn responses_carry_the_security_headers() {
        let (app, _) = app();
        for uri in ["/", "/p/all/general", "/app.js", "/healthz", "/v1/whoami"] {
            let (_, headers, _) = call(&app, get_req(uri)).await;
            assert!(
                headers[header::CONTENT_SECURITY_POLICY]
                    .to_str()
                    .unwrap()
                    .contains("default-src 'none'"),
                "{uri}"
            );
            assert_eq!(headers["x-content-type-options"], "nosniff", "{uri}");
        }
    }

    #[tokio::test]
    async fn assets_are_served_with_their_types() {
        let (app, _) = app();
        let (_, css, _) = call(&app, get_req("/app.css")).await;
        let (_, js, body) = call(&app, get_req("/app.js")).await;
        assert!(
            css[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/css")
        );
        assert!(
            js[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/javascript")
        );
        assert!(body.contains("EventSource"));
    }

    #[tokio::test]
    async fn event_stream_delivers_events_and_ends_on_shutdown() {
        let store = Store::open_in_memory().unwrap();
        let (stop, shutdown) = watch::channel(false);
        let state = AppState::new(store, shutdown);
        let app = router(state.clone());
        let res = app.oneshot(get_req("/events")).await.unwrap();
        assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
        state.notify("msg:demo/general".into());
        let mut body = res.into_body();
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("data: msg:demo/general"));
        stop.send(true).unwrap();
        assert!(body.frame().await.is_none(), "the stream ends on shutdown");
    }

    #[tokio::test]
    async fn an_unknown_path_is_not_found() {
        let (app, _) = app();
        assert_eq!(call(&app, get_req("/nope")).await.0, StatusCode::NOT_FOUND);
        let _ = Body::empty();
    }
}
