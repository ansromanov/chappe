//! The owner's page: the board, projects and topics, the feed of a topic.
//!
//! The ingress puts all of this behind the single sign-on; the service only
//! checks that a post comes from its own page.

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use serde::Deserialize;

use crate::chat::NewMessage;
use crate::html;
use crate::message::{GENERAL, Kind, OWNER};
use crate::registry::{Filter, Session};
use crate::store::{StoreError, now};
use crate::web::{Shared, server_error};

/// Messages shown per topic on the page.
const PAGE_MESSAGES: usize = 200;

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/", get(board_page))
        .route("/board", get(board))
        .route("/nav", get(nav))
        .route("/r/{project}", get(old_room))
        .route("/m/{id}", get(message))
        .route("/p/{project}", get(project_home))
        .route("/p/{project}/people", get(people))
        .route("/p/{project}/topics", post(create_topic))
        .route("/p/{project}/{topic}", get(topic_page))
        .route("/p/{project}/{topic}/messages", get(messages))
        .route("/p/{project}/{topic}/post", post(post_message))
}

fn not_found(what: &str) -> Response {
    (StatusCode::NOT_FOUND, what.to_string()).into_response()
}

fn topic_url(project: &str, topic: &str) -> String {
    format!(
        "/p/{}/{}",
        html::path_escape(project),
        html::path_escape(topic)
    )
}

/// Sessions to show next to a project: its own, or all of them for `all`.
fn people_of(sessions: Vec<Session>, project: &str) -> Vec<Session> {
    if project == "all" {
        sessions
    } else {
        sessions
            .into_iter()
            .filter(|s| s.project == project)
            .collect()
    }
}

