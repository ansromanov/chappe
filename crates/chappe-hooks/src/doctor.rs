//! `chappe doctor`: why a session is not on the bus, in one screen.
//!
//! The usual reasons are quiet. A session that is not started from a terminal (a
//! desktop app, a service) has no `~/.local/bin` in its `PATH`, so a hook fails with
//! "command not found" and nobody sees it; a token is missing or was rotated; the
//! directory is in the wrong project; the host daemon of the machine is down. The
//! doctor looks at each of these the way the thing that needs it would, and says
//! `ok`, `warn` (works, but check) or `FAIL` (does not work). It never prints a token.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::{Client, Env, Reply, hook};

/// The commands of the bus that a session or a machine needs; only the first is
/// needed by a hook.
const COMMANDS: [(&str, bool); 2] = [("chappe", true), ("chappe-host", false)];

/// What the `PATH` of a session not started from a terminal usually holds.
#[cfg(target_os = "macos")]
const BARE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
#[cfg(not(target_os = "macos"))]
const BARE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Ok,
    Warn,
    Fail,
}

struct Report<'a> {
    out: &'a mut dyn Write,
    fails: usize,
    warns: usize,
}

impl Report<'_> {
    fn line(&mut self, level: Level, what: &str, detail: &str) {
        let tag = match level {
            Level::Ok => "ok",
            Level::Warn => {
                self.warns += 1;
                "warn"
            }
            Level::Fail => {
                self.fails += 1;
                "FAIL"
            }
        };
        let _ = writeln!(self.out, "{tag:<5} {what:<32} {detail}");
    }

    fn heading(&mut self, text: &str) {
        let _ = writeln!(self.out, "\n{text}");
    }
}

/// `$HOME`, `${HOME}`, a leading `~` and `$PATH` replaced; nothing else is expanded.
fn expand(text: &str, home: &str, path: &str) -> String {
    let text = match text.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => text.to_string(),
    };
    text.replace("${HOME}", home)
        .replace("$HOME", home)
        .replace("${PATH}", path)
        .replace("$PATH", path)
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Where `name` is found in the directories of `path` (separated by `:`).
fn find_in_path(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| is_executable(candidate))
}

