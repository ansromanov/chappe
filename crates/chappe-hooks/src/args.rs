//! Command line of `chappe`: four subcommands and their flags.

pub const USAGE: &str = "\
usage: chappe <command> [flags]

  register --role ROLE[,ROLE...] --project PROJECT [--issue TEXT] [--branch NAME]
           [--worktree PATH] [--session-id ID] [--transcript PATH] [--name TEXT]
      Start of work. Prints the session handle; pass it to beat and leave.
  beat [--handle HANDLE]
      A sign of life. Send one at each checkpoint of your project's AGENTS.md.
  who [--project PROJECT] [--role ROLE]
      The sessions that have not ended.
  leave [--handle HANDLE]
      End of work.
  topic create NAME [--about TEXT] [--handle HANDLE]
      A topic in your project for a part of the system a group of agents works
      on. Every project has `general`, open to everyone. A few new topics a day.
  topics [--project PROJECT]
      The topics of the projects.
  doctor
      Why this session or machine is not on the bus: the commands as a hook sees
      them, the tokens, the project of this directory, the host daemon. Prints no token.

Messages, only for a real need. Each is built from fields; there is no free text,
and no way to greet, thank, acknowledge or report progress. Add --handle HANDLE
(or set CHAPPE_HANDLE) and, if not `general`, --topic NAME.

  announce --what TEXT [--why TEXT] [--link URL] [--topic NAME]
      You are about to change a shared resource (goes to project `all`, to everyone).
  ask --to @ROLE|HANDLE|project:NAME --what TEXT --why TEXT --expect TEXT --by 30m [--link URL] [--wait] [--send]
      You are blocked on something only that area can answer. Without --send it
      only shows who would get it, to look first; the answer may be there already.
      An idle receiver is woken (at most twice an hour); --wait lets its answer
      wake you in turn.
  answer --ask NUMBER --result TEXT
      The one, final answer to an ask sent to you.
  notice --to @ROLE|HANDLE --what TEXT --why TEXT [--link URL]
      You changed or broke something that area depends on.
  owner --what TEXT --why TEXT --expect TEXT
      You are blocked on a decision or permission only the owner can give.
  inbox
      What is waiting for you, in one block. At most once in five minutes.

The handle may also be given in CHAPPE_HANDLE. The token is read from
CHAPPE_TOKEN or the file CHAPPE_TOKEN_FILE (default
~/.config/chappe/token); the service address is CHAPPE_URL.";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Register {
    pub roles: Vec<String>,
    pub project: Option<String>,
    pub issue: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub session_id: Option<String>,
    pub transcript: Option<String>,
    /// The name the tool shows for the session; `CHAPPE_NAME` or the tool's
    /// own record when absent.
    pub name: Option<String>,
}

/// A message: the kind and the fields; the service checks which fields the kind uses.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub kind: String,
    pub handle: Option<String>,
    pub to: Option<String>,
    pub topic: Option<String>,
    pub what: Option<String>,
    pub why: Option<String>,
    pub expect: Option<String>,
    pub by: Option<String>,
    pub link: Option<String>,
    pub ask: Option<i64>,
    pub result: Option<String>,
    /// `ask` only: really send, after looking.
    pub send: bool,
    /// `ask` only: the answer may wake the sender, if it is idle by then.
    pub wait: bool,
}

/// The kinds of message there are: a closed list.
pub const KINDS: [&str; 5] = ["announce", "ask", "answer", "notice", "owner"];

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Message(Message),
    Inbox {
        handle: Option<String>,
    },
    Register(Register),
    Beat {
        handle: Option<String>,
    },
    Leave {
        handle: Option<String>,
    },
    Who {
        project: Option<String>,
        role: Option<String>,
    },
    TopicCreate {
        name: String,
        about: Option<String>,
        handle: Option<String>,
    },
    Topics {
        project: Option<String>,
    },
    Doctor,
    Help,
}

/// Splits `--flag value` and `--flag=value` into pairs; every flag takes a value.
fn flags(args: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(format!("unexpected argument {arg:?}"));
        };
        if flag == "send" || flag == "wait" {
            out.push((flag.to_string(), "true".to_string()));
            continue;
        }
        match flag.split_once('=') {
            Some((name, value)) => out.push((name.to_string(), value.to_string())),
            None => {
                let value = it.next().ok_or_else(|| format!("--{flag} needs a value"))?;
                out.push((flag.to_string(), value.clone()));
            }
        }
    }
    Ok(out)
}

