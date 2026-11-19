//! `chappe-host`: the daemon of a machine that wakes idle sessions.
//!
//! It asks the service, over HTTPS with a token named `<machine>.host`, for the
//! sessions of its machine that have to be woken, does the waking with the
//! tool's own command (one adapter per tool), and reports how it went. The
//! service never reaches a machine; the daemon only ever runs a fixed command
//! with the message on standard input.

pub mod claude;
pub mod codex;
pub mod runner;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::json;

/// How long one request to the service may take; a poll waits up to 25 s of it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const POLL_WAIT_SECS: u64 = 25;
/// How often the processes of the sessions are reported to the service.
const RUNNING_EVERY: Duration = Duration::from_secs(60);

/// A session to wake.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    pub handle: String,
    pub tool: String,
    pub session_id: Option<String>,
    pub worktree: Option<String>,
    pub transcript: Option<String>,
    /// The waiting messages, each under its fixed header.
    pub text: String,
    pub message_ids: Vec<i64>,
}

/// How a wake went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The session was woken and has the messages; the text is for the log.
    Delivered(String),
    /// Not now: busy, waiting for the owner, or possibly open elsewhere.
    Deferred(String),
    /// This machine cannot wake sessions of this tool.
    Unsupported(String),
    Failed(String),
}

impl Outcome {
    fn name(&self) -> &'static str {
        match self {
            Outcome::Delivered(_) => "delivered",
            Outcome::Deferred(_) => "deferred",
            Outcome::Unsupported(_) => "unsupported",
            Outcome::Failed(_) => "failed",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Outcome::Delivered(d)
            | Outcome::Deferred(d)
            | Outcome::Unsupported(d)
            | Outcome::Failed(d) => d,
        }
    }
}

/// Wakes the sessions of one tool.
pub trait Adapter {
    fn tool(&self) -> &str;
    fn wake(&self, job: &Job) -> Outcome;
    /// The ids of the sessions of this tool that a live process holds on this
    /// machine, for the service to tell which sessions are gone. `None` when that
    /// cannot be told (an unreadable directory): the service then assumes nothing.
    fn running(&self) -> Option<Vec<String>> {
        None
    }
}