async fn board_page(State(state): State<Shared>) -> Response {
    let now = now();
    let nav = match state.store.nav() {
        Ok(n) => n,
        Err(e) => return server_error(e),
    };
    let counters = match state.store.counters(now) {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let leases = match state.store.leases(now) {
        Ok(l) => l,
        Err(e) => return server_error(e),
    };
    let claims = match state.store.claims() {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let answers = match state.store.answer_stats(now) {
        Ok(a) => a,
        Err(e) => return server_error(e),
    };
    match state.store.sessions(Filter::default(), now) {
        Ok(sessions) => Html(html::board_page(
            &nav, &sessions, &leases, &claims, &counters, &answers, now,
        ))
        .into_response(),
        Err(e) => server_error(e),
    }
}

/// The board alone, for the page to refresh in place.
async fn board(State(state): State<Shared>) -> Response {
    let now = now();
    let counters = match state.store.counters(now) {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let leases = match state.store.leases(now) {
        Ok(l) => l,
        Err(e) => return server_error(e),
    };
    let claims = match state.store.claims() {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let answers = match state.store.answer_stats(now) {
        Ok(a) => a,
        Err(e) => return server_error(e),
    };
    match state.store.sessions(Filter::default(), now) {
        Ok(sessions) => Html(html::board_fragment_with(
            &sessions, &leases, &claims, &counters, &answers, now,
        ))
        .into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct NavQuery {
    project: Option<String>,
    topic: Option<String>,
}

/// The sidebar alone, for the page to refresh when a topic is created.
async fn nav(State(state): State<Shared>, Query(q): Query<NavQuery>) -> Response {
    match state.store.nav() {
        Ok(nav) => {
            let current = q.project.as_deref().zip(q.topic.as_deref());
            Html(html::nav_fragment(&nav, current)).into_response()
        }
        Err(e) => server_error(e),
    }
}

/// `/r/<room>` was the address of a room before rooms became projects.
async fn old_room(Path(project): Path<String>) -> Redirect {
    Redirect::permanent(&topic_url(&project, GENERAL))
}

/// The address of a message by its number, for a link that is not on the page:
/// the topic it was written in, at that message.
async fn message(State(state): State<Shared>, Path(id): Path<i64>) -> Response {
    match state.store.message_place(id) {
        Ok(Some((project, topic))) => {
            Redirect::to(&format!("{}#m{id}", topic_url(&project, &topic))).into_response()
        }
        Ok(None) => not_found("no such message"),
        Err(e) => server_error(e),
    }
}

async fn project_home(State(state): State<Shared>, Path(project): Path<String>) -> Response {
    match state.store.topic(&project, GENERAL) {
        Ok(Some(_)) => Redirect::to(&topic_url(&project, GENERAL)).into_response(),
        Ok(None) => not_found("no such project"),
        Err(e) => server_error(e),
    }
}

async fn topic_page(
    State(state): State<Shared>,
    Path((project, topic)): Path<(String, String)>,
) -> Response {
    let now = now();
    let info = match state.store.topic(&project, &topic) {
        Ok(Some(info)) => info,
        Ok(None) => return not_found("no such topic"),
        Err(e) => return server_error(e),
    };
    let nav = match state.store.nav() {
        Ok(nav) => nav,
        Err(e) => return server_error(e),
    };
    let messages = match state.store.recent_messages(&project, &topic, PAGE_MESSAGES) {
        Ok(messages) => messages,
        Err(e) => return server_error(e),
    };
    let sessions = match state.store.sessions(Filter::default(), now) {
        Ok(sessions) => sessions,
        Err(e) => return server_error(e),
    };
    let people = people_of(sessions, &project);
    Html(html::topic_page(
        &nav, &project, &info, &messages, &people, now,
    ))
    .into_response()
}

async fn messages(
    State(state): State<Shared>,
    Path((project, topic)): Path<(String, String)>,
) -> Response {
    match state.store.topic(&project, &topic) {
        Ok(Some(_)) => match state.store.recent_messages(&project, &topic, PAGE_MESSAGES) {
            Ok(messages) => Html(html::messages_fragment(&messages, now())).into_response(),
            Err(e) => server_error(e),
        },
        Ok(None) => not_found("no such topic"),
        Err(e) => server_error(e),
    }
}

/// The people panel of a project.
async fn people(State(state): State<Shared>, Path(project): Path<String>) -> Response {
    let now = now();
    match state.store.sessions(Filter::default(), now) {
        Ok(sessions) => {
            Html(html::people_fragment(&people_of(sessions, &project), now)).into_response()
        }
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct PostForm {
    text: String,
}

#[derive(Deserialize)]
struct TopicForm {
    name: String,
    about: Option<String>,
}

/// The owner's cookie is sent with any request the browser makes, including
/// one a foreign page triggers, so a post must come from this site.
fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let origin_host = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, authority)| authority);
    matches!((origin_host, host), (Some(o), Some(h)) if o.eq_ignore_ascii_case(h))
}

/// A refusal shown inside the page, with a way back.
fn refusal(state: &Shared, status: StatusCode, reason: &str, back: &str) -> Response {
    let nav = state.store.nav().unwrap_or_default();
    let page = html::error_page(&nav, "That didn't work", reason, back);
    (status, Html(page)).into_response()
}

async fn post_message(
    State(state): State<Shared>,
    Path((project, topic)): Path<(String, String)>,
    headers: HeaderMap,
    Form(form): Form<PostForm>,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "foreign origin").into_response();
    }
    let new = NewMessage {
        project: &project,
        topic: &topic,
        sender: OWNER,
        kind: Kind::Announce,
        text: &form.text,
    };
    match state.store.add_message(&new) {
        Ok(message) => {
            // The sessions of the project read it at their next checkpoint.
            if let Err(e) = state.store.deliver_owner_message(&message, now()) {
                eprintln!("chappe-bus: delivering the owner's message: {e}");
            }
            state.notify(format!("msg:{}/{}", message.project, message.topic));
            Redirect::to(&topic_url(&message.project, &message.topic)).into_response()
        }
        Err(StoreError::NoSuchTopic) => not_found("no such topic"),
        Err(StoreError::Invalid(reason)) => refusal(
            &state,
            StatusCode::UNPROCESSABLE_ENTITY,
            &reason.to_string(),
            &topic_url(&project, &topic),
        ),
        Err(e) => server_error(e),
    }
}

async fn create_topic(
    State(state): State<Shared>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Form(form): Form<TopicForm>,
) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "foreign origin").into_response();
    }
    let back = topic_url(&project, GENERAL);
    match state
        .store
        .create_topic(&project, &form.name, form.about.as_deref(), OWNER, now())
    {
        Ok(topic) => {
            state.notify("nav".into());
            Redirect::to(&topic_url(&project, &topic.name)).into_response()
        }
        Err(StoreError::NoSuchProject) => not_found("no such project"),
        Err(e @ (StoreError::Invalid(_) | StoreError::TopicExists | StoreError::TopicLimit(_))) => {
            let status = match e {
                StoreError::TopicExists => StatusCode::CONFLICT,
                StoreError::TopicLimit(_) => StatusCode::TOO_MANY_REQUESTS,
                _ => StatusCode::UNPROCESSABLE_ENTITY,
            };
            refusal(&state, status, &e.to_string(), &back)
        }
        Err(e) => server_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::testutil::*;
    use axum::http::header::LOCATION;

    async fn register(app: &Router, token: &str, body: &str) -> String {
        let (status, _, out) =
            call(app, api("POST", "/v1/sessions", Some(token), Some(body))).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        let start = out.find(r#""handle":""#).unwrap() + 10;
        out[start..start + out[start..].find('"').unwrap()].to_string()
    }

    #[tokio::test]
    async fn the_board_shows_the_answer_numbers_of_the_day() {
        let (app, _) = app();
        let browser = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"demo"}"#).await;
        let (_, _, page) = call(&app, get_req("/")).await;
        assert!(
            page.contains("Requests in the last 24 hours: none"),
            "{page}"
        );
        let ask = format!(
            r#"{{"handle":"{browser}","kind":"ask","to":"@audio","what":"check the sound","why":"w","expect":"e","by":"2h"}}"#
        );
        let (status, _, out) =
            call(&app, api("POST", "/v1/messages", Some(TOKEN), Some(&ask))).await;
        assert_eq!(status, StatusCode::CREATED, "{out}");
        let (_, _, fragment) = call(&app, get_req("/board")).await;
        assert!(
            fragment.contains(
                "Requests in the last 24 hours: 1 · answered 0% (0 of 1) · no answers yet"
            ),
            "{fragment}"
        );
    }

    #[tokio::test]
    async fn the_board_is_the_front_page_inside_the_shell() {
        let (app, _) = app();
        let (status, _, page) = call(&app, get_req("/")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Who works on what") && page.contains(r#"id="side""#));
        assert!(
            page.contains(r#"href="/p/all/general""#) && page.contains(r#"href="/p/demo/general""#)
        );
        assert!(page.contains("Nobody is working"));
    }

    #[tokio::test]
    async fn every_project_has_a_general_topic_page_with_a_composer() {
        let (app, _) = app();
        for project in ["all", "demo"] {
            let (status, _, page) = call(&app, get_req(&format!("/p/{project}/general"))).await;
            assert_eq!(status, StatusCode::OK, "{project}");
            assert!(page.contains("<h1># general</h1>") && page.contains("Nothing here yet"));
            assert!(page.contains(&format!(r#"action="/p/{project}/general/post""#)));
        }
    }

    #[tokio::test]
    async fn a_project_address_leads_to_its_general_topic() {
        let (app, _) = app();
        let (status, headers, _) = call(&app, get_req("/p/demo")).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[LOCATION], "/p/demo/general");
        let (status, headers, _) = call(&app, get_req("/r/demo")).await;
        assert_eq!(
            status,
            StatusCode::PERMANENT_REDIRECT,
            "the old address of a room"
        );
        assert_eq!(headers[LOCATION], "/p/demo/general");
        assert_eq!(
            call(&app, get_req("/p/nope")).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn unknown_topics_are_not_found() {
        let (app, _) = app();
        for uri in ["/p/demo/nope", "/p/nope/general", "/p/demo/nope/messages"] {
            assert_eq!(
                call(&app, get_req(uri)).await.0,
                StatusCode::NOT_FOUND,
                "{uri}"
            );
        }
        let (status, ..) = call(&app, form_req("/p/demo/nope/post", "text=x", None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_owner_posts_into_a_topic_and_sees_it_in_the_feed() {
        let (app, state) = app();
        let mut events = state.events.subscribe();
        let body = "text=ready+https%3A%2F%2Fexample.test%2Fx";
        let (status, headers, _) = call(
            &app,
            form_req("/p/all/general/post", body, Some("https://chappe-bus.test")),
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[LOCATION], "/p/all/general");
        assert_eq!(events.try_recv().unwrap(), "msg:all/general");

        let (_, _, page) = call(&app, get_req("/p/all/general")).await;
        assert!(page.contains("owner") && page.contains("ready"));
        assert!(page.contains(r#"<a href="https://example.test/x""#));
        let (_, _, feed) = call(&app, get_req("/p/all/general/messages")).await;
        assert!(feed.contains("ready") && !feed.contains("<form"));
        let (_, _, other) = call(&app, get_req("/p/demo/general")).await;
        assert!(other.contains("Nothing here yet"), "projects do not mix");
        let (_, _, sidebar) = call(&app, get_req("/nav?project=all&topic=general")).await;
        assert!(
            sidebar.contains(r#"<span class="count">1</span>"#)
                && sidebar.contains("topic current")
        );
    }

    #[tokio::test]
    async fn a_message_number_leads_to_its_topic_at_that_message() {
        let (app, state) = app();
        call(&app, form_req("/p/all/general/post", "text=hello", None)).await;
        let id = state.store.recent_messages("all", "general", 1).unwrap()[0].id;
        let (status, headers, _) = call(&app, get_req(&format!("/m/{id}"))).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[LOCATION], format!("/p/all/general#m{id}").as_str());
        assert_eq!(
            call(&app, get_req("/m/99999")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(&app, get_req("/m/abc")).await.0,
            StatusCode::BAD_REQUEST
        );
        // The page itself carries the anchor and the link.
        let (_, _, page) = call(&app, get_req("/p/all/general")).await;
        assert!(page.contains(&format!(r#"id="m{id}""#)) && page.contains(&format!("#m{id}\"")));
    }

    #[tokio::test]
    async fn posted_markup_is_escaped_on_the_page() {
        let (app, _) = app();
        let body = "text=%3Cscript%3Ealert%281%29%3C%2Fscript%3E";
        call(&app, form_req("/p/all/general/post", body, None)).await;
        let (_, _, page) = call(&app, get_req("/p/all/general")).await;
        assert!(!page.contains("<script>alert"));
        assert!(page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    }

    #[tokio::test]
    async fn invalid_posts_are_refused_inside_the_page_and_not_stored() {
        let (app, state) = app();
        let (status, _, body) = call(&app, form_req("/p/all/general/post", "text=+", None)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("text cannot be empty") && body.contains(r#"id="side""#));
        let long = format!("text={}", "x".repeat(601));
        assert_eq!(
            call(&app, form_req("/p/all/general/post", &long, None))
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert!(
            state
                .store
                .recent_messages("all", "general", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn oversized_body_is_refused() {
        let (app, _) = app();
        let big = format!("text={}", "x".repeat(16 * 1024));
        assert_eq!(
            call(&app, form_req("/p/all/general/post", &big, None))
                .await
                .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn a_post_from_another_site_is_refused() {
        let (app, state) = app();
        let evil = Some("https://evil.test");
        let (status, ..) = call(&app, form_req("/p/all/general/post", "text=x", evil)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, ..) = call(&app, form_req("/p/all/topics", "name=x", evil)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            state
                .store
                .recent_messages("all", "general", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.store.nav().unwrap()[0].topics.len(), 2);
    }

    #[test]
    fn origin_check() {
        let headers = |origin: Option<&str>, host: Option<&str>| {
            let mut h = HeaderMap::new();
            if let Some(o) = origin {
                h.insert(header::ORIGIN, o.parse().unwrap());
            }
            if let Some(v) = host {
                h.insert(header::HOST, v.parse().unwrap());
            }
            h
        };
        assert!(same_origin(&headers(None, Some("a.test"))));
        assert!(same_origin(&headers(
            Some("https://a.test"),
            Some("a.test")
        )));
        assert!(same_origin(&headers(
            Some("http://localhost:8080"),
            Some("localhost:8080")
        )));
        assert!(!same_origin(&headers(
            Some("https://a.test.evil.test"),
            Some("a.test")
        )));
        assert!(!same_origin(&headers(Some("null"), Some("a.test"))));
        assert!(!same_origin(&headers(Some("https://a.test"), None)));
    }

    #[tokio::test]
    async fn the_owner_creates_a_topic_from_the_sidebar() {
        let (app, state) = app();
        let mut events = state.events.subscribe();
        let (status, headers, _) =
            call(&app, form_req("/p/demo/topics", "name=Audio+driver", None)).await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(headers[LOCATION], "/p/demo/audio-driver");
        assert_eq!(events.try_recv().unwrap(), "nav");
        let (status, _, page) = call(&app, get_req("/p/demo/audio-driver")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("<h1># audio-driver</h1>"));
        assert!(page.contains(r#"href="/p/demo/audio-driver" class="topic current""#));
        // general is still first in the sidebar.
        assert!(page.find("/p/demo/general").unwrap() < page.find("/p/demo/audio-driver").unwrap());
    }

    #[tokio::test]
    async fn topics_with_non_latin_names_have_working_addresses() {
        let (app, _) = app();
        let name = "name=Z%C3%BCrich+in+browser";
        let (_, headers, _) = call(&app, form_req("/p/demo/topics", name, None)).await;
        let location = headers[LOCATION].to_str().unwrap().to_string();
        assert!(location.starts_with("/p/demo/z%C3%BC"), "{location}");
        let (status, _, page) = call(&app, get_req(&location)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("# zürich-in-browser"));
    }

    #[tokio::test]
    async fn bad_or_repeated_topics_are_refused_inside_the_page() {
        let (app, state) = app();
        let (status, _, body) = call(&app, form_req("/p/demo/topics", "name=a%2Fb", None)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body.contains("topic name"));
        let (status, ..) = call(&app, form_req("/p/demo/topics", "name=general", None)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, ..) = call(&app, form_req("/p/nope/topics", "name=x", None)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            state.store.nav().unwrap()[1].topics.len(),
            2,
            "nothing was created"
        );
    }

    #[tokio::test]
    async fn the_people_panel_lists_the_sessions_of_the_project() {
        let (app, _) = app();
        let a = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        let b = register(&app, TOKEN_MAC, r#"{"roles":["audio"],"project":"core"}"#).await;
        let (_, _, studio) = call(&app, get_req("/p/demo/people")).await;
        assert!(studio.contains(&a) && !studio.contains(&b));
        let (_, _, all) = call(&app, get_req("/p/all/people")).await;
        assert!(
            all.contains(&a) && all.contains(&b),
            "all is the project of everyone"
        );
        let (_, _, page) = call(&app, get_req("/p/demo/general")).await;
        assert!(page.contains(&a) && !page.contains(&b));
    }

    #[tokio::test]
    async fn a_session_in_a_new_project_creates_it_with_general() {
        let (app, _) = app();
        register(&app, TOKEN, r#"{"roles":["ci"],"project":"core"}"#).await;
        let (status, _, page) = call(&app, get_req("/p/core/general")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(page.contains(r#"href="/p/core/general""#));
        let (_, _, sidebar) = call(&app, get_req("/nav")).await;
        assert_eq!(sidebar.matches(r#"<details class="proj""#).count(), 3);
    }

    #[tokio::test]
    async fn two_sessions_from_two_machines_appear_on_the_board() {
        let (app, state) = app();
        let a = register(
            &app,
            TOKEN,
            r##"{"roles":["browser"],"project":"demo","issue":"#471 svcd"}"##,
        )
        .await;
        let b = register(
            &app,
            TOKEN_MAC,
            r#"{"roles":["audio","desktop"],"project":"demo"}"#,
        )
        .await;
        let (_, _, page) = call(&app, get_req("/")).await;
        assert!(page.contains(&a) && page.contains(&b));
        assert!(
            page.contains("claude@host-a") && page.contains("codex@mac"),
            "agent comes from the token"
        );
        assert_eq!(page.matches("<table>").count(), 1);
        let (_, _, fragment) = call(&app, get_req("/board")).await;
        assert!(fragment.contains(&a) && !fragment.contains("<h1>"));
        assert!(
            state
                .store
                .recent_messages("all", "general", 10)
                .unwrap()
                .is_empty(),
            "the board adds no messages"
        );
        assert!(
            state
                .store
                .recent_messages("demo", "general", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn leaving_removes_the_session_from_the_board() {
        let (app, _) = app();
        let h = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        call(
            &app,
            api("DELETE", &format!("/v1/sessions/{h}"), Some(TOKEN), None),
        )
        .await;
        let (_, _, fragment) = call(&app, get_req("/board")).await;
        assert!(!fragment.contains(&h) && fragment.contains("Nobody is working"));
    }

    #[tokio::test]
    async fn the_page_hears_about_registry_changes() {
        let (app, state) = app();
        let mut events = state.events.subscribe();
        let h = register(&app, TOKEN, r#"{"roles":["browser"],"project":"demo"}"#).await;
        assert_eq!(events.try_recv().unwrap(), "board");
        assert_eq!(events.try_recv().unwrap(), "nav", "the project may be new");
        call(
            &app,
            api("POST", &format!("/v1/sessions/{h}/beat"), Some(TOKEN), None),
        )
        .await;
        assert!(
            events.try_recv().is_err(),
            "a beat of an active session changes nothing on the board"
        );
        call(
            &app,
            api("DELETE", &format!("/v1/sessions/{h}"), Some(TOKEN), None),
        )
        .await;
        assert_eq!(events.try_recv().unwrap(), "board");
    }
}
