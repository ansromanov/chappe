//! The interface of agents: `/v1/*`, every route with a bearer token.

use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::auth;
use crate::messaging::{InboxItem, Outgoing, Receiver, SendError, Sent};
use crate::registry::{Filter, NewSession, Session, SessionError, State as SessionState};
use crate::store::{Agent, StoreError, now};
use crate::wakes::{Finish, WakeJob};
use crate::web::{Shared, server_error};

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/sessions", post(register_session).get(list_sessions))
        .route("/v1/sessions/{handle}", delete(leave_session))
        .route("/v1/sessions/{handle}/beat", post(beat_session))
        .route("/v1/topics", post(create_topic).get(list_topics))
        .route("/v1/messages", post(send_message))
        .route("/v1/inbox", post(read_inbox))
        .route("/v1/host/status", get(host_status))
        .route("/v1/host/claim", post(host_claim))
        .route("/v1/host/wakes/{id}", post(host_report))
}

/// An agent that presented a valid token.
struct Authenticated(Agent);

impl FromRequestParts<Shared> for Authenticated {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Shared,
    ) -> Result<Self, Self::Rejection> {
        let unauthorized = || {
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "token required\n",
            )
                .into_response()
        };
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(auth::bearer)
            .ok_or_else(unauthorized)?;
        match state.store.authenticate(&auth::hash_token(token)) {
            Ok(Some(agent)) => Ok(Authenticated(agent)),
            Ok(None) => Err(unauthorized()),
            Err(e) => Err(server_error(e)),
        }
    }
}

/// An error of the agent interface, as `{"error": "..."}`.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            error: String,
        }
        (self.0, Json(Body { error: self.1 })).into_response()
    }
}

fn internal(e: &dyn std::fmt::Display) -> ApiError {
    eprintln!("chappe-bus: {e}");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
}

impl From<SessionError> for ApiError {
    fn from(e: SessionError) -> Self {
        let status = match &e {
            SessionError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            SessionError::NotFound => StatusCode::NOT_FOUND,
            SessionError::Forbidden => StatusCode::FORBIDDEN,
            SessionError::Gone => StatusCode::GONE,
            SessionError::Store(inner) => return internal(inner),
        };
        ApiError(status, e.to_string())
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        let status = match &e {
            StoreError::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            StoreError::NoSuchProject | StoreError::NoSuchTopic => StatusCode::NOT_FOUND,
            StoreError::TopicExists => StatusCode::CONFLICT,
            StoreError::TopicLimit(_) => StatusCode::TOO_MANY_REQUESTS,
            _ => return internal(&e),
        };
        ApiError(status, e.to_string())
    }
}

impl From<SendError> for ApiError {
    fn from(e: SendError) -> Self {
        match e {
            SendError::Invalid(why) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, why),
            SendError::Refused(why) => ApiError(StatusCode::TOO_MANY_REQUESTS, why),
            SendError::Unavailable(why) => ApiError(StatusCode::CONFLICT, why),
            SendError::Session(e) => e.into(),
            SendError::Store(e) => e.into(),
        }
    }
}

#[derive(Serialize)]
struct WhoAmI {
    machine: String,
    tool: String,
}

async fn whoami(Authenticated(agent): Authenticated) -> Json<WhoAmI> {
    Json(WhoAmI {
        machine: agent.machine,
        tool: agent.tool,
    })
}

/// A session as agents see it. The transcript and the session id stay inside.
#[derive(Serialize)]
struct SessionView {
    handle: String,
    name: Option<String>,
    roles: Vec<String>,
    agent: String,
    machine: String,
    tool: String,
    project: String,
    issue: Option<String>,
    branch: Option<String>,
    state: &'static str,
    last_seen_secs_ago: i64,
    /// At registration: who else works in the project and which shared
    /// resources are held, for the session to know before it starts.
    #[serde(skip_serializing_if = "Option::is_none")]
    briefing: Option<String>,
}

/// Who else works in the project of `me` and on what, and the leases in force:
/// what a session should know before it touches anything. None when there is
/// nothing to say.
fn briefing(state: &Shared, me: &Session, now: i64) -> Result<Option<String>, ApiError> {
    let filter = Filter {
        project: Some(&me.project),
        role: None,
    };
    let others: Vec<Session> = state
        .store
        .sessions(filter, now)?
        .into_iter()
        .filter(|s| s.handle != me.handle)
        .collect();
    let claims = state.store.claims().map_err(SessionError::Store)?;
    let leases = state.store.leases(now).map_err(SessionError::Store)?;
    if others.is_empty() && leases.is_empty() {
        return Ok(None);
    }
    let mut out = String::new();
    if !others.is_empty() {
        out.push_str(&format!("Working in project {} now:\n", me.project));
        for s in others.iter().take(8) {
            let name = s
                .name
                .as_deref()
                .map_or(String::new(), |n| format!(" \"{n}\""));
            let issue = s.issue.clone().or_else(|| {
                crate::forge::claim_of(s.branch.as_deref(), s.worktree.as_deref(), &claims)
                    .map(|c| format!("#{} {}", c.issue, c.title))
            });
            let issue = issue.map_or(String::new(), |i| format!(", task: {i}"));
            let branch = s
                .branch
                .as_deref()
                .filter(|b| !matches!(*b, "main" | "master"))
                .map_or(String::new(), |b| format!(", branch {b}"));
            out.push_str(&format!(
                "- {}{name} ({}){issue}{branch}, {}\n",
                s.handle,
                s.agent(),
                s.state(now).label()
            ));
        }
        if others.len() > 8 {
            out.push_str(&format!("- and {} more\n", others.len() - 8));
        }
    }
    for l in &leases {
        let kind = if l.kind == crate::forge::LeaseKind::Exclusive {
            "busy (exclusive lease)"
        } else {
            "in use"
        };
        out.push_str(&format!(
            "Resource \"{}\" {kind}: {}, until {} UTC ({}#{}).\n",
            l.title,
            l.agent,
            crate::message::format_hm(l.until),
            l.repo,
            l.issue
        ));
    }
    out.push_str("Don't take on someone else's task or branch and don't change a busy resource; don't write to these sessions unless necessary.");
    Ok(Some(out))
}