/// Settings, from environment variables.
#[derive(Debug)]
pub struct Config {
    pub url: String,
    pub token: String,
    pub claude_program: String,
    pub claude_model: Option<String>,
    pub claude_sessions: PathBuf,
    pub codex_program: String,
    pub codex_model: Option<String>,
    /// `~/.codex/thread-writer-locks`: which threads a process writes.
    pub codex_locks: PathBuf,
    pub wake_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Result<Config, String> {
        Config::from_lookup(|k| std::env::var(k).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Config, String> {
        let get = |k: &str| get(k).filter(|v| !v.is_empty());
        let home = get("HOME");
        let token = match (get("CHAPPE_HOST_TOKEN"), get("CHAPPE_HOST_TOKEN_FILE")) {
            (Some(t), _) => t,
            (None, file) => {
                let path = file
                    .or_else(|| {
                        get("XDG_CONFIG_HOME")
                            .map(|d| format!("{d}/chappe/host-token"))
                            .or_else(|| {
                                home.as_ref()
                                    .map(|h| format!("{h}/.config/chappe/host-token"))
                            })
                    })
                    .ok_or("no token: set CHAPPE_HOST_TOKEN_FILE")?;
                std::fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read the host token {path}: {e}"))?
            }
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            return Err("the host token is empty".into());
        }
        let secs = match get("CHAPPE_HOST_WAKE_TIMEOUT_SECS") {
            Some(s) => s
                .parse()
                .map_err(|_| format!("CHAPPE_HOST_WAKE_TIMEOUT_SECS {s:?}: a number of seconds"))?,
            None => 900,
        };
        Ok(Config {
            url: get("CHAPPE_URL").ok_or("no service address: set CHAPPE_URL")?,
            token,
            claude_program: get("CHAPPE_HOST_CLAUDE").unwrap_or_else(|| "claude".into()),
            claude_model: get("CHAPPE_HOST_CLAUDE_MODEL"),
            claude_sessions: get("CHAPPE_HOST_CLAUDE_SESSIONS")
                .map(PathBuf::from)
                .or_else(|| {
                    home.clone()
                        .map(|h| PathBuf::from(h).join(".claude").join("sessions"))
                })
                .ok_or("no HOME: set CHAPPE_HOST_CLAUDE_SESSIONS")?,
            codex_program: get("CHAPPE_HOST_CODEX").unwrap_or_else(|| "codex".into()),
            codex_model: get("CHAPPE_HOST_CODEX_MODEL"),
            codex_locks: get("CHAPPE_HOST_CODEX_LOCKS")
                .map(PathBuf::from)
                .or_else(|| {
                    get("CODEX_HOME")
                        .or_else(|| home.map(|h| format!("{h}/.codex")))
                        .map(|d| PathBuf::from(d).join("thread-writer-locks"))
                })
                .ok_or("no HOME: set CHAPPE_HOST_CODEX_LOCKS")?,
            wake_timeout: Duration::from_secs(secs),
        })
    }
}

pub struct Host {
    base: String,
    token: String,
    agent: ureq::Agent,
    adapters: HashMap<String, Box<dyn Adapter>>,
    /// When the processes of the sessions were last reported to the service.
    reported_at: std::sync::Mutex<Option<Instant>>,
}

impl Host {
    pub fn new(base: &str, token: String, adapters: Vec<Box<dyn Adapter>>) -> Host {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();
        Host {
            base: base.trim_end_matches('/').to_string(),
            token,
            agent,
            adapters: adapters
                .into_iter()
                .map(|a| (a.tool().to_string(), a))
                .collect(),
            reported_at: std::sync::Mutex::new(None),
        }
    }

    /// Which sessions have a live process, per tool, if it is time to say (once a
    /// minute: the service needs it to tell a vanished session after two hours,
    /// not at every poll). A tool whose answer is unknown is left out.
    fn running(&self) -> Option<serde_json::Value> {
        let mut at = self.reported_at.lock().unwrap_or_else(|e| e.into_inner());
        if at.is_some_and(|t| t.elapsed() < RUNNING_EVERY) {
            return None;
        }
        let mut running = serde_json::Map::new();
        for (tool, adapter) in &self.adapters {
            if let Some(ids) = adapter.running() {
                running.insert(tool.clone(), json!(ids));
            }
        }
        *at = Some(Instant::now());
        (!running.is_empty()).then_some(serde_json::Value::Object(running))
    }

    fn post(&self, path: &str, body: serde_json::Value) -> Result<String, String> {
        let mut response = self
            .agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
            .send_json(body)
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().unwrap_or_default();
        match status {
            200..=299 => Ok(text),
            401 | 403 => Err(format!(
                "the service refused the host token ({status}): {text}"
            )),
            _ => Err(format!("the service answered {status}: {text}")),
        }
    }

    /// The wakes to carry out now; with `wait_secs` the service waits for one.
    pub fn claim(&self, wait_secs: u64) -> Result<Vec<Job>, String> {
        let mut body = json!({ "wait_secs": wait_secs });
        if let Some(running) = self.running() {
            body["running"] = running;
        }
        let text = self.post("/v1/host/claim", body)?;
        serde_json::from_str(&text).map_err(|e| format!("an unexpected answer to the claim: {e}"))
    }

    pub fn report(&self, job: &Job, outcome: &Outcome) -> Result<(), String> {
        let body = match outcome {
            Outcome::Delivered(_) => {
                json!({"outcome": "delivered", "message_ids": job.message_ids})
            }
            Outcome::Deferred(r) => json!({"outcome": "deferred", "reason": r}),
            Outcome::Unsupported(r) => json!({"outcome": "unsupported", "reason": r}),
            Outcome::Failed(r) => json!({"outcome": "failed", "reason": r}),
        };
        self.post(&format!("/v1/host/wakes/{}", job.id), body)
            .map(|_| ())
    }

    /// Wakes one session with the adapter of its tool.
    pub fn wake(&self, job: &Job) -> Outcome {
        match self.adapters.get(&job.tool) {
            Some(adapter) => adapter.wake(job),
            None => Outcome::Unsupported(format!("no adapter for {} on this machine", job.tool)),
        }
    }

    /// One round: claim, wake, report. Returns how many wakes were handled.
    pub fn work_once(&self, wait_secs: u64) -> Result<usize, String> {
        let jobs = self.claim(wait_secs)?;
        for job in &jobs {
            let outcome = self.wake(job);
            // Never the text of a message: who, how, and the reason.
            eprintln!(
                "chappe-host: wake #{} of {} ({}): {} {}",
                job.id,
                job.handle,
                job.tool,
                outcome.name(),
                outcome.detail()
            );
            if let Err(e) = self.report(job, &outcome) {
                eprintln!("chappe-host: could not report wake #{}: {e}", job.id);
            }
        }
        Ok(jobs.len())
    }

    /// Works until `stop` is set; a service that is down is waited for.
    pub fn run(&self, stop: &AtomicBool) {
        let mut pause = 1;
        while !stop.load(Ordering::Relaxed) {
            match self.work_once(POLL_WAIT_SECS) {
                Ok(_) => pause = 1,
                Err(e) => {
                    eprintln!("chappe-host: {e}; trying again in {pause} s");
                    std::thread::sleep(Duration::from_secs(pause));
                    pause = (pause * 2).min(60);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &'static [(&str, &str)]) -> Result<Config, String> {
        Config::from_lookup(|k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn settings_have_defaults_and_the_token_comes_from_its_file() {
        let dir = std::env::temp_dir().join(format!("host-config-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("chappe")).unwrap();
        std::fs::write(dir.join("chappe/host-token"), "tok-from-file\n").unwrap();
        let d: &'static str = Box::leak(dir.display().to_string().into_boxed_str());
        let c = config(Box::leak(Box::new([
            ("XDG_CONFIG_HOME", d),
            ("HOME", "/home/u"),
            ("CHAPPE_URL", "http://bus.invalid"),
        ])))
        .unwrap();
        assert_eq!(c.token, "tok-from-file");
        assert_eq!(c.url, "http://bus.invalid");
        assert_eq!(c.claude_program, "claude");
        assert_eq!(c.claude_sessions, PathBuf::from("/home/u/.claude/sessions"));
        assert_eq!(c.codex_program, "codex");
        assert_eq!(
            c.codex_locks,
            PathBuf::from("/home/u/.codex/thread-writer-locks")
        );
        assert_eq!(c.wake_timeout, Duration::from_secs(900));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_can_be_overridden_and_bad_ones_are_refused() {
        let c = config(&[
            ("CHAPPE_HOST_TOKEN", " t "),
            ("CHAPPE_URL", "http://x:1"),
            ("CHAPPE_HOST_CLAUDE", "/opt/claude"),
            ("CHAPPE_HOST_CLAUDE_MODEL", "haiku"),
            ("CHAPPE_HOST_CLAUDE_SESSIONS", "/s"),
            ("CHAPPE_HOST_CODEX", "/opt/codex"),
            ("CHAPPE_HOST_CODEX_MODEL", "small"),
            ("CHAPPE_HOST_CODEX_LOCKS", "/l"),
            ("CHAPPE_HOST_WAKE_TIMEOUT_SECS", "60"),
        ])
        .unwrap();
        assert_eq!(
            (c.token.as_str(), c.url.as_str(), c.claude_model.as_deref()),
            ("t", "http://x:1", Some("haiku"))
        );
        assert_eq!(
            (c.claude_program.as_str(), c.wake_timeout),
            ("/opt/claude", Duration::from_secs(60))
        );
        assert_eq!(
            (c.codex_program.as_str(), c.codex_model.as_deref()),
            ("/opt/codex", Some("small"))
        );
        assert_eq!(c.codex_locks, PathBuf::from("/l"));
        assert!(
            config(&[("HOME", "/nonexistent")])
                .unwrap_err()
                .contains("cannot read the host token")
        );
        assert!(
            config(&[
                ("CHAPPE_HOST_TOKEN", "t"),
                ("HOME", "/h"),
                ("CHAPPE_HOST_WAKE_TIMEOUT_SECS", "soon")
            ])
            .is_err()
        );
    }

    struct Lister(&'static str, Option<Vec<String>>);

    impl Adapter for Lister {
        fn tool(&self) -> &str {
            self.0
        }
        fn wake(&self, _: &Job) -> Outcome {
            Outcome::Failed("not here".into())
        }
        fn running(&self) -> Option<Vec<String>> {
            self.1.clone()
        }
    }

    #[test]
    fn the_running_sessions_are_reported_once_a_minute_and_only_for_tools_that_know() {
        let host = Host::new(
            "http://127.0.0.1:1",
            "t".into(),
            vec![
                Box::new(Lister("claude", Some(vec!["s1".into()]))),
                Box::new(Lister("codex", None)),
                Box::new(Lister("opencode", Some(vec![]))),
            ],
        );
        let first = host.running().expect("the first time it is time to say");
        assert_eq!(first, json!({"claude": ["s1"], "opencode": []}));
        assert_eq!(host.running(), None, "not again within a minute");
        // A host whose tools cannot tell says nothing at all.
        let silent = Host::new(
            "http://127.0.0.1:1",
            "t".into(),
            vec![Box::new(Lister("codex", None))],
        );
        assert_eq!(silent.running(), None);
    }

    #[test]
    fn a_tool_without_an_adapter_is_unsupported() {
        let host = Host::new("http://127.0.0.1:1", "t".into(), vec![]);
        let job = Job {
            id: 1,
            handle: "a-1".into(),
            tool: "antigravity".into(),
            session_id: None,
            worktree: None,
            transcript: None,
            text: "x".into(),
            message_ids: vec![1],
        };
        assert_eq!(
            host.wake(&job),
            Outcome::Unsupported("no adapter for antigravity on this machine".into())
        );
    }
}
