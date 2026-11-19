//! The adapter of Claude Code.
//!
//! State and "who holds the session" come from `~/.claude/sessions/<pid>.json`,
//! which every running interactive Claude Code keeps: its `sessionId`, `pid`,
//! `procStart` and `status` (`idle`, `busy`, `waiting`). Records of dead
//! processes stay, so a record counts only while its pid is alive and started
//! when `procStart` says (a reused pid does not match). The wake is
//! `claude -p --resume <id>` with the messages on standard input.
//!
//! Rules: a session that is working or waits for the owner is not touched, and
//! a session that a live process holds is never resumed in a second process
//! (Claude Code has no lock for that: both would write one transcript).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::runner::{Ran, Runner, Spec, SystemRunner};
use crate::{Adapter, Job, Outcome};

/// A live process that holds a session.
#[derive(Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u64,
    pub status: String,
}

/// Whether the process `pid` is the one that started at `proc_start`.
#[cfg(target_os = "linux")]
fn alive(pid: u64, proc_start: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The fields after `(comm)`; `starttime` is the twentieth of them.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .is_some_and(|start| start == proc_start)
}

/// Elsewhere there is no `/proc`: a live pid counts, a reused one errs on the safe side.
#[cfg(not(target_os = "linux"))]
fn alive(pid: u64, _proc_start: &str) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty())
}

/// The sessions that a live process holds, from the records in `dir`; `None` when
/// the directory cannot be read (then nothing is known, which is not "no process").
fn live_sessions(dir: &Path) -> Option<Vec<(String, Holder)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let (Some(session), Some(pid), Some(start)) = (
            record["sessionId"].as_str(),
            record["pid"].as_u64(),
            record["procStart"].as_str(),
        ) else {
            continue;
        };
        if alive(pid, start) {
            let status = record["status"].as_str().unwrap_or("unknown").to_string();
            out.push((session.to_string(), Holder { pid, status }));
        }
    }
    Some(out)
}

/// The live process that holds the session, if any.
pub fn holder_of(dir: &Path, session_id: &str) -> Option<Holder> {
    live_sessions(dir)?
        .into_iter()
        .find_map(|(id, holder)| (id == session_id).then_some(holder))
}

/// The ids of all sessions a live process holds; `None` when that cannot be told.
pub fn running_sessions(dir: &Path) -> Option<Vec<String>> {
    Some(live_sessions(dir)?.into_iter().map(|(id, _)| id).collect())
}

pub struct ClaudeAdapter {
    program: String,
    model: Option<String>,
    sessions_dir: PathBuf,
    timeout: Duration,
    runner: Box<dyn Runner>,
}

impl ClaudeAdapter {
    pub fn new(
        program: String,
        model: Option<String>,
        sessions_dir: PathBuf,
        timeout: Duration,
    ) -> ClaudeAdapter {
        ClaudeAdapter::with_runner(
            program,
            model,
            sessions_dir,
            timeout,
            Box::new(SystemRunner),
        )
    }

    pub fn with_runner(
        program: String,
        model: Option<String>,
        sessions_dir: PathBuf,
        timeout: Duration,
        runner: Box<dyn Runner>,
    ) -> ClaudeAdapter {
        ClaudeAdapter {
            program,
            model,
            sessions_dir,
            timeout,
            runner,
        }
    }

    /// The command that resumes the session; the text goes on standard input.
    fn spec(&self, job: &Job, session_id: &str, worktree: PathBuf) -> Spec {
        let mut args: Vec<String> = ["-p", "--resume", session_id, "--output-format", "json"]
            .map(String::from)
            .to_vec();
        // The woken session answers with `chappe`; everything else follows its own settings.
        args.extend(["--allowedTools".into(), "Bash(chappe:*)".into()]);
        if let Some(model) = &self.model {
            args.extend(["--model".into(), model.clone()]);
        }
        Spec {
            program: self.program.clone(),
            args,
            cwd: Some(worktree),
            env: vec![("CHAPPE_HANDLE".into(), job.handle.clone())],
            stdin: job.text.clone(),
            timeout: self.timeout,
        }
    }
}