fn view(s: &Session, now: i64) -> SessionView {
    SessionView {
        agent: s.agent(),
        state: s.state(now).as_str(),
        last_seen_secs_ago: (now - s.last_seen).max(0),
        handle: s.handle.clone(),
        name: s.name.clone(),
        roles: s.roles.clone(),
        machine: s.machine.clone(),
        tool: s.tool.clone(),
        project: s.project.clone(),
        issue: s.issue.clone(),
        branch: s.branch.clone(),
        briefing: None,
    }
}

async fn register_session(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
    body: Result<Json<NewSession>, JsonRejection>,
) -> Result<(StatusCode, Json<SessionView>), ApiError> {
    let Json(new) = body.map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.body_text()))?;
    let now = now();
    let session = state.store.register(&agent, new, now)?;
    state.notify("board".into());
    state.notify("nav".into());
    let mut out = view(&session, now);
    out.briefing = briefing(&state, &session, now)?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Serialize)]
struct StateView {
    state: &'static str,
}

async fn beat_session(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
    Path(handle): Path<String>,
) -> Result<Json<StateView>, ApiError> {
    let before = state.store.beat(&agent, &handle, now())?;
    if before != SessionState::Active {
        state.notify("board".into());
    }
    Ok(Json(StateView {
        state: SessionState::Active.as_str(),
    }))
}

async fn leave_session(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
    Path(handle): Path<String>,
) -> Result<Json<StateView>, ApiError> {
    state.store.leave(&agent, &handle, now())?;
    state.notify("board".into());
    Ok(Json(StateView {
        state: SessionState::Gone.as_str(),
    }))
}

#[derive(Deserialize)]
struct WhoQuery {
    project: Option<String>,
    role: Option<String>,
}

