//! The adapter of Codex.
//!
//! "Who holds the session" comes from `~/.codex/thread-writer-locks/<thread>.lock`:
//! a process that writes a thread keeps an `flock` on its file for as long as it
//! runs, and the file is gone when nobody writes the thread. A thread that is
//! locked is never resumed in a second process. The wake is
//! `codex exec resume <thread> -` with the messages on standard input.
//!
//! Codex runs commands in a sandbox that closes the network, loopback included,
//! so a woken session could not reach the service to answer. The wake therefore
//! opens the network for the commands of that one run (`workspace-write` with
//! `network_access`); the owner allowed this. Writes stay limited to the
//! worktree, and nothing is asked of the owner (`approval_policy = "never"`).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::runner::{Ran, Runner, Spec, SystemRunner};
use crate::{Adapter, Job, Outcome};

/// Whether a live process writes the thread.
pub fn thread_is_held(locks: &Path, thread_id: &str) -> bool {
    let file: File = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(locks.join(format!("{thread_id}.lock")))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        // It cannot be told: stay on the safe side.
        Err(_) => return true,
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => true,
    }
}

/// The threads a live process writes: those whose lock file is locked. `None`
/// when the directory cannot be read (nothing is known then, not "nothing runs").
pub fn held_threads(locks: &Path) -> Option<Vec<String>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(locks).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(thread) = name.strip_suffix(".lock")
            && thread_is_held(locks, thread)
        {
            out.push(thread.to_string());
        }
    }
    Some(out)
}

pub struct CodexAdapter {
    program: String,
    model: Option<String>,
    locks_dir: PathBuf,
    timeout: Duration,
    runner: Box<dyn Runner>,
}

impl CodexAdapter {
    pub fn new(
        program: String,
        model: Option<String>,
        locks_dir: PathBuf,
        timeout: Duration,
    ) -> CodexAdapter {
        CodexAdapter::with_runner(program, model, locks_dir, timeout, Box::new(SystemRunner))
    }

    pub fn with_runner(
        program: String,
        model: Option<String>,
        locks_dir: PathBuf,
        timeout: Duration,
        runner: Box<dyn Runner>,
    ) -> CodexAdapter {
        CodexAdapter {
            program,
            model,
            locks_dir,
            timeout,
            runner,
        }
    }