/// Takes the single value of `name`; a second occurrence is an error.
fn take(pairs: &mut Vec<(String, String)>, name: &str) -> Result<Option<String>, String> {
    if pairs.iter().filter(|(n, _)| n == name).count() > 1 {
        return Err(format!("--{name} given more than once"));
    }
    Ok(pairs
        .iter()
        .position(|(n, _)| n == name)
        .map(|i| pairs.remove(i).1))
}

fn no_more(pairs: Vec<(String, String)>) -> Result<(), String> {
    match pairs.first() {
        Some((name, _)) => Err(format!("unknown flag --{name}")),
        None => Ok(()),
    }
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((command, rest)) = args.split_first() else {
        return Err("no command".into());
    };
    if matches!(command.as_str(), "help" | "--help" | "-h") {
        return Ok(Command::Help);
    }
    // `topic create NAME` is the one command with a word and a name before its flags.
    let (rest, name) = if command == "topic" {
        match rest {
            [sub, name, tail @ ..] if sub == "create" && !name.starts_with("--") => {
                (tail, Some(name.clone()))
            }
            _ => return Err("usage: topic create NAME [--about TEXT] [--handle HANDLE]".into()),
        }
    } else {
        (rest, None)
    };
    let mut pairs = flags(rest)?;
    let parsed = match command.as_str() {
        "register" => {
            let roles = pairs
                .iter()
                .filter(|(n, _)| n == "role")
                .flat_map(|(_, v)| v.split(','))
                .map(|r| r.trim().to_string())
                .filter(|r| !r.is_empty())
                .collect();
            pairs.retain(|(n, _)| n != "role");
            Command::Register(Register {
                roles,
                project: take(&mut pairs, "project")?,
                issue: take(&mut pairs, "issue")?,
                branch: take(&mut pairs, "branch")?,
                worktree: take(&mut pairs, "worktree")?,
                session_id: take(&mut pairs, "session-id")?,
                transcript: take(&mut pairs, "transcript")?,
                name: take(&mut pairs, "name")?,
            })
        }
        kind if KINDS.contains(&kind) => Command::Message(Message {
            kind: kind.to_string(),
            handle: take(&mut pairs, "handle")?,
            to: take(&mut pairs, "to")?,
            topic: take(&mut pairs, "topic")?,
            what: take(&mut pairs, "what")?,
            why: take(&mut pairs, "why")?,
            expect: take(&mut pairs, "expect")?,
            by: take(&mut pairs, "by")?,
            link: take(&mut pairs, "link")?,
            ask: take(&mut pairs, "ask")?
                .map(|n| {
                    n.trim_start_matches('#')
                        .parse()
                        .map_err(|_| "--ask: a message number".to_string())
                })
                .transpose()?,
            result: take(&mut pairs, "result")?,
            send: take(&mut pairs, "send")?.is_some(),
            wait: take(&mut pairs, "wait")?.is_some(),
        }),
        "inbox" => Command::Inbox {
            handle: take(&mut pairs, "handle")?,
        },
        "beat" => Command::Beat {
            handle: take(&mut pairs, "handle")?,
        },
        "leave" => Command::Leave {
            handle: take(&mut pairs, "handle")?,
        },
        "topic" => Command::TopicCreate {
            name: name.unwrap_or_default(),
            about: take(&mut pairs, "about")?,
            handle: take(&mut pairs, "handle")?,
        },
        "topics" => Command::Topics {
            project: take(&mut pairs, "project")?,
        },
        "doctor" => Command::Doctor,
        "who" => Command::Who {
            project: take(&mut pairs, "project")?,
            role: take(&mut pairs, "role")?,
        },
        other => return Err(format!("unknown command {other:?}")),
    };
    no_more(pairs)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(line: &str) -> Result<Command, String> {
        parse(
            &line
                .split_whitespace()
                .map(str::to_string)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn register_collects_roles_from_repeated_and_comma_separated_flags() {
        let Ok(Command::Register(r)) = parse_str(
            "register --role audio,desktop --role=kernel --project demo --issue=471 --session-id abc --name=Audio",
        ) else {
            panic!("not a register")
        };
        assert_eq!(r.roles, ["audio", "desktop", "kernel"]);
        assert_eq!(
            (
                r.project.as_deref(),
                r.issue.as_deref(),
                r.session_id.as_deref()
            ),
            (Some("demo"), Some("471"), Some("abc"))
        );
        assert_eq!(r.branch, None);
        assert_eq!(r.name.as_deref(), Some("Audio"));
    }

    #[test]
    fn handle_commands() {
        assert_eq!(
            parse_str("beat --handle audio-1a2b"),
            Ok(Command::Beat {
                handle: Some("audio-1a2b".into())
            })
        );
        assert_eq!(parse_str("leave"), Ok(Command::Leave { handle: None }));
        assert_eq!(
            parse_str("who --project p --role r"),
            Ok(Command::Who {
                project: Some("p".into()),
                role: Some("r".into())
            })
        );
    }

    #[test]
    fn topic_commands() {
        assert_eq!(
            parse_str("topic create audio-driver --about driver --handle audio-1a2b"),
            Ok(Command::TopicCreate {
                name: "audio-driver".into(),
                about: Some("driver".into()),
                handle: Some("audio-1a2b".into())
            })
        );
        assert_eq!(
            parse_str("topic create x"),
            Ok(Command::TopicCreate {
                name: "x".into(),
                about: None,
                handle: None
            })
        );
        assert_eq!(
            parse_str("topics --project demo"),
            Ok(Command::Topics {
                project: Some("demo".into())
            })
        );
        assert!(parse_str("topic").unwrap_err().contains("usage"));
        assert!(parse_str("topic delete x").unwrap_err().contains("usage"));
        assert!(
            parse_str("topic create --about x")
                .unwrap_err()
                .contains("usage")
        );
        assert!(
            parse_str("topic create x --role r")
                .unwrap_err()
                .contains("unknown flag")
        );
    }

    #[test]
    fn message_commands_collect_their_fields() {
        let Ok(Command::Message(m)) = parse_str(
            "ask --to @audio --what x --why y --expect z --by 30m --topic audio-driver --handle b-1 --send",
        ) else {
            panic!("not a message")
        };
        assert_eq!(m.kind, "ask");
        assert_eq!(
            (m.to.as_deref(), m.by.as_deref(), m.topic.as_deref()),
            (Some("@audio"), Some("30m"), Some("audio-driver"))
        );
        assert!(m.send);
        let Ok(Command::Message(a)) = parse_str("answer --ask #12 --result ok") else {
            panic!()
        };
        assert_eq!(
            (a.ask, a.result.as_deref(), a.send),
            (Some(12), Some("ok"), false)
        );
        assert_eq!(
            parse_str("inbox --handle b-1"),
            Ok(Command::Inbox {
                handle: Some("b-1".into())
            })
        );
        assert!(
            parse_str("answer --ask abc --result x")
                .unwrap_err()
                .contains("--ask")
        );
        assert!(
            parse_str("announce --nope x")
                .unwrap_err()
                .contains("unknown flag")
        );
        assert!(
            parse_str("announce --what")
                .unwrap_err()
                .contains("needs a value")
        );
    }

    #[test]
    fn the_kinds_are_the_closed_list_of_the_design() {
        assert_eq!(KINDS, ["announce", "ask", "answer", "notice", "owner"]);
    }

    #[test]
    fn help() {
        for h in ["help", "--help", "-h"] {
            assert_eq!(parse_str(h), Ok(Command::Help));
        }
    }

    #[test]
    fn mistakes_are_reported() {
        assert!(parse_str("").is_err());
        assert!(parse_str("greet").unwrap_err().contains("unknown command"));
        assert!(
            parse_str("beat --nope x")
                .unwrap_err()
                .contains("unknown flag --nope")
        );
        assert!(
            parse_str("beat --handle")
                .unwrap_err()
                .contains("needs a value")
        );
        assert!(
            parse_str("beat now")
                .unwrap_err()
                .contains("unexpected argument")
        );
        assert!(
            parse_str("who --project a --project b")
                .unwrap_err()
                .contains("more than once")
        );
        assert!(
            parse_str("beat --role x")
                .unwrap_err()
                .contains("unknown flag --role")
        );
    }

    #[test]
    fn there_is_no_way_to_post_a_greeting_or_a_progress_report() {
        // The design closes the list of occasions: no greetings, thanks,
        // acknowledgements or progress reports, in any stage.
        for c in [
            "say", "post", "send", "hello", "thanks", "ack", "status", "progress",
        ] {
            assert!(parse_str(c).is_err(), "{c} must not exist");
        }
    }
}
