//! The HTTP client for the bus and the shapes of what it sends and receives.

use super::*;

/// An id that is safe as a file name: letters, digits, `-`, `_`, `.`, no dots first.
pub(crate) fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 100
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// What came back from the service.
pub(crate) enum Reply {
    Ok(String),
    /// The service answered with an error the agent can act on.
    Refused {
        status: u16,
        reason: String,
    },
    /// No answer, or the service itself is failing.
    Down(String),
}

pub(crate) struct Client {
    pub(super) base: String,
    pub(super) token: String,
    pub(super) agent: ureq::Agent,
}

#[derive(Serialize)]
pub(crate) struct RegisterBody<'a> {
    pub(super) roles: &'a [String],
    pub(super) project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) issue: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) branch: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) worktree: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) transcript: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<&'a str>,
}

#[derive(Deserialize)]
pub(crate) struct Handle {
    pub(crate) handle: String,
    /// Who else works in the project and which resources are held.
    #[serde(default)]
    pub(crate) briefing: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct Row {
    pub(super) handle: String,
    #[serde(default)]
    pub(super) name: Option<String>,
    pub(super) roles: Vec<String>,
    pub(super) agent: String,
    pub(super) project: String,
    pub(super) issue: Option<String>,
    pub(super) state: String,
    pub(super) last_seen_secs_ago: i64,
}

#[derive(Deserialize)]
pub(super) struct TopicRow {
    pub(super) project: String,
    pub(super) name: String,
    pub(super) about: Option<String>,
    pub(super) messages: i64,
}

#[derive(Serialize)]
pub(super) struct TopicBody<'a> {
    pub(super) handle: &'a str,
    pub(super) name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) about: Option<&'a str>,
}

#[derive(Serialize)]
pub(super) struct MessageBody<'a> {
    pub(super) handle: &'a str,
    pub(super) kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) topic: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) what: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) why: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) expect: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) by: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) link: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) ask: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) result: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) wait: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) dry_run: bool,
}

#[derive(Deserialize)]
pub(super) struct ReceiverRow {
    pub(super) handle: String,
    pub(super) agent: String,
    pub(super) roles: Vec<String>,
    pub(super) issue: Option<String>,
    pub(super) active: bool,
}

#[derive(Deserialize)]
pub(super) struct SentRow {
    pub(super) id: Option<i64>,
    pub(super) project: String,
    pub(super) topic: String,
    pub(super) text: String,
    pub(super) receivers: Vec<ReceiverRow>,
    pub(super) wake: Option<String>,
    pub(super) dry_run: bool,
    #[serde(default)]
    pub(super) similar: Vec<SimilarRow>,
}

#[derive(Deserialize)]
pub(super) struct SimilarRow {
    pub(super) id: i64,
    pub(super) question: String,
    pub(super) answer: String,
}

#[derive(Serialize)]
pub(crate) struct InboxBody<'a> {
    pub(super) handle: &'a str,
    /// The tool's current name for the session, so that a rename reaches the page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<&'a str>,
    /// Where the session is now, for the board to find its issue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) worktree: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) branch: Option<&'a str>,
}

#[derive(Deserialize)]
pub(super) struct InboxRow {
    pub(super) rendered: String,
}

#[derive(Deserialize)]
pub(crate) struct InboxAnswer {
    #[serde(default)]
    pub(super) notes: Vec<String>,
    pub(super) messages: Vec<InboxRow>,
    pub(super) waiting: usize,
    pub(super) next_in_secs: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct ErrorBody {
    pub(super) error: String,
}

impl Client {
    pub(crate) fn new(base: &str, token: String) -> Client {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Client {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent,
        }
    }

    fn finish(result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Reply {
        let mut response = match result {
            Ok(r) => r,
            Err(e) => return Reply::Down(e.to_string()),
        };
        let status = response.status().as_u16();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        match status {
            200..=299 => Reply::Ok(body),
            500..=599 => Reply::Down(format!("the service answered {status}")),
            _ => {
                let reason = serde_json::from_str::<ErrorBody>(&body)
                    .map(|e| e.error)
                    .unwrap_or_else(|_| {
                        if status == 401 {
                            "the token was rejected".into()
                        } else {
                            format!("status {status}")
                        }
                    });
                Reply::Refused { status, reason }
            }
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub(crate) fn get(&self, path: &str) -> Reply {
        Client::finish(
            self.agent
                .get(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .call(),
        )
    }

    pub(crate) fn delete(&self, path: &str) -> Reply {
        Client::finish(
            self.agent
                .delete(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .call(),
        )
    }

    pub(crate) fn post_empty(&self, path: &str) -> Reply {
        Client::finish(
            self.agent
                .post(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .send_empty(),
        )
    }

    pub(crate) fn post_json(&self, path: &str, body: &impl Serialize) -> Reply {
        Client::finish(
            self.agent
                .post(self.url(path))
                .header("Authorization", format!("Bearer {}", self.token))
                .send_json(body),
        )
    }
}