/// Words of a command line, with single and double quotes honoured. Enough for a
/// hook command; this is not a shell.
fn words(line: &str) -> Vec<String> {
    let (mut out, mut word, mut quote, mut started) = (Vec::new(), String::new(), None, false);
    for c in line.chars() {
        match (c, quote) {
            ('"' | '\'', None) => {
                quote = Some(c);
                started = true;
            }
            (c, Some(q)) if c == q => quote = None,
            (c, None) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (c, _) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

/// What a hook command runs: the program, and the `PATH` the command sets for it
/// (`PATH=$HOME/.local/bin:$PATH chappe hook claude SessionStart`), if it sets one.
#[derive(Debug, PartialEq, Eq)]
struct HookCall {
    program: String,
    path: Option<String>,
}

/// Reads a hook command: leading `NAME=value` words are assignments (only `PATH`
/// matters, with `$PATH` standing for `base`), the next word is the program.
fn parse_hook_command(command: &str, home: &str, base: &str) -> Option<HookCall> {
    let mut path = None;
    for word in words(command) {
        match word.split_once('=') {
            Some((name, value))
                if !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !name.starts_with(|c: char| c.is_ascii_digit()) =>
            {
                if name == "PATH" {
                    path = Some(expand(value, home, base));
                }
            }
            _ => {
                return Some(HookCall {
                    program: expand(&word, home, base),
                    path,
                });
            }
        }
    }
    None
}

/// The program of a hook as the hook finds it: a path as written, else the first
/// match in `path`.
fn resolve_program(program: &str, path: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return is_executable(&p).then_some(p);
    }
    find_in_path(program, path)
}

/// The commands of the hooks of Claude Code that run `chappe hook claude`, in the
/// settings text of one file.
fn hook_commands_in(settings: &str) -> Vec<String> {
    let Ok(json) = serde_json::from_str::<Value>(settings) else {
        return vec![];
    };
    let mut out = Vec::new();
    for groups in json["hooks"]
        .as_object()
        .into_iter()
        .flat_map(|events| events.values())
    {
        for group in groups.as_array().into_iter().flatten() {
            for hook in group["hooks"].as_array().into_iter().flatten() {
                if let Some(command) = hook["command"].as_str()
                    && command.contains("hook claude")
                    && !out.iter().any(|c| c == command)
                {
                    out.push(command.to_string());
                }
            }
        }
    }
    out
}

/// The settings files of Claude Code that apply here: the user's, then the project's
/// (`.claude/settings.json` and `.claude/settings.local.json` of the working directory
/// and of every directory above it).
fn settings_files(env: &Env) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = env.get("HOME") {
        files.push(Path::new(home).join(".claude/settings.json"));
    }
    let mut dir = env.cwd.as_deref().map(PathBuf::from);
    while let Some(d) = dir {
        files.push(d.join(".claude/settings.json"));
        files.push(d.join(".claude/settings.local.json"));
        dir = d.parent().map(Path::to_path_buf);
    }
    files
}

fn shown(path: &Path, home: Option<&str>) -> String {
    let text = path.display().to_string();
    match home.and_then(|h| text.strip_prefix(h)) {
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => text,
    }
}

/// How a token file is protected: a warning when others can read it.
fn file_mode_warning(path: &Path) -> Option<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).ok()?.permissions().mode() & 0o777;
        (mode & 0o077 != 0).then(|| {
            format!(
                "{} is readable by others (mode {mode:o}); chmod 600",
                path.display()
            )
        })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn commands(report: &mut Report<'_>, env: &Env) {
    report.heading("Commands, as a hook of a session not started from a terminal sees them");
    let home = env.get("HOME").unwrap_or("");
    let bare = env
        .get("CHAPPE_DOCTOR_HOOK_PATH")
        .unwrap_or(BARE_PATH)
        .to_string();
    let shell_path = env.get("PATH").unwrap_or("").to_string();

    // The hooks in the settings: the command each runs, and with which PATH.
    let mut hook_path = bare.clone();
    let mut found_any = false;
    for file in settings_files(env) {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for command in hook_commands_in(&text) {
            found_any = true;
            let Some(call) = parse_hook_command(&command, home, &bare) else {
                continue;
            };
            let search = call.path.clone().unwrap_or_else(|| bare.clone());
            if call.path.is_some() {
                hook_path = search.clone();
            }
            let label = format!("hook in {}", shown(&file, env.get("HOME")));
            match resolve_program(&call.program, &search) {
                Some(found) => report.line(Level::Ok, &label, &found.display().to_string()),
                None => report.line(
                    Level::Fail,
                    &label,
                    &format!(
                        "`{}` is not found with PATH={search}; the hook fails silently with \"command not found\". Use a hook command that sets PATH or gives the full path (adapters/claude/settings.json), or link the command into a directory of that PATH",
                        call.program
                    ),
                ),
            }
        }
    }
    if !found_any {
        report.line(
            Level::Warn,
            "hooks of Claude Code",
            "no hook `chappe hook claude <Event>` in ~/.claude/settings.json or in a .claude/settings*.json above here: sessions of Claude Code do not register by themselves (see adapters/claude/settings.json)",
        );
    }

    for (name, needed_by_hooks) in COMMANDS {
        let in_shell = find_in_path(name, &shell_path);
        let for_hook = find_in_path(name, &hook_path);
        let label = format!("{name} (PATH of a hook)");
        match (&for_hook, &in_shell) {
            (Some(path), _) => report.line(Level::Ok, &label, &path.display().to_string()),
            (None, Some(path)) => {
                let level = if needed_by_hooks && !found_any {
                    Level::Fail
                } else {
                    Level::Warn
                };
                report.line(
                    level,
                    &label,
                    &format!(
                        "not found with PATH={hook_path}; found in your shell at {}. A session not started from a terminal does not find it unless it is linked into one of those directories, for example `ln -s ~/.local/bin/chappe /usr/local/bin/chappe`",
                        path.display()
                    ),
                );
            }
            (None, None) => {
                let level = if needed_by_hooks {
                    Level::Fail
                } else {
                    Level::Warn
                };
                report.line(
                    level,
                    &label,
                    &format!(
                        "not found, neither with PATH={hook_path} nor in your shell{}",
                        if name == "chappe-host" {
                            " (only needed on a machine whose idle sessions are to be woken)"
                        } else {
                            ""
                        }
                    ),
                );
            }
        }
    }
}

