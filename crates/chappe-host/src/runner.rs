//! Runs a fixed command with a text on standard input.
//!
//! The text of a message never goes into the command line and never through a
//! shell: the program and its arguments are a list, the text is written to the
//! program's standard input.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A command to run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub stdin: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ran {
    /// `None` when it did not start or was killed.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Why it did not start, if it did not.
    pub spawn_error: Option<String>,
}

pub trait Runner {
    fn run(&self, spec: &Spec) -> Ran;
}

/// Runs the command on this machine.
pub struct SystemRunner;

fn drain<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

impl Runner for SystemRunner {
    fn run(&self, spec: &Spec) -> Ran {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return Ran {
                    spawn_error: Some(e.to_string()),
                    ..Ran::default()
                };
            }
        };
        let writer = child.stdin.take().map(|mut stdin| {
            let text = spec.stdin.clone();
            std::thread::spawn(move || {
                let _ = stdin.write_all(text.as_bytes());
            })
        });
        let (out, err) = (
            child.stdout.take().map(drain),
            child.stderr.take().map(drain),
        );
        let started = Instant::now();
        let (mut code, mut timed_out) = (None, false);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    code = status.code();
                    break;
                }
                Ok(None) if started.elapsed() >= spec.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => break,
            }
        }
        if let Some(w) = writer {
            let _ = w.join();
        }
        Ran {
            code,
            stdout: out.and_then(|h| h.join().ok()).unwrap_or_default(),
            stderr: err.and_then(|h| h.join().ok()).unwrap_or_default(),
            timed_out,
            spawn_error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(program: &str, args: &[&str], stdin: &str) -> Spec {
        Spec {
            program: program.into(),
            args: args.iter().map(|a| a.to_string()).collect(),
            stdin: stdin.into(),
            timeout: Duration::from_secs(10),
            ..Spec::default()
        }
    }

    #[test]
    fn the_text_arrives_on_standard_input_untouched() {
        let nasty = "$(touch /tmp/pwned) `id` ; rm -rf / \"quote\" 'q' \\n %s\nline two";
        let r = SystemRunner.run(&spec("cat", &[], nasty));
        assert_eq!((r.code, r.stdout.as_str()), (Some(0), nasty));
    }

    #[test]
    fn arguments_are_not_interpreted_by_a_shell() {
        let r = SystemRunner.run(&spec("echo", &["$(echo injected)", ";", "&&"], ""));
        assert_eq!(r.stdout, "$(echo injected) ; &&\n");
    }

    #[test]
    fn working_directory_and_environment_are_set() {
        let dir = std::env::temp_dir();
        let mut s = spec(
            "sh",
            &["-c", "printf '%s|%s' \"$PWD\" \"$CHAPPE_HANDLE\""],
            "",
        );
        s.cwd = Some(dir.clone());
        s.env = vec![("CHAPPE_HANDLE".into(), "audio-1a2b".into())];
        let out = SystemRunner.run(&s).stdout;
        assert!(out.ends_with("|audio-1a2b"), "{out}");
        assert!(
            out.starts_with(dir.canonicalize().unwrap().to_str().unwrap())
                || out.starts_with(dir.to_str().unwrap())
        );
    }

    #[test]
    fn exit_status_and_error_output_are_returned() {
        let r = SystemRunner.run(&spec("sh", &["-c", "echo oops >&2; exit 3"], ""));
        assert_eq!((r.code, r.stderr.trim()), (Some(3), "oops"));
    }

    #[test]
    fn a_command_that_runs_too_long_is_killed() {
        let mut s = spec("sleep", &["30"], "");
        s.timeout = Duration::from_millis(300);
        let started = Instant::now();
        let r = SystemRunner.run(&s);
        assert!(r.timed_out && r.code.is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_missing_program_is_reported_not_panicked_on() {
        let r = SystemRunner.run(&spec("/nonexistent/program", &[], ""));
        assert!(r.spawn_error.is_some() && r.code.is_none());
    }

    #[test]
    fn a_large_text_does_not_block_a_program_that_reads_it_late() {
        let big = "x".repeat(2_000_000);
        let r = SystemRunner.run(&spec("wc", &["-c"], &big));
        assert_eq!(r.stdout.trim(), "2000000");
    }
}
