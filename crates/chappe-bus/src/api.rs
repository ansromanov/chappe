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
#[path = "api_test.rs"]
mod tests;