/// The token file `chappe` reads, and what it says about the token (never the token).
fn agent_token(report: &mut Report<'_>, env: &Env) -> bool {
    let origin = if env.get("CHAPPE_TOKEN").is_some() {
        "CHAPPE_TOKEN".to_string()
    } else {
        env.get("CHAPPE_TOKEN_FILE")
            .map(str::to_string)
            .or_else(|| env.default_token_file())
            .unwrap_or_else(|| "(no place to look)".into())
    };
    match env.token() {
        Ok(token) => {
            let note = (!origin.starts_with("CHAPPE_TOKEN"))
                .then(|| file_mode_warning(Path::new(&origin)))
                .flatten();
            let level = if note.is_some() {
                Level::Warn
            } else {
                Level::Ok
            };
            report.line(
                level,
                "agent token",
                &format!(
                    "{origin}, {} characters{}",
                    token.chars().count(),
                    note.map_or(String::new(), |n| format!("; {n}"))
                ),
            );
            true
        }
        Err(why) => {
            report.line(Level::Fail, "agent token", &why);
            false
        }
    }
}

/// The token of the host daemon: only whether it is there.
fn other_tokens(report: &mut Report<'_>, env: &Env) {
    let home = env.get("HOME");
    let host_file = env
        .get("CHAPPE_HOST_TOKEN_FILE")
        .map(str::to_string)
        .or_else(|| {
            env.get("XDG_CONFIG_HOME")
                .map(|d| format!("{d}/chappe/host-token"))
                .or_else(|| home.map(|h| format!("{h}/.config/chappe/host-token")))
        });
    if env.get("CHAPPE_HOST_TOKEN").is_some() {
        report.line(Level::Ok, "host token", "CHAPPE_HOST_TOKEN");
    } else {
        match host_file {
            Some(file) if std::fs::read_to_string(&file).is_ok_and(|t| !t.trim().is_empty()) => {
                match file_mode_warning(Path::new(&file)) {
                    Some(note) => report.line(Level::Warn, "host token", &note),
                    None => report.line(Level::Ok, "host token", &file),
                }
            }
            Some(file) => report.line(
                Level::Warn,
                "host token",
                &format!("none in {file} (needed only where chappe-host runs)"),
            ),
            None => report.line(Level::Warn, "host token", "no place to look (no HOME)"),
        }
    }
}

/// The project and roles of the session that would start in the current directory.
fn project(report: &mut Report<'_>, env: &Env) {
    report.heading("Where a session started here would be");
    let cwd = env.cwd.as_deref();
    match hook::placement(env, cwd) {
        Ok(place) => report.line(
            Level::Ok,
            "project of this directory",
            &format!(
                "{} (roles {}); {}",
                place.project,
                place.roles.join(", "),
                place.why
            ),
        ),
        Err(why) => report.line(
            Level::Warn,
            "project of this directory",
            &format!(
                "not registered by a hook: {why}. Register by hand: chappe register --role dev --project <project>"
            ),
        ),
    }
}

#[derive(Deserialize)]
struct WhoAmI {
    machine: String,
    tool: String,
}

#[derive(Deserialize)]
struct HostStatus {
    machine: String,
    up: bool,
    last_poll_secs_ago: Option<i64>,
}

fn minutes(secs: i64) -> String {
    match secs {
        s if s < 120 => format!("{s} s ago"),
        s if s < 7200 => format!("{} min ago", s / 60),
        s => format!("{} h ago", s / 3600),
    }
}

/// What the service says: that it accepts the token, and whether the host daemon of
/// this machine has asked for work lately.
fn service(report: &mut Report<'_>, env: &Env, client: &Client) {
    report.heading("Service");
    let url = env.url().unwrap_or_default();
    match client.get("/v1/whoami") {
        Reply::Ok(body) => match serde_json::from_str::<WhoAmI>(&body) {
            Ok(me) => report.line(
                Level::Ok,
                "token at the service",
                &format!("{url} accepts it as {}.{}", me.machine, me.tool),
            ),
            Err(_) => report.line(Level::Warn, "token at the service", "an unexpected answer"),
        },
        Reply::Refused { status: 401, .. } => {
            report.line(
                Level::Fail,
                "token at the service",
                &format!("{url} rejects it: ask the owner for a token for this machine and tool"),
            );
            return;
        }
        Reply::Refused { reason, .. } => {
            report.line(Level::Fail, "token at the service", &reason);
            return;
        }
        Reply::Down(why) => {
            report.line(
                Level::Fail,
                "service",
                &format!("{url} is not reachable: {why}"),
            );
            return;
        }
    }
    match client.get("/v1/host/status") {
        Reply::Ok(body) => match serde_json::from_str::<HostStatus>(&body) {
            Ok(host) if host.up => report.line(
                Level::Ok,
                "host daemon",
                &format!(
                    "the daemon of {} asked for work {}",
                    host.machine,
                    minutes(host.last_poll_secs_ago.unwrap_or(0))
                ),
            ),
            Ok(host) => report.line(
                Level::Fail,
                "host daemon",
                &format!(
                    "the daemon of {} has not asked for work{}: idle sessions here are not woken, and a sender is told so (adapters/host/)",
                    host.machine,
                    host.last_poll_secs_ago
                        .map_or(" yet".to_string(), |s| format!(" since {}", minutes(s)))
                ),
            ),
            Err(_) => report.line(Level::Warn, "host daemon", "an unexpected answer"),
        },
        Reply::Refused { status: 404, .. } => report.line(
            Level::Warn,
            "host daemon",
            "this version of the service cannot say",
        ),
        Reply::Refused { reason, .. } | Reply::Down(reason) => {
            report.line(Level::Warn, "host daemon", &reason)
        }
    }
}