    /// The command that resumes the thread; the text goes on standard input.
    fn spec(&self, job: &Job, thread_id: &str, worktree: PathBuf) -> Spec {
        let mut args: Vec<String> = [
            "exec",
            "resume",
            "--json",
            "--skip-git-repo-check",
            "-c",
            "sandbox_mode=\"workspace-write\"",
            "-c",
            "sandbox_workspace_write.network_access=true",
            "-c",
            "approval_policy=\"never\"",
        ]
        .map(String::from)
        .to_vec();
        if let Some(model) = &self.model {
            args.extend(["-m".into(), model.clone()]);
        }
        args.extend([thread_id.to_string(), "-".into()]);
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

/// What `codex exec --json` printed: the tokens of the finished turn, or why it failed.
fn read_result(stdout: &str) -> Result<String, String> {
    let mut done = None;
    let mut failed = None;
    for event in stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
    {
        match event["type"].as_str() {
            Some("turn.completed") => done = Some(event["usage"].clone()),
            Some("turn.failed") => {
                failed = event["error"]["message"].as_str().map(str::to_string);
            }
            Some("error") => failed = event["message"].as_str().map(str::to_string),
            _ => {}
        }
    }
    if let Some(why) = failed {
        return Err(format!(
            "the woken session ended with an error: {}",
            first_line(&why)
        ));
    }
    let usage = done.ok_or("codex printed no finished turn")?;
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    Ok(format!(
        "input {} (cached {}), output {}",
        n("input_tokens"),
        n("cached_input_tokens"),
        n("output_tokens")
    ))
}

impl Adapter for CodexAdapter {
    fn tool(&self) -> &str {
        "codex"
    }

    fn running(&self) -> Option<Vec<String>> {
        held_threads(&self.locks_dir)
    }

    fn wake(&self, job: &Job) -> Outcome {
        let Some(thread_id) = job.session_id.as_deref() else {
            return Outcome::Unsupported("the session registered without a session id".into());
        };
        if !thread_id
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
        if thread_is_held(&self.locks_dir, thread_id) {
            return Outcome::Deferred(
                "the session is open in a process: not resumed in a second one".into(),
            );
        }
        let Ran {
            code,
            stdout,
            stderr,
            timed_out,
            spawn_error,
        } = self.runner.run(&self.spec(job, thread_id, worktree));
        if let Some(e) = spawn_error {
            return Outcome::Failed(format!("cannot start {}: {e}", self.program));
        }
        if timed_out {
            return Outcome::Failed("the woken session did not finish in time".into());
        }
        match code {
            Some(0) => match read_result(&stdout) {
                Ok(usage) => Outcome::Delivered(usage),
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

    const THREAD: &str = "01a1d231-1349-7583-9d88-6a9e3acfbb44";
    const OK: &str = concat!(
        r#"{"type":"thread.started","thread_id":"01a1d231-1349-7583-9d88-6a9e3acfbb44"}"#,
        "\n",
        r#"{"type":"turn.started"}"#,
        "\n",
        r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"done"}}"#,
        "\n",
        r#"{"type":"turn.completed","usage":{"input_tokens":32182,"cached_input_tokens":23040,"output_tokens":40}}"#,
        "\n"
    );

    fn job(text: &str) -> Job {
        Job {
            id: 7,
            handle: "audio-1a2b".into(),
            tool: "codex".into(),
            session_id: Some(THREAD.into()),
            worktree: Some(std::env::temp_dir().display().to_string()),
            transcript: None,
            text: text.into(),
            message_ids: vec![1],
        }
    }

    fn adapter(answer: Ran, locks: &Path) -> (CodexAdapter, Arc<Mutex<Vec<Spec>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let a = CodexAdapter::with_runner(
            "codex".into(),
            None,
            locks.to_path_buf(),
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
        let d = std::env::temp_dir().join(format!("codex-locks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_thread_is_resumed_with_the_text_on_standard_input_and_the_network_open() {
        let dir = empty_dir("resume");
        let nasty = "[chappe] From browser-1 (claude@x). Need: `rm -rf /` $(id) ; \"q\"";
        let (a, seen) = adapter(ok(), &dir);
        let out = a.wake(&job(nasty));
        assert_eq!(
            out,
            Outcome::Delivered("input 32182 (cached 23040), output 40".into())
        );
        let spec = &seen.lock().unwrap()[0];
        assert_eq!(spec.program, "codex");
        assert_eq!(
            spec.args,
            [
                "exec",
                "resume",
                "--json",
                "--skip-git-repo-check",
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "sandbox_workspace_write.network_access=true",
                "-c",
                "approval_policy=\"never\"",
                THREAD,
                "-"
            ]
        );
        assert_eq!(spec.stdin, nasty, "the text goes on standard input");
        assert!(
            spec.args.iter().all(|a| !a.contains("rm -rf")),
            "never in the arguments"
        );
        assert_eq!(
            spec.env,
            [("CHAPPE_HANDLE".to_string(), "audio-1a2b".to_string())]
        );
        assert_eq!(spec.cwd.as_deref(), Some(std::env::temp_dir().as_path()));
    }

    #[test]
    fn a_cheaper_model_for_wakes_can_be_chosen() {
        let dir = empty_dir("model");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let a = CodexAdapter::with_runner(
            "codex".into(),
            Some("gpt-x".into()),
            dir,
            Duration::from_secs(5),
            Box::new(Fake {
                seen: seen.clone(),
                answer: ok(),
            }),
        );
        a.wake(&job("x"));
        let args = &seen.lock().unwrap()[0].args;
        assert_eq!(args[args.len() - 4..], ["-m", "gpt-x", THREAD, "-"]);
    }

    #[test]
    fn a_locked_thread_is_not_resumed_in_a_second_process() {
        let dir = empty_dir("held");
        let lock = dir.join(format!("{THREAD}.lock"));
        let holder = File::create(&lock).unwrap();
        holder.lock().unwrap();
        assert!(thread_is_held(&dir, THREAD));
        let (a, seen) = adapter(ok(), &dir);
        match a.wake(&job("x")) {
            Outcome::Deferred(why) => assert!(why.contains("not resumed in a second"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(seen.lock().unwrap().is_empty(), "nothing was run");
        // The process ends: the lock is released and the thread is free again.
        drop(holder);
        // A child that another test forks meanwhile holds a copy of the
        // descriptor until it execs; give it a moment.
        let free = (0..100).any(|_| {
            let free = !thread_is_held(&dir, THREAD);
            if !free {
                std::thread::sleep(Duration::from_millis(20));
            }
            free
        });
        assert!(free);
        assert!(matches!(a.wake(&job("x")), Outcome::Delivered(_)));
    }

    #[test]
    fn the_held_threads_are_those_with_a_locked_file() {
        let dir = empty_dir("listed");
        assert_eq!(held_threads(&dir), Some(vec![]));
        let holder = File::create(dir.join("t-held.lock")).unwrap();
        holder.lock().unwrap();
        File::create(dir.join("t-free.lock")).unwrap();
        File::create(dir.join("note.txt")).unwrap();
        assert_eq!(held_threads(&dir), Some(vec!["t-held".into()]));
        let (a, _) = adapter(ok(), &dir);
        assert_eq!(a.running(), Some(vec!["t-held".into()]));
        assert_eq!(held_threads(Path::new("/nonexistent/locks")), None);
    }

    #[test]
    fn a_thread_without_a_lock_file_is_free() {
        assert!(!thread_is_held(&empty_dir("none"), THREAD));
        assert!(!thread_is_held(Path::new("/nonexistent/locks"), THREAD));
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
        j.session_id = Some(THREAD.into());
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
                "cannot start codex: No such file",
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
                    stderr: "\nError: thread/resume failed: no rollout found\nmore".into(),
                    ..Ran::default()
                },
                "exited with 1: Error: thread/resume failed",
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
                "printed no finished turn",
            ),
            (
                Ran {
                    code: Some(0),
                    stdout: r#"{"type":"turn.started"}
{"type":"turn.failed","error":{"message":"usage limit reached"}}"#
                        .into(),
                    ..Ran::default()
                },
                "ended with an error: usage limit reached",
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