/// The first line of some output, short enough for a log.
fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

/// What the result of `claude -p --output-format json` says: the cost, or the error.
fn read_result(stdout: &str) -> Result<String, String> {
    let v: Value =
        serde_json::from_str(stdout).map_err(|_| "claude printed no result".to_string())?;
    if v["is_error"].as_bool() == Some(true) {
        let why = v["result"].as_str().unwrap_or("an error");
        return Err(format!(
            "the woken session ended with an error: {}",
            first_line(why)
        ));
    }
    let u = &v["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Ok(format!(
        "input {} (cache read {}, cache write {}), output {}, cost ${:.4}",
        n("input_tokens") + n("cache_read_input_tokens") + n("cache_creation_input_tokens"),
        n("cache_read_input_tokens"),
        n("cache_creation_input_tokens"),
        n("output_tokens"),
        v["total_cost_usd"].as_f64().unwrap_or(0.0)
    ))
}

impl Adapter for ClaudeAdapter {
    fn tool(&self) -> &str {
        "claude"
    }

    fn running(&self) -> Option<Vec<String>> {
        running_sessions(&self.sessions_dir)
    }

    fn wake(&self, job: &Job) -> Outcome {
        let Some(session_id) = job.session_id.as_deref() else {
            return Outcome::Unsupported("the session registered without a session id".into());
        };
        if !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Outcome::Failed("an odd session id".into());
        }
        let Some(worktree) = job
            .worktree
            .as_deref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
        else {
            return Outcome::Failed("the worktree of the session is gone".into());
        };
        if let Some(holder) = holder_of(&self.sessions_dir, session_id) {
            return Outcome::Deferred(match holder.status.as_str() {
                "busy" => "the session is working".into(),
                "waiting" => "the session waits for the owner".into(),
                _ => format!(
                    "the session is open in a terminal (pid {}): not resumed in a second process",
                    holder.pid
                ),
            });
        }
        let Ran {
            code,
            stdout,
            stderr,
            timed_out,
            spawn_error,
        } = self.runner.run(&self.spec(job, session_id, worktree));
        if let Some(e) = spawn_error {
            return Outcome::Failed(format!("cannot start {}: {e}", self.program));
        }
        if timed_out {
            return Outcome::Failed("the woken session did not finish in time".into());
        }
        match code {
            Some(0) => match read_result(&stdout) {
                Ok(cost) => Outcome::Delivered(cost),
                Err(e) => Outcome::Failed(e),
            },
            Some(c) => Outcome::Failed(format!(
                "{} exited with {c}: {}",
                self.program,
                first_line(&stderr)
            )),
            None => Outcome::Failed(format!("{} was killed", self.program)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records the commands and answers with a canned result.
    struct Fake {
        seen: Arc<Mutex<Vec<Spec>>>,
        answer: Ran,
    }

    impl Runner for Fake {
        fn run(&self, spec: &Spec) -> Ran {
            self.seen.lock().unwrap().push(spec.clone());
            self.answer.clone()
        }
    }

    const OK: &str = r#"{"type":"result","is_error":false,"result":"ok","total_cost_usd":0.0052,"usage":{"input_tokens":2,"cache_creation_input_tokens":758,"cache_read_input_tokens":29559,"output_tokens":223}}"#;

    fn job(text: &str) -> Job {
        Job {
            id: 7,
            handle: "audio-1a2b".into(),
            tool: "claude".into(),
            session_id: Some("53d1f51c-c5b3-4c49-8710-ac0b209dd6b0".into()),
            worktree: Some(std::env::temp_dir().display().to_string()),
            transcript: None,
            text: text.into(),
            message_ids: vec![1],
        }
    }

    fn adapter(answer: Ran, sessions: &Path) -> (ClaudeAdapter, Arc<Mutex<Vec<Spec>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let a = ClaudeAdapter::with_runner(
            "claude".into(),
            None,
            sessions.to_path_buf(),
            Duration::from_secs(60),
            Box::new(Fake {
                seen: seen.clone(),
                answer,
            }),
        );
        (a, seen)
    }

    fn ok() -> Ran {
        Ran {
            code: Some(0),
            stdout: OK.into(),
            ..Ran::default()
        }
    }

    fn empty_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("claude-sessions-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(target_os = "linux")]
    /// The record Claude Code keeps of a process.
    fn record(dir: &Path, pid: u64, proc_start: &str, session: &str, status: &str) {
        let json = format!(
            r#"{{"pid":{pid},"sessionId":"{session}","procStart":"{proc_start}","status":"{status}","kind":"interactive"}}"#
        );
        std::fs::write(dir.join(format!("{pid}.json")), json).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn own_start() -> String {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        stat.rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .nth(19)
            .unwrap()
            .to_string()
    }

    #[test]
    fn the_session_is_resumed_with_the_text_on_standard_input_only() {
        let dir = empty_dir("resume");
        let nasty = "[chappe] From browser-1 (claude@x). Need: `rm -rf /` $(id) ; \"q\"";
        let (a, seen) = adapter(ok(), &dir);
        let out = a.wake(&job(nasty));
        assert!(
            matches!(&out, Outcome::Delivered(c) if c.contains("cache read 29559") && c.contains("$0.0052")),
            "{out:?}"
        );
        let spec = &seen.lock().unwrap()[0];
        assert_eq!(spec.program, "claude");
        assert_eq!(
            spec.args,
            [
                "-p",
                "--resume",
                "53d1f51c-c5b3-4c49-8710-ac0b209dd6b0",
                "--output-format",
                "json",
                "--allowedTools",
                "Bash(chappe:*)"
            ]
        );
        assert_eq!(spec.stdin, nasty, "the text goes on standard input");
        assert!(
            spec.args
                .iter()
                .all(|a| !a.contains("rm -rf") && !a.contains("chappe-bus")),
            "never in the arguments"
        );
        assert_eq!(
            spec.env,
            [("CHAPPE_HANDLE".to_string(), "audio-1a2b".to_string())]
        );
        assert_eq!(spec.cwd.as_deref(), Some(std::env::temp_dir().as_path()));
        assert_eq!(spec.timeout, Duration::from_secs(60));
    }

    #[test]
    fn a_cheaper_model_for_wakes_can_be_chosen() {
        let dir = empty_dir("model");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let a = ClaudeAdapter::with_runner(
            "claude".into(),
            Some("haiku".into()),
            dir,
            Duration::from_secs(5),
            Box::new(Fake {
                seen: seen.clone(),
                answer: ok(),
            }),
        );
        a.wake(&job("x"));
        let args = &seen.lock().unwrap()[0].args;
        assert_eq!(args[args.len() - 2..], ["--model", "haiku"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_session_that_a_live_process_holds_is_not_resumed_in_a_second_one() {
        let dir = empty_dir("holder");
        let me = std::process::id() as u64;
        let sid = "53d1f51c-c5b3-4c49-8710-ac0b209dd6b0";
        for (status, expect) in [
            ("busy", "the session is working"),
            ("waiting", "the session waits for the owner"),
            ("idle", "open in a terminal"),
            ("something-new", "open in a terminal"),
        ] {
            record(&dir, me, &own_start(), sid, status);
            let (a, seen) = adapter(ok(), &dir);
            match a.wake(&job("x")) {
                Outcome::Deferred(why) => assert!(why.contains(expect), "{status}: {why}"),
                other => panic!("{status}: {other:?}"),
            }
            assert!(seen.lock().unwrap().is_empty(), "{status}: nothing was run");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_records_do_not_hold_a_session() {
        let dir = empty_dir("stale");
        let sid = "53d1f51c-c5b3-4c49-8710-ac0b209dd6b0";
        let me = std::process::id() as u64;
        // A dead process, a reused pid (another start time), another session, rubbish.
        record(&dir, 4_000_000_000, "1", sid, "idle");
        record(&dir, me, "1", sid, "idle");
        std::fs::write(dir.join("rubbish.json"), "not json").unwrap();
        std::fs::write(dir.join("note.txt"), "x").unwrap();
        assert_eq!(holder_of(&dir, sid), None);
        std::fs::remove_file(dir.join(format!("{me}.json"))).unwrap();
        record(&dir, me, &own_start(), "another-session", "idle");
        assert_eq!(holder_of(&dir, sid), None, "it holds another session");
        assert_eq!(
            holder_of(&dir, "another-session"),
            Some(Holder {
                pid: me,
                status: "idle".into()
            })
        );
        let (a, seen) = adapter(ok(), &dir);
        assert!(matches!(a.wake(&job("x")), Outcome::Delivered(_)));
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_missing_sessions_directory_means_nobody_holds_it() {
        assert_eq!(holder_of(Path::new("/nonexistent/sessions"), "x"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_running_sessions_are_those_with_a_live_process_and_unknown_when_unreadable() {
        let dir = empty_dir("running");
        assert_eq!(running_sessions(&dir), Some(vec![]));
        let me = std::process::id() as u64;
        record(&dir, me, &own_start(), "live-session", "idle");
        record(&dir, 4_000_000_000, "1", "dead-session", "idle");
        std::fs::write(dir.join("rubbish.json"), "not json").unwrap();
        assert_eq!(running_sessions(&dir), Some(vec!["live-session".into()]));
        // Unreadable is not "nothing runs": the host then says nothing about this tool.
        assert_eq!(running_sessions(Path::new("/nonexistent/sessions")), None);
        let (a, _) = adapter(ok(), &dir);
        assert_eq!(a.running(), Some(vec!["live-session".into()]));
    }

    #[test]
    fn a_session_that_cannot_be_woken_is_said_so_without_running_anything() {
        let dir = empty_dir("cannot");
        let (a, seen) = adapter(ok(), &dir);
        let mut j = job("x");
        j.session_id = None;
        assert!(
            matches!(a.wake(&j), Outcome::Unsupported(w) if w.contains("without a session id"))
        );
        j.session_id = Some("../../etc".into());
        assert!(matches!(a.wake(&j), Outcome::Failed(w) if w.contains("odd session id")));
        j.session_id = Some("53d1f51c-c5b3-4c49-8710-ac0b209dd6b0".into());
        j.worktree = Some("/nonexistent/worktree".into());
        assert!(
            matches!(a.wake(&j), Outcome::Failed(w) if w.contains("worktree of the session is gone"))
        );
        j.worktree = None;
        assert!(matches!(a.wake(&j), Outcome::Failed(_)));
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn failures_of_the_command_are_reported_with_a_reason() {
        let dir = empty_dir("fail");
        let cases = [
            (
                Ran {
                    spawn_error: Some("No such file or directory".into()),
                    ..Ran::default()
                },
                "cannot start claude: No such file",
            ),
            (
                Ran {
                    timed_out: true,
                    ..Ran::default()
                },
                "did not finish in time",
            ),
            (
                Ran {
                    code: Some(1),
                    stderr: "\nAPI error: overloaded\nmore".into(),
                    ..Ran::default()
                },
                "exited with 1: API error: overloaded",
            ),
            (
                Ran {
                    code: None,
                    ..Ran::default()
                },
                "was killed",
            ),
            (
                Ran {
                    code: Some(0),
                    stdout: "garbage".into(),
                    ..Ran::default()
                },
                "printed no result",
            ),
            (
                Ran {
                    code: Some(0),
                    stdout: r#"{"is_error":true,"result":"Session not found"}"#.into(),
                    ..Ran::default()
                },
                "ended with an error: Session not found",
            ),
        ];
        for (ran, expect) in cases {
            let (a, _) = adapter(ran, &dir);
            match a.wake(&job("x")) {
                Outcome::Failed(why) => assert!(why.contains(expect), "{why}"),
                other => panic!("{expect}: {other:?}"),
            }
        }
    }
}