async fn list_sessions(
    State(state): State<Shared>,
    Authenticated(_): Authenticated,
    Query(q): Query<WhoQuery>,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    let now = now();
    let filter = Filter {
        project: q.project.as_deref(),
        role: q.role.as_deref(),
    };
    let sessions = state.store.sessions(filter, now)?;
    let claims = state.store.claims().map_err(SessionError::Store)?;
    Ok(Json(
        sessions
            .iter()
            .map(|s| {
                let mut v = view(s, now);
                // What the session did not say itself, its claim in the forge says.
                if v.issue.is_none() {
                    v.issue =
                        crate::forge::claim_of(s.branch.as_deref(), s.worktree.as_deref(), &claims)
                            .map(|c| {
                                format!("#{} {}", c.issue, c.title)
                                    .chars()
                                    .take(60)
                                    .collect()
                            });
                }
                v
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewTopic {
    /// The session that makes the topic; the topic is made in its project.
    handle: String,
    name: String,
    about: Option<String>,
}

#[derive(Serialize)]
struct TopicView {
    project: String,
    name: String,
    about: Option<String>,
    messages: i64,
}

/// A session makes a topic in its own project, for a part of the system a
/// group of agents works on.
async fn create_topic(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
    body: Result<Json<NewTopic>, JsonRejection>,
) -> Result<(StatusCode, Json<TopicView>), ApiError> {
    let Json(new) = body.map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.body_text()))?;
    let now = now();
    let session = state.store.live_session(&agent, &new.handle, now)?;
    let topic = state.store.create_topic(
        &session.project,
        &new.name,
        new.about.as_deref(),
        &session.handle,
        now,
    )?;
    state.notify("nav".into());
    Ok((
        StatusCode::CREATED,
        Json(TopicView {
            project: session.project,
            name: topic.name,
            about: topic.about,
            messages: 0,
        }),
    ))
}

#[derive(Deserialize)]
struct TopicsQuery {
    project: Option<String>,
}

async fn list_topics(
    State(state): State<Shared>,
    Authenticated(_): Authenticated,
    Query(q): Query<TopicsQuery>,
) -> Result<Json<Vec<TopicView>>, ApiError> {
    let mut out = Vec::new();
    for project in state.store.nav()? {
        if q.project.as_deref().is_some_and(|p| p != project.name) {
            continue;
        }
        for t in project.topics {
            out.push(TopicView {
                project: project.name.clone(),
                name: t.name,
                about: t.about,
                messages: t.messages,
            });
        }
    }
    Ok(Json(out))
}

#[derive(Serialize)]
struct ReceiverView {
    handle: String,
    agent: String,
    roles: Vec<String>,
    issue: Option<String>,
    active: bool,
}

#[derive(Serialize)]
struct SentView {
    /// Absent for a dry run.
    id: Option<i64>,
    project: String,
    topic: String,
    kind: &'static str,
    text: String,
    receivers: Vec<ReceiverView>,
    /// For an ask: `requested`, `pending`, `limit`, `no_host` or `not_needed`.
    wake: Option<&'static str>,
    dry_run: bool,
    /// For an ask that only looks: earlier asks about the same and their answers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    similar: Vec<SimilarView>,
}

#[derive(Serialize)]
struct SimilarView {
    id: i64,
    question: String,
    answer: String,
}

fn sent_view(s: Sent) -> SentView {
    let receiver = |r: Receiver| ReceiverView {
        handle: r.handle,
        agent: r.agent,
        roles: r.roles,
        issue: r.issue,
        active: r.active,
    };
    SentView {
        id: s.id,
        project: s.project,
        topic: s.topic,
        kind: s.kind.as_str(),
        text: s.text,
        receivers: s.receivers.into_iter().map(receiver).collect(),
        wake: s.wake.map(|w| w.as_str()),
        dry_run: s.dry_run,
        similar: s
            .similar
            .into_iter()
            .map(|x| SimilarView {
                id: x.id,
                question: x.question,
                answer: x.answer,
            })
            .collect(),
    }
}

/// A message of an agent: the closed list of kinds, built from fields, under
/// the budgets. The reply says who gets it, or why it was not sent.
async fn send_message(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
    body: Result<Json<Outgoing>, JsonRejection>,
) -> Result<(StatusCode, Json<SentView>), ApiError> {
    let Json(out) = body.map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.body_text()))?;
    let sent = state.store.send(&agent, out, now())?;
    let status = if sent.dry_run {
        StatusCode::OK
    } else {
        state.notify(format!("msg:{}/{}", sent.project, sent.topic));
        state.notify("board".into());
        StatusCode::CREATED
    };
    Ok((status, Json(sent_view(sent))))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxRequest {
    handle: String,
    /// The tool's current name for the session; the hook sends it so a
    /// rename in the tool reaches the page.
    name: Option<String>,
    /// Where the session is now, for the board to find its issue.
    worktree: Option<String>,
    branch: Option<String>,
}

#[derive(Serialize)]
struct InboxItemView {
    id: i64,
    kind: &'static str,
    from: String,
    from_name: Option<String>,
    agent: Option<String>,
    project: String,
    topic: String,
    text: String,
    reply_by: Option<i64>,
    /// The text with its fixed header: what to show the agent.
    rendered: String,
}

#[derive(Serialize)]
struct InboxView {
    /// What the service itself has to say.
    notes: Vec<String>,
    messages: Vec<InboxItemView>,
    waiting: usize,
    next_in_secs: Option<i64>,
}

fn item_view(i: InboxItem) -> InboxItemView {
    InboxItemView {
        id: i.id,
        kind: i.kind.as_str(),
        from: i.from,
        from_name: i.from_name,
        agent: i.agent,
        project: i.project,
        topic: i.topic,
        text: i.text,
        reply_by: i.reply_by,
        rendered: i.rendered,
    }
}

/// Everything waiting for the session, in one block, at most once in five minutes.
async fn read_inbox(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
    body: Result<Json<InboxRequest>, JsonRejection>,
) -> Result<Json<InboxView>, ApiError> {
    let Json(req) = body.map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.body_text()))?;
    if let Some(name) = &req.name {
        // A name the service cannot take must not cost the session its inbox.
        match state.store.set_name(&agent, &req.handle, name, now()) {
            Err(SessionError::Invalid(_)) => {}
            other => other?,
        }
    }
    if req.worktree.is_some() || req.branch.is_some() {
        match state.store.set_place(
            &agent,
            &req.handle,
            req.worktree.as_deref(),
            req.branch.as_deref(),
        ) {
            Ok(true) => state.notify("board".into()),
            Ok(false) | Err(SessionError::Invalid(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let inbox = state.store.inbox(&agent, &req.handle, now())?;
    // A message that was delivered changes how its ask looks on the page.
    let mut topics: Vec<String> = inbox
        .messages
        .iter()
        .map(|m| format!("msg:{}/{}", m.project, m.topic))
        .collect();
    topics.dedup();
    for topic in topics {
        state.notify(topic);
    }
    Ok(Json(InboxView {
        notes: inbox.notes,
        messages: inbox.messages.into_iter().map(item_view).collect(),
        waiting: inbox.waiting,
        next_in_secs: inbox.next_in_secs,
    }))
}

/// The host daemon of a machine: a token named `<machine>.host`.
struct HostAuth(Agent);

impl FromRequestParts<Shared> for HostAuth {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Shared,
    ) -> Result<Self, Self::Rejection> {
        let Authenticated(agent) = Authenticated::from_request_parts(parts, state).await?;
        if agent.tool == "host" {
            Ok(HostAuth(agent))
        } else {
            Err(ApiError(
                StatusCode::FORBIDDEN,
                "this is for the host daemon of a machine".into(),
            )
            .into_response())
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ClaimRequest {
    /// Seconds to wait for a wake when there is none (at most 30).
    wait_secs: Option<u64>,
    /// Per tool, the ids of the sessions that have a live process on the machine.
    /// Left out when the host could not tell.
    running: Option<std::collections::HashMap<String, Vec<String>>>,
}

/// At most this many session ids per tool are taken from one report.
const MAX_RUNNING: usize = 5000;

#[derive(Serialize)]
struct WakeView {
    id: i64,
    handle: String,
    tool: String,
    session_id: Option<String>,
    worktree: Option<String>,
    transcript: Option<String>,
    text: String,
    message_ids: Vec<i64>,
}

fn wake_view(j: WakeJob) -> WakeView {
    WakeView {
        id: j.id,
        handle: j.handle,
        tool: j.tool,
        session_id: j.session_id,
        worktree: j.worktree,
        transcript: j.transcript,
        text: j.text,
        message_ids: j.message_ids,
    }
}

/// The wakes to carry out on the host's machine. With `wait_secs` the call
/// waits (a long poll) until there is one.
async fn host_claim(
    State(state): State<Shared>,
    HostAuth(host): HostAuth,
    body: Option<Json<ClaimRequest>>,
) -> Result<Json<Vec<WakeView>>, ApiError> {
    let (wait, running) = body.map_or((0, None), |Json(b)| (b.wait_secs.unwrap_or(0), b.running));
    let wait = wait.min(30);
    if let Some(mut running) = running {
        running.retain(|tool, _| crate::auth::parse_name(&format!("m.{tool}")).is_some());
        running
            .values_mut()
            .for_each(|ids| ids.truncate(MAX_RUNNING));
        state.store.note_processes(&host, &running, now())?;
    }
    let mut waited = 0;
    loop {
        let jobs = state.store.claim_wakes(&host, now())?;
        if !jobs.is_empty() || waited >= wait {
            return Ok(Json(jobs.into_iter().map(wake_view).collect()));
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        waited += 1;
    }
}

#[derive(Serialize)]
struct HostStatusView {
    machine: String,
    /// Whether the host daemon of the caller's machine asked for work lately.
    up: bool,
    /// Seconds since its last request; absent when it never asked.
    last_poll_secs_ago: Option<i64>,
}

/// Whether the host daemon of the caller's own machine is answering: what
/// `chappe doctor` shows.
async fn host_status(
    State(state): State<Shared>,
    Authenticated(agent): Authenticated,
) -> Result<Json<HostStatusView>, ApiError> {
    let now = now();
    let ago = state
        .store
        .host_last_poll(&agent.machine)?
        .map(|t| (now - t).max(0));
    Ok(Json(HostStatusView {
        machine: agent.machine,
        up: ago.is_some_and(|a| a < crate::store::Store::HOST_ALIVE_SECS),
        last_poll_secs_ago: ago,
    }))
}

/// The outcome of a wake.
async fn host_report(
    State(state): State<Shared>,
    HostAuth(host): HostAuth,
    Path(id): Path<i64>,
    body: Result<Json<Finish>, JsonRejection>,
) -> Result<Json<StateView>, ApiError> {
    let Json(finish) =
        body.map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.body_text()))?;
    state.store.finish_wake(&host, id, finish, now())?;
    Ok(Json(StateView { state: "ok" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::GiteaFeed;
    use crate::web::testutil::*;

    async fn register(app: &Router, token: &str, body: &str) -> String {
        let (status, _, out) =
            call(app, api("POST", "/v1/sessions", Some(token), Some(body))).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        let start = out.find(r#""handle":""#).unwrap() + 10;
        out[start..start + out[start..].find('"').unwrap()].to_string()
    }

    #[tokio::test]
    async fn token_is_accepted() {
        let (app, _) = app();
        let (status, _, body) = call(&app, api("GET", "/v1/whoami", Some(TOKEN), None)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"machine":"host-a","tool":"claude"}"#);
    }

    #[tokio::test]
    async fn missing_wrong_or_malformed_token_is_rejected() {
        let (app, _) = app();
        for authorization in [
            None,
            Some("Bearer nope"),
            Some("Basic abc"),
            Some("Bearer "),
            Some(TOKEN),
        ] {
            let mut req = axum::http::Request::get("/v1/whoami");
            if let Some(value) = authorization {
                req = req.header(header::AUTHORIZATION, value);
            }
            let (status, headers, body) =
                call(&app, req.body(axum::body::Body::empty()).unwrap()).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{authorization:?}");
            assert_eq!(headers[header::WWW_AUTHENTICATE], "Bearer");
            assert!(!body.contains(TOKEN));
        }
    }

    #[tokio::test]
    async fn agent_interface_does_not_expose_the_owner_page() {
        let (app, _) = app();
        assert_eq!(call(&app, get_req("/v1/")).await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn registry_interface_needs_a_token() {
        let (app, _) = app();
        for (method, uri) in [
            ("POST", "/v1/sessions"),
            ("GET", "/v1/sessions"),
            ("POST", "/v1/sessions/x-0000/beat"),
            ("DELETE", "/v1/sessions/x-0000"),
            ("POST", "/v1/topics"),
            ("GET", "/v1/topics"),
        ] {
            let body = (method == "POST" && uri != "/v1/sessions/x-0000/beat")
                .then_some(r#"{"roles":["a"],"project":"p"}"#);
            for token in [None, Some("wrong")] {
                let (status, ..) = call(&app, api(method, uri, token, body)).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri} {token:?}");
            }
        }
    }

    #[tokio::test]
    async fn beat_and_leave_belong_to_the_registering_machine() {
        let (app, _) = app();
        let h = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let beat = format!("/v1/sessions/{h}/beat");
        assert_eq!(
            call(&app, api("POST", &beat, Some(TOKEN), None)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, api("POST", &beat, Some(TOKEN_MAC), None))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let leave = format!("/v1/sessions/{h}");
        assert_eq!(
            call(&app, api("DELETE", &leave, Some(TOKEN_MAC), None))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &app,
                api("POST", "/v1/sessions/nope-0000/beat", Some(TOKEN), None)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, api("DELETE", &leave, Some(TOKEN), None)).await.0,
            StatusCode::OK
        );
        assert_eq!(
            call(&app, api("DELETE", &leave, Some(TOKEN), None)).await.0,
            StatusCode::OK,
            "leaving twice is fine"
        );
        assert_eq!(
            call(&app, api("POST", &beat, Some(TOKEN), None)).await.0,
            StatusCode::GONE
        );
    }

    #[tokio::test]
    async fn bad_registrations_are_refused_with_a_json_reason() {
        let (app, state) = app();
        for (body, reason) in [
            (r#"{"roles":[],"project":"p"}"#, "at least one role"),
            (r#"{"roles":["Bad Role"],"project":"p"}"#, "role"),
            (r#"{"roles":["a"],"project":"P P"}"#, "project"),
            (
                r#"{"roles":["a"],"project":"p","agent":"claude@other"}"#,
                "unknown field",
            ),
            (r#"{"roles":"a","project":"p"}"#, "roles"),
            ("not json", ""),
        ] {
            let (status, headers, out) =
                call(&app, api("POST", "/v1/sessions", Some(TOKEN), Some(body))).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
            assert!(
                headers[header::CONTENT_TYPE]
                    .to_str()
                    .unwrap()
                    .starts_with("application/json")
            );
            assert!(
                out.starts_with(r#"{"error":""#) && out.contains(reason),
                "{out}"
            );
        }
        let secret = format!(
            r#"{{"roles":["a"],"project":"p","worktree":"/x/{}"}}"#,
            "ab".repeat(32)
        );
        let (status, _, out) = call(
            &app,
            api("POST", "/v1/sessions", Some(TOKEN), Some(&secret)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(out.contains("looks like a secret") && !out.contains(&"ab".repeat(32)));
        assert!(
            state
                .store
                .sessions(Filter::default(), now())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn who_filters_by_project_and_role_and_hides_private_fields() {
        let (app, _) = app();
        register(
            &app,
            TOKEN,
            r#"{"roles":["browser"],"project":"demo","session_id":"sid-1","transcript":"/t"}"#,
        )
        .await;
        register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"core"}"#).await;
        let count = |out: &str| out.matches(r#""handle""#).count();
        let get = |q: &str| api("GET", &format!("/v1/sessions{q}"), Some(TOKEN), None);
        let (_, _, all) = call(&app, get("")).await;
        assert_eq!(count(&all), 2);
        assert!(
            all.contains(r#""agent":"codex@mac""#)
                && !all.contains("transcript")
                && !all.contains("session_id")
        );
        assert_eq!(count(&call(&app, get("?project=demo")).await.2), 1);
        assert_eq!(count(&call(&app, get("?role=audio")).await.2), 1);
        assert_eq!(
            count(&call(&app, get("?project=demo&role=audio")).await.2),
            0
        );
    }

    #[tokio::test]
    async fn the_session_name_is_listed_and_follows_a_rename_through_the_inbox() {
        let (app, _) = app();
        let h = register(
            &app,
            TOKEN,
            r#"{"roles":["audio"],"project":"demo","name":"Graphics and sound"}"#,
        )
        .await;
        let who = || call(&app, api("GET", "/v1/sessions", Some(TOKEN), None));
        assert!(who().await.2.contains(r#""name":"Graphics and sound""#));
        let inbox = |name: &str| {
            api(
                "POST",
                "/v1/inbox",
                Some(TOKEN),
                Some(&format!(r#"{{"handle":"{h}","name":"{name}"}}"#)),
            )
        };
        let (status, ..) = call(&app, inbox("Audio")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(who().await.2.contains(r#""name":"Audio""#));
        // A name the service cannot take does not cost the session its inbox.
        let (status, ..) = call(&app, inbox(&"x".repeat(80))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(who().await.2.contains(r#""name":"Audio""#));
    }

    #[tokio::test]
    async fn reading_the_inbox_tells_the_page_that_the_ask_was_delivered() {
        let (app, state) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"updated the driver","expect":"works or not","by":"30m"}}"#
        );
        let (status, _) = post(&app, TOKEN, "/v1/messages", &ask).await;
        assert_eq!(status, StatusCode::CREATED);
        let mut stream = state.events.subscribe();
        while stream.try_recv().is_ok() {}
        let (status, inbox) = post(
            &app,
            TOKEN_MAC,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{inbox}");
        assert!(inbox.contains("check the sound"));
        let heard: Vec<_> = std::iter::from_fn(|| stream.try_recv().ok()).collect();
        assert_eq!(
            heard,
            ["msg:demo/general"],
            "once, for the topic of the ask"
        );
    }

    #[tokio::test]
    async fn the_inbox_call_tells_where_the_session_is_and_who_shows_its_claimed_issue() {
        let (app, state) = app();
        let h = register(&app, TOKEN, r#"{"roles":["dev"],"project":"core"}"#).await;
        let claim = serde_json::json!({
            "action": "created",
            "comment": {"id": 1, "body": "<!-- agent-lock: claimed -->\n- branch: `claude/x`"},
            "issue": {"number": 618, "title": "board"},
            "repository": {"full_name": "acme/core"},
        });
        state
            .store
            .gitea_event("issue_comment", "d1", &claim, now())
            .unwrap();
        let who = || call(&app, api("GET", "/v1/sessions", Some(TOKEN), None));
        assert!(who().await.2.contains(r#""issue":null"#));
        let mut stream = state.events.subscribe();
        while stream.try_recv().is_ok() {}
        let body = format!(r#"{{"handle":"{h}","worktree":"/w/x","branch":"claude/x"}}"#);
        let (status, _) = post(&app, TOKEN, "/v1/inbox", &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(stream.try_recv().ok().as_deref(), Some("board"));
        assert!(who().await.2.contains(r##""issue":"#618 board""##));
        // The same place again changes nothing; a bad value does not cost the inbox.
        post(&app, TOKEN, "/v1/inbox", &body).await;
        assert!(stream.try_recv().is_err());
        let bad = format!(r#"{{"handle":"{h}","branch":"{}"}}"#, "x".repeat(200));
        assert_eq!(post(&app, TOKEN, "/v1/inbox", &bad).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_session_that_registers_is_told_who_else_works_there_and_what_is_held() {
        let (app, state) = app();
        let first = api(
            "POST",
            "/v1/sessions",
            Some(TOKEN),
            Some(r#"{"roles":["dev"],"project":"core","name":"First","branch":"claude/x"}"#),
        );
        let (_, _, alone) = call(&app, first).await;
        assert!(
            !alone.contains("briefing"),
            "nobody else, nothing held: {alone}"
        );
        let claim = serde_json::json!({
            "action": "created",
            "comment": {"id": 1, "body": "<!-- agent-lock: claimed -->\n- branch: `claude/x`"},
            "issue": {"number": 618, "title": "board"},
            "repository": {"full_name": "acme/core"},
        });
        state
            .store
            .gitea_event("issue_comment", "d1", &claim, now())
            .unwrap();
        let lease = serde_json::json!({
            "action": "created",
            "comment": {"id": 2, "body": "<!-- resource-lease: exclusive -->\nagent: codex@mac\naction: reboot\nuntil: 2099-01-01 10:00 UTC"},
            "issue": {"number": 480, "title": "Laptop"},
            "repository": {"full_name": "acme/demo"},
        });
        state
            .store
            .gitea_event("issue_comment", "d2", &lease, now())
            .unwrap();
        let second = api(
            "POST",
            "/v1/sessions",
            Some(TOKEN_MAC),
            Some(r#"{"roles":["dev"],"project":"core"}"#),
        );
        let (_, _, out) = call(&app, second).await;
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let text = v["briefing"].as_str().unwrap();
        assert!(text.contains("Working in project core now:"), "{text}");
        assert!(
            text.contains("\"First\" (claude@host-a), task: #618 board, branch claude/x, active"),
            "{text}"
        );
        assert!(text.contains("Resource \"Laptop\" busy (exclusive lease): codex@mac, until 10:00 UTC (acme/demo#480)."), "{text}");
        assert!(
            !text.contains(v["handle"].as_str().unwrap()),
            "not about itself"
        );
        // Another project sees the lease but not those sessions.
        let third = api(
            "POST",
            "/v1/sessions",
            Some(TOKEN),
            Some(r#"{"roles":["dev"],"project":"other"}"#),
        );
        let (_, _, out) = call(&app, third).await;
        assert!(out.contains("Laptop") && !out.contains("First"), "{out}");
    }

    #[tokio::test]
    async fn a_session_creates_a_topic_in_its_own_project() {
        let (app, state) = app();
        let mut events = state.events.subscribe();
        let h = register(&app, TOKEN, r#"{"roles":["audio"],"project":"demo"}"#).await;
        while events.try_recv().is_ok() {}
        let body = format!(r#"{{"handle":"{h}","name":"Audio Driver","about":"sound driver"}}"#);
        let (status, _, out) =
            call(&app, api("POST", "/v1/topics", Some(TOKEN), Some(&body))).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        assert!(out.contains(r#""project":"demo""#) && out.contains(r#""name":"audio-driver""#));
        assert_eq!(events.try_recv().unwrap(), "nav");
        let (_, _, page) = call(&app, get_req("/p/demo/audio-driver")).await;
        assert!(page.contains("sound driver"));
        let (_, _, list) = call(
            &app,
            api("GET", "/v1/topics?project=demo", Some(TOKEN), None),
        )
        .await;
        assert!(
            list.contains("audio-driver")
                && list.contains(r#""name":"general""#)
                && !list.contains(r#""project":"all""#)
        );
        let (_, _, every) = call(&app, api("GET", "/v1/topics", Some(TOKEN), None)).await;
        assert!(every.contains(r#""project":"all""#));
    }

    #[tokio::test]
    async fn topic_creation_is_checked() {
        let (app, _) = app();
        let h = register(&app, TOKEN, r#"{"roles":["audio"],"project":"demo"}"#).await;
        let create = |token: &str, handle: &str, name: &str| {
            api(
                "POST",
                "/v1/topics",
                Some(token),
                Some(&format!(r#"{{"handle":"{handle}","name":"{name}"}}"#)),
            )
        };
        assert_eq!(
            call(&app, create(TOKEN, &h, "general")).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(&app, create(TOKEN, &h, "a/b")).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            call(&app, create(TOKEN, "nope-0000", "x")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, create(TOKEN_MAC, &h, "x")).await.0,
            StatusCode::FORBIDDEN,
            "not your session"
        );
        for i in 0..3 {
            assert_eq!(
                call(&app, create(TOKEN, &h, &format!("t{i}"))).await.0,
                StatusCode::CREATED
            );
        }
        let (status, _, out) = call(&app, create(TOKEN, &h, "t3")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(out.contains("no more than three"));
        let extra = api(
            "POST",
            "/v1/topics",
            Some(TOKEN),
            Some(&format!(r#"{{"handle":"{h}","name":"z","project":"all"}}"#)),
        );
        assert_eq!(
            call(&app, extra).await.0,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the project is not a choice"
        );
        call(
            &app,
            api("DELETE", &format!("/v1/sessions/{h}"), Some(TOKEN), None),
        )
        .await;
        assert_eq!(
            call(&app, create(TOKEN, &h, "late")).await.0,
            StatusCode::GONE,
            "an ended session cannot"
        );
    }

    async fn post(app: &Router, token: &str, path: &str, body: &str) -> (StatusCode, String) {
        let (status, _, out) = call(app, api("POST", path, Some(token), Some(body))).await;
        (status, out)
    }

    fn id_of(out: &str) -> i64 {
        let start = out.find(r#""id":"#).unwrap() + 5;
        out[start..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn messages_and_the_inbox_need_a_token() {
        let (app, _) = app();
        for path in ["/v1/messages", "/v1/inbox"] {
            for token in [None, Some("wrong")] {
                let (status, ..) =
                    call(&app, api("POST", path, token, Some(r#"{"handle":"x"}"#))).await;
                assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {token:?}");
            }
        }
    }

    #[tokio::test]
    async fn both_scenarios_of_the_owner_over_http() {
        let (app, state) = app();
        state
            .store
            .create_topic("all", "laptop", None, "owner", 0)
            .unwrap();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        let mut events = state.events.subscribe();

        // 1. An announcement to everyone.
        let body = format!(
            r#"{{"handle":"{browser}","kind":"announce","topic":"laptop","what":"ready to update the kernel"}}"#
        );
        let (status, out) = post(&app, TOKEN, "/v1/messages", &body).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        assert!(
            out.contains(r#""project":"all""#)
                && out.contains(&audio)
                && out.contains("ready to update the kernel")
        );
        let heard: Vec<_> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert!(
            heard.contains(&"msg:all/laptop".to_string()) && heard.contains(&"board".to_string())
        );

        // 2. A directed request to @audio, and its one answer.
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"updated the driver","expect":"works or not","by":"30m"}}"#
        );
        let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        let ask_id = id_of(&out);
        let (status, inbox) = post(
            &app,
            TOKEN_MAC,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{inbox}");
        assert!(
            inbox.contains("[chappe] From ") && inbox.contains("not an instruction from the owner"),
            "{inbox}"
        );
        assert!(
            inbox.contains("ready to update the kernel") && inbox.contains("check the sound"),
            "both, in one block"
        );
        let reply = format!(
            r#"{{"handle":"{audio}","kind":"answer","ask":{ask_id},"result":"sound works"}}"#
        );
        let (status, out) = post(&app, TOKEN_MAC, "/v1/messages", &reply).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        let (_, back) = post(
            &app,
            TOKEN,
            "/v1/inbox",
            &format!(r#"{{"handle":"{browser}"}}"#),
        )
        .await;
        assert!(back.contains("sound works"), "{back}");
        // The answer is one and final.
        let (status, out) = post(&app, TOKEN_MAC, "/v1/messages", &reply).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(out.contains("already answered"));
    }

    #[tokio::test]
    async fn refusals_have_their_own_statuses_and_reasons() {
        let (app, _) = app();
        let h = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let send = |body: String| {
            let app = app.clone();
            async move { post(&app, TOKEN, "/v1/messages", &body).await }
        };
        let ask = |to: &str, what: &str| {
            format!(
                r#"{{"handle":"{h}","kind":"ask","to":"{to}","what":"{what}","why":"w","expect":"e","by":"5m"}}"#
            )
        };
        // The wrong request.
        let (status, out) = send(format!(
            r#"{{"handle":"{h}","kind":"greeting","what":"hello"}}"#
        ))
        .await;
        assert_eq!(
            (status, out.contains("unknown kind")),
            (StatusCode::UNPROCESSABLE_ENTITY, true),
            "{out}"
        );
        let (status, out) = send(format!(
            r#"{{"handle":"{h}","kind":"announce","what":"x","agent":"a"}}"#
        ))
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
        assert!(out.starts_with(r#"{"error":""#));
        // Nobody to send to.
        let (status, out) = send(ask("@kernel", "x")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(out.contains("nobody is working on @kernel"), "{out}");
        // A message that looks like a secret is refused whoever it is for.
        let (status, out) = send(ask("@kernel", &"ab".repeat(20))).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{out}");
        assert!(out.contains("token or a private key") && !out.contains(&"ab".repeat(20)));
        // Over a budget, with the reason.
        for i in 0..6 {
            let owner = format!(
                r#"{{"handle":"{h}","kind":"owner","what":"question {i}","why":"w","expect":"e"}}"#
            );
            assert_eq!(send(owner).await.0, StatusCode::CREATED);
        }
        let over =
            format!(r#"{{"handle":"{h}","kind":"owner","what":"seventh","why":"w","expect":"e"}}"#);
        let (status, out) = send(over).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(out.contains("6 messages per hour"), "{out}");
        // Another machine's session, and a session that has ended.
        let (status, ..) = post(
            &app,
            TOKEN_MAC,
            "/v1/messages",
            &format!(r#"{{"handle":"{h}","kind":"announce","what":"x"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        call(
            &app,
            api("DELETE", &format!("/v1/sessions/{h}"), Some(TOKEN), None),
        )
        .await;
        let (status, ..) = post(
            &app,
            TOKEN,
            "/v1/messages",
            &format!(r#"{{"handle":"{h}","kind":"announce","what":"y"}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::GONE);
        let (status, ..) = post(&app, TOKEN, "/v1/inbox", &format!(r#"{{"handle":"{h}"}}"#)).await;
        assert_eq!(status, StatusCode::GONE);
    }

    #[tokio::test]
    async fn a_dry_run_says_who_would_get_it_and_sends_nothing() {
        let (app, state) = app();
        let browser = register(
            &app,
            TOKEN,
            r##"{"roles":["browser"],"project":"demo","issue":"#471"}"##,
        )
        .await;
        let audio = register(
            &app,
            TOKEN_MAC,
            r##"{"roles":["audio","desktop"],"project":"demo","issue":"#486"}"##,
        )
        .await;
        let body = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"x","why":"y","expect":"z","by":"1h","dry_run":true}}"#
        );
        let (status, out) = post(&app, TOKEN, "/v1/messages", &body).await;
        assert_eq!(status, StatusCode::OK, "{out}");
        assert!(
            out.contains(r#""dry_run":true"#)
                && out.contains(r#""id":null"#)
                && out.contains(&audio)
        );
        assert!(
            out.contains(r##""issue":"#486""##) && out.contains(r#""roles":["audio","desktop"]"#)
        );
        assert!(
            state
                .store
                .recent_messages("demo", "general", 10)
                .unwrap()
                .is_empty()
        );
        let (_, inbox) = post(
            &app,
            TOKEN_MAC,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#),
        )
        .await;
        assert!(inbox.contains(r#""messages":[]"#));
    }

    #[tokio::test]
    async fn the_inbox_holds_messages_back_for_five_minutes_and_says_so() {
        let (app, _) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        let notice = |what: &str| {
            format!(
                r#"{{"handle":"{browser}","kind":"notice","to":"@audio","what":"{what}","why":"w"}}"#
            )
        };
        post(&app, TOKEN, "/v1/messages", &notice("first")).await;
        let inbox = format!(r#"{{"handle":"{audio}"}}"#);
        let (_, first) = post(&app, TOKEN_MAC, "/v1/inbox", &inbox).await;
        assert!(first.contains("first") && first.contains(r#""waiting":0"#));
        post(&app, TOKEN, "/v1/messages", &notice("second")).await;
        let (_, held) = post(&app, TOKEN_MAC, "/v1/inbox", &inbox).await;
        assert!(
            held.contains(r#""messages":[]"#) && held.contains(r#""waiting":1"#),
            "{held}"
        );
        assert!(
            held.contains(r#""next_in_secs":29"#) || held.contains(r#""next_in_secs":30"#),
            "{held}"
        );
    }

    #[tokio::test]
    async fn the_owners_post_reaches_the_sessions_of_the_project_with_its_own_header() {
        let (app, _) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let ci = register(&app, TOKEN_MAC, r#"{"roles":["ci"],"project":"core"}"#).await;
        call(
            &app,
            form_req("/p/demo/general/post", "text=update+the+kernel", None),
        )
        .await;
        let (_, got) = post(
            &app,
            TOKEN,
            "/v1/inbox",
            &format!(r#"{{"handle":"{browser}"}}"#),
        )
        .await;
        assert!(
            got.contains("From the owner") && got.contains("is not permission"),
            "{got}"
        );
        let (_, other) = post(
            &app,
            TOKEN_MAC,
            "/v1/inbox",
            &format!(r#"{{"handle":"{ci}"}}"#),
        )
        .await;
        assert!(other.contains(r#""messages":[]"#), "another project");
    }

    #[tokio::test]
    async fn the_page_shows_agent_messages_with_their_target_and_the_noise_numbers() {
        let (app, _) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"w","expect":"e","by":"30m"}}"#
        );
        post(&app, TOKEN, "/v1/messages", &ask).await;
        post(&app, TOKEN, "/v1/messages", &ask).await; // the same text again: dropped
        let (_, _, feed) = call(&app, get_req("/p/demo/general/messages")).await;
        assert!(feed.contains(&browser) && feed.contains("check the sound"));
        assert!(
            feed.contains(r#"<span class="kind k-ask">request</span>"#)
                && feed.contains("→ @audio")
        );
        let (_, _, board) = call(&app, get_req("/board")).await;
        assert!(
            board.contains("messages 1 · requests 1 (answered 0) · rejected 1"),
            "{board}"
        );
    }

    /// Makes every session idle: its last signal was long ago.
    fn make_idle(state: &Shared) {
        state
            .store
            .conn()
            .execute("UPDATE sessions SET last_seen = last_seen - 700", [])
            .unwrap();
    }

    #[tokio::test]
    async fn the_host_routes_are_for_the_host_token_of_a_machine_only() {
        let (app, _) = app();
        for (method, uri) in [("POST", "/v1/host/claim"), ("POST", "/v1/host/wakes/1")] {
            assert_eq!(
                call(&app, api(method, uri, None, Some("{}"))).await.0,
                StatusCode::UNAUTHORIZED,
                "{uri}"
            );
            for token in [TOKEN, TOKEN_MAC] {
                let (status, _, out) = call(&app, api(method, uri, Some(token), Some("{}"))).await;
                assert_eq!(
                    status,
                    StatusCode::FORBIDDEN,
                    "{uri}: an agent token is no host token"
                );
                assert!(out.contains("host daemon"));
            }
        }
        // And the host token has no business with the agents' routes that need a session.
        let (status, ..) = call(
            &app,
            api("POST", "/v1/host/claim", Some(TOKEN_HOST), Some("{}")),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_host_reports_processes_and_a_session_without_one_drops_off_the_board() {
        let (app, state) = app();
        let audio = register(
            &app,
            TOKEN_MAC,
            r#"{"roles":["audio"],"project":"demo","session_id":"sid-9"}"#,
        )
        .await;
        let who = |app: &Router| {
            let app = app.clone();
            async move {
                let (_, _, out) = call(&app, api("GET", "/v1/sessions", Some(TOKEN), None)).await;
                out
            }
        };
        state
            .store
            .conn()
            .execute("UPDATE sessions SET last_seen = last_seen - 8000", [])
            .unwrap();
        // The process is there: silence alone does not end the session.
        let alive = r#"{"running":{"codex":["sid-9"]}}"#;
        let (status, _) = post(&app, TOKEN_HOST, "/v1/host/claim", alive).await;
        assert_eq!(status, StatusCode::OK);
        assert!(who(&app).await.contains(&audio));
        // The process is gone: after two hours of silence the session is gone too.
        let (status, _) = post(
            &app,
            TOKEN_HOST,
            "/v1/host/claim",
            r#"{"running":{"codex":[]}}"#,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(!who(&app).await.contains(&audio));
        // An agent token cannot make such a report.
        let (status, _) = post(&app, TOKEN_MAC, "/v1/host/claim", alive).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_agent_asks_whether_the_host_daemon_of_its_machine_answers() {
        let (app, _state) = app();
        let (status, _, out) =
            call(&app, api("GET", "/v1/host/status", Some(TOKEN_MAC), None)).await;
        assert_eq!(status, StatusCode::OK, "{out}");
        assert!(
            out.contains(r#""machine":"mac""#) && out.contains(r#""up":false"#),
            "{out}"
        );
        assert!(out.contains(r#""last_poll_secs_ago":null"#), "{out}");
        post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        let (_, _, out) = call(&app, api("GET", "/v1/host/status", Some(TOKEN_MAC), None)).await;
        assert!(out.contains(r#""up":true"#), "{out}");
        let (status, _, _) = call(&app, api("GET", "/v1/host/status", None, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_ask_to_an_idle_session_is_woken_by_the_host_over_http() {
        let (app, state) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let audio = register(
            &app,
            TOKEN_MAC,
            r#"{"roles":["audio"],"project":"demo","session_id":"sid-9","worktree":"/work/audio"}"#,
        )
        .await;
        make_idle(&state);
        // The host daemon of the mac is up: it has just asked for work.
        post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        // The browser asks; both sessions are idle, but the sender is the one that just spoke.
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"w","expect":"e","by":"2h","wait":true}}"#
        );
        let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        assert!(out.contains(r#""wake":"requested""#), "{out}");

        // The host of the mac claims it, with what it needs to wake the session.
        let (status, claimed) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            claimed.contains(&format!(r#""handle":"{audio}""#))
                && claimed.contains(r#""tool":"codex""#),
            "{claimed}"
        );
        assert!(
            claimed.contains(r#""session_id":"sid-9""#)
                && claimed.contains(r#""worktree":"/work/audio""#)
        );
        assert!(claimed.contains("[chappe] From ") && claimed.contains("check the sound"));
        let id = id_of(&claimed);
        let report = format!(
            r#"{{"outcome":"delivered","message_ids":[{}]}}"#,
            id_of(&out)
        );
        let (status, _) = post(&app, TOKEN_HOST, &format!("/v1/host/wakes/{id}"), &report).await;
        assert_eq!(status, StatusCode::OK);
        // The session has its message; its inbox has nothing more.
        let (_, inbox) = post(
            &app,
            TOKEN_MAC,
            "/v1/inbox",
            &format!(r#"{{"handle":"{audio}"}}"#),
        )
        .await;
        assert!(inbox.contains(r#""messages":[]"#), "{inbox}");
        let (_, again) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        assert_eq!(again, "[]");
    }

    #[tokio::test]
    async fn the_third_wake_in_an_hour_is_queued_and_the_sender_is_told_on_the_spot() {
        let (app, state) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        for i in 0..3 {
            make_idle(&state);
            let ask = format!(
                r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"question {i}","why":"w","expect":"e","by":"2h"}}"#
            );
            let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
            assert_eq!(status, StatusCode::CREATED, "{out}");
            let expected = if i < 2 { "requested" } else { "limit" };
            assert!(
                out.contains(&format!(r#""wake":"{expected}""#)),
                "{i}: {out}"
            );
            let (_, claimed) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
            if i < 2 {
                let report = format!(
                    r#"{{"outcome":"delivered","message_ids":[{}]}}"#,
                    id_of(&out)
                );
                post(
                    &app,
                    TOKEN_HOST,
                    &format!("/v1/host/wakes/{}", id_of(&claimed)),
                    &report,
                )
                .await;
            }
        }
    }

    #[tokio::test]
    async fn a_wake_that_cannot_be_done_leaves_a_note_in_the_senders_inbox() {
        let (app, state) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let audio = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        make_idle(&state);
        post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"x","why":"w","expect":"e","by":"2h"}}"#
        );
        post(&app, TOKEN, "/v1/messages", &ask).await;
        let (_, claimed) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        let report = r#"{"outcome":"unsupported","reason":"no adapter for codex"}"#;
        post(
            &app,
            TOKEN_HOST,
            &format!("/v1/host/wakes/{}", id_of(&claimed)),
            report,
        )
        .await;
        let (_, inbox) = post(
            &app,
            TOKEN,
            "/v1/inbox",
            &format!(r#"{{"handle":"{browser}"}}"#),
        )
        .await;
        assert!(
            inbox.contains("has no way to wake") && inbox.contains(&audio),
            "{inbox}"
        );
        // A report that is not one of the four is refused.
        let (status, _) = post(
            &app,
            TOKEN_HOST,
            "/v1/host/wakes/1",
            r#"{"outcome":"explode"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, _) = post(
            &app,
            TOKEN_HOST,
            "/v1/host/wakes/999",
            r#"{"outcome":"deferred","reason":"x"}"#,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn without_a_host_the_sender_is_told_there_is_nobody_to_wake_the_session() {
        let (app, state) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        make_idle(&state);
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"x","why":"w","expect":"e","by":"2h"}}"#
        );
        let (status, out) = post(&app, TOKEN, "/v1/messages", &ask).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        assert!(out.contains(r#""wake":"no_host""#), "{out}");
        // No wake was made while nobody could carry it out. A host that comes up
        // afterwards finds the ask still waiting, and the session is woken then.
        let (_, jobs) = post(&app, TOKEN_HOST, "/v1/host/claim", "{}").await;
        assert!(
            jobs.contains(r#""tool":"codex""#) && jobs.contains("Need: x"),
            "{jobs}"
        );
    }

    #[tokio::test]
    async fn a_long_poll_waits_and_comes_back_empty() {
        let (app, _) = app();
        let started = std::time::Instant::now();
        let (status, out) = post(&app, TOKEN_HOST, "/v1/host/claim", r#"{"wait_secs":1}"#).await;
        assert_eq!((status, out.as_str()), (StatusCode::OK, "[]"));
        assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    }
}