/// `chappe doctor`; the exit status is 1 when something does not work.
pub fn run(env: &Env, out: &mut dyn Write) -> i32 {
    let mut report = Report {
        out,
        fails: 0,
        warns: 0,
    };
    let _ = writeln!(report.out, "chappe doctor");
    commands(&mut report, env);
    report.heading("Tokens");
    let token_ok = agent_token(&mut report, env);
    other_tokens(&mut report, env);
    project(&mut report, env);
    report.heading("Service address");
    match env.url() {
        Ok(url) => {
            report.line(Level::Ok, "service address", &url);
            if token_ok && let Ok(token) = env.token() {
                let client = Client::new(&url, token);
                service(&mut report, env, &client);
            }
        }
        Err(e) => report.line(Level::Fail, "service address", &e),
    }
    let (fails, warns) = (report.fails, report.warns);
    let _ = writeln!(
        report.out,
        "\n{}",
        match (fails, warns) {
            (0, 0) => "everything is in order".to_string(),
            (0, w) => format!("works, {w} to check"),
            (f, w) => format!("{f} not working, {w} to check"),
        }
    );
    i32::from(fails > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/dev";

    #[test]
    fn variables_and_the_tilde_are_expanded_in_a_hook_command() {
        let base = "/usr/bin:/bin";
        assert_eq!(expand("~/bin", HOME, base), "/Users/dev/bin");
        assert_eq!(expand("~other", HOME, base), "~other");
        assert_eq!(
            expand("$HOME/.local/bin:${HOME}/x:$PATH", HOME, base),
            "/Users/dev/.local/bin:/Users/dev/x:/usr/bin:/bin"
        );
    }

    #[test]
    fn a_hook_command_is_read_for_its_program_and_the_path_it_sets() {
        let base = "/usr/bin:/bin";
        assert_eq!(
            parse_hook_command("chappe hook claude SessionStart", HOME, base),
            Some(HookCall {
                program: "chappe".into(),
                path: None
            })
        );
        assert_eq!(
            parse_hook_command(
                "PATH=$HOME/.local/bin:/opt/homebrew/bin:$PATH chappe hook claude SessionStart",
                HOME,
                base
            ),
            Some(HookCall {
                program: "chappe".into(),
                path: Some("/Users/dev/.local/bin:/opt/homebrew/bin:/usr/bin:/bin".into())
            })
        );
        assert_eq!(
            parse_hook_command(
                r#"FOO=1 "$HOME/.local/bin/chappe" hook claude PostToolBatch"#,
                HOME,
                base
            ),
            Some(HookCall {
                program: "/Users/dev/.local/bin/chappe".into(),
                path: None
            })
        );
        assert_eq!(parse_hook_command("PATH=/x", HOME, base), None);
        assert_eq!(parse_hook_command("", HOME, base), None);
    }

    #[test]
    fn hooks_of_claude_code_are_found_in_a_settings_file() {
        let text = r#"{"hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "chappe hook claude SessionStart"}]}],
            "PostToolBatch": [{"hooks": [{"type": "command", "command": "chappe hook claude PostToolBatch"},
                                         {"type": "command", "command": "notify-send done"}]}],
            "Stop": [{"hooks": [{"type": "command", "command": "chappe hook claude SessionStart"}]}]
        }}"#;
        assert_eq!(
            {
                let mut found = hook_commands_in(text);
                found.sort();
                found
            },
            [
                "chappe hook claude PostToolBatch",
                "chappe hook claude SessionStart"
            ],
            "other hooks are not ours and a repeated command is listed once"
        );
        assert!(hook_commands_in("not json").is_empty());
        assert!(hook_commands_in("{}").is_empty());
    }

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("doctor-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn executable(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn a_command_is_found_in_the_first_directory_that_has_it_as_an_executable() {
        let (a, b) = (temp_dir("find-a"), temp_dir("find-b"));
        std::fs::write(a.join("chappe"), "not executable").unwrap();
        let in_b = executable(&b, "chappe");
        let path = format!("{}::{}", a.display(), b.display());
        assert_eq!(find_in_path("chappe", &path), Some(in_b.clone()));
        assert_eq!(find_in_path("chappe", &a.display().to_string()), None);
        assert_eq!(find_in_path("missing", &path), None);
        assert_eq!(resolve_program(&in_b.display().to_string(), ""), Some(in_b));
        assert_eq!(resolve_program("/nonexistent/chappe", &path), None);
    }

    #[cfg(unix)]
    #[test]
    fn the_doctor_says_that_a_hook_cannot_find_the_command_and_which_hook_can() {
        let home = temp_dir("home");
        let bin = temp_dir("bin");
        executable(&bin, "chappe");
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        let settings = |command: &str| {
            format!(
                r#"{{"hooks": {{"SessionStart": [{{"hooks": [{{"type": "command", "command": "{command}"}}]}}]}}}}"#
            )
        };
        let env = |home: &Path| {
            let mut vars = std::collections::HashMap::new();
            vars.insert("HOME".to_string(), home.display().to_string());
            // What a hook of a session not started from a terminal has.
            vars.insert("CHAPPE_DOCTOR_HOOK_PATH".into(), "/usr/bin:/bin".into());
            vars.insert("PATH".into(), bin.display().to_string());
            vars.insert("CHAPPE_TOKEN".into(), "x".repeat(40));
            vars.insert("CHAPPE_URL".into(), "http://127.0.0.1:1".into());
            Env {
                vars,
                cwd: Some(home.display().to_string()),
            }
        };
        let doctor = |env: &Env| {
            let mut out = Vec::new();
            let code = run(env, &mut out);
            (code, String::from_utf8(out).unwrap())
        };

        // The old template: bare `chappe`; the shell finds it, the hook does not.
        std::fs::write(
            home.join(".claude/settings.json"),
            settings("chappe hook claude SessionStart"),
        )
        .unwrap();
        let (code, text) = doctor(&env(&home));
        assert_eq!(code, 1, "{text}");
        assert!(
            text.contains("FAIL  hook in ~/.claude/settings.json")
                && text.contains("`chappe` is not found with PATH=/usr/bin:/bin"),
            "{text}"
        );
        assert!(text.contains("chappe (PATH of a hook)"), "{text}");

        // The new template: the command sets its own PATH.
        std::fs::write(
            home.join(".claude/settings.json"),
            settings(&format!(
                "PATH={}:$PATH chappe hook claude SessionStart",
                bin.display()
            )),
        )
        .unwrap();
        let (_, text) = doctor(&env(&home));
        assert!(
            text.contains(&format!(
                "ok    hook in ~/.claude/settings.json  {}/chappe",
                bin.display()
            )) || text.contains(&format!("{}/chappe", bin.display())),
            "{text}"
        );
        assert!(!text.contains("FAIL  hook in"), "{text}");

        // A full path needs no PATH at all.
        std::fs::write(
            home.join(".claude/settings.json"),
            settings(&format!(
                "{}/chappe hook claude SessionStart",
                bin.display()
            )),
        )
        .unwrap();
        let (_, text) = doctor(&env(&home));
        assert!(!text.contains("FAIL  hook in"), "{text}");
    }

    #[test]
    fn the_doctor_without_hooks_or_tokens_says_what_is_missing_and_never_prints_a_token() {
        let home = temp_dir("bare");
        let mut vars = std::collections::HashMap::new();
        vars.insert("HOME".to_string(), home.display().to_string());
        vars.insert("PATH".into(), String::new());
        vars.insert("CHAPPE_DOCTOR_HOOK_PATH".into(), "/nonexistent".into());
        let env = Env {
            vars,
            cwd: Some(home.display().to_string()),
        };
        let mut out = Vec::new();
        let code = run(&env, &mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 1, "{text}");
        assert!(
            text.contains("no hook `chappe hook claude <Event>`"),
            "{text}"
        );
        assert!(text.contains("FAIL  agent token"), "{text}");
        assert!(
            text.contains("host token") && text.contains("FAIL  service address"),
            "{text}"
        );
        assert!(
            text.contains("project of this directory")
                && text.contains("CHAPPE_PROJECTS_ROOT is not set"),
            "{text}"
        );
        assert!(text.contains("not working"), "{text}");
    }
}
