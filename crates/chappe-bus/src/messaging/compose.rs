//! Building a message from its fields: validation, cleaning and the final text.

use super::*;

/// The fields after cleaning.
pub(super) struct Fields {
    pub(super) to: Option<String>,
    pub(super) topic: String,
    pub(super) what: Option<String>,
    pub(super) why: Option<String>,
    pub(super) expect: Option<String>,
    pub(super) by_secs: Option<i64>,
    pub(super) link: Option<String>,
    pub(super) ask: Option<i64>,
    pub(super) result: Option<String>,
}

/// A single-line optional field: trimmed, empty means absent.
pub(super) fn field(name: &str, value: &Option<String>, max: usize) -> Result<Option<String>> {
    let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    if v.chars().any(char::is_control) {
        return invalid(format!("--{name}: one line, no control characters"));
    }
    if v.chars().count() > max {
        return invalid(format!("--{name}: longer than {max} characters"));
    }
    Ok(Some(v.to_string()))
}

/// `30m` or `2h` in seconds, between a minute and a day.
pub(super) fn parse_by(s: &str) -> Result<i64> {
    let bad = || SendError::Invalid("--by: a time like 30m or 2h, from 1m to 24h".into());
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num.parse().map_err(|_| bad())?;
    let secs = match unit {
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(bad()),
    };
    if !(MIN_BY..=MAX_BY).contains(&secs) {
        return Err(bad());
    }
    Ok(secs)
}

/// Which fields a kind uses; any other is refused so that the format stays fixed.
pub(super) fn check_allowed(kind: Kind, o: &Outgoing) -> Result<()> {
    let used = [
        ("to", o.to.is_some()),
        ("topic", o.topic.is_some()),
        ("what", o.what.is_some()),
        ("why", o.why.is_some()),
        ("expect", o.expect.is_some()),
        ("by", o.by.is_some()),
        ("link", o.link.is_some()),
        ("ask", o.ask.is_some()),
        ("result", o.result.is_some()),
        ("wait", o.wait),
    ];
    let allowed: &[&str] = match kind {
        Kind::Announce => &["topic", "what", "why", "link"],
        Kind::Ask => &["to", "topic", "what", "why", "expect", "by", "link", "wait"],
        Kind::Answer => &["ask", "result", "link"],
        Kind::Notice => &["to", "topic", "what", "why", "link"],
        Kind::Owner => &["topic", "what", "why", "expect", "link"],
    };
    for (name, used) in used {
        if used && !allowed.contains(&name) {
            return invalid(format!("--{name} is not used by {}", kind.as_str()));
        }
    }
    Ok(())
}

impl Fields {
    pub(super) fn clean(kind: Kind, o: &Outgoing) -> Result<Fields> {
        check_allowed(kind, o)?;
        let link = field("link", &o.link, 200)?;
        if link
            .as_deref()
            .is_some_and(|l| !(l.starts_with("https://") || l.starts_with("http://")))
        {
            return invalid("--link: an http or https address");
        }
        let topic = match o.topic.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => {
                clean_topic_name(t).map_err(|e| SendError::Invalid(format!("--topic: {e}")))?
            }
            None => GENERAL.to_string(),
        };
        if topic == crate::forge::EVENTS {
            return invalid("--topic: events is where the service writes the events of the forge");
        }
        let f = Fields {
            to: field("to", &o.to, 40)?,
            topic,
            what: field("what", &o.what, 300)?,
            why: field("why", &o.why, 200)?,
            expect: field("expect", &o.expect, 200)?,
            by_secs: field("by", &o.by, 8)?.map(|b| parse_by(&b)).transpose()?,
            link,
            ask: o.ask,
            result: field("result", &o.result, 500)?,
        };
        let missing = |names: &[(&str, bool)]| -> Result<()> {
            let gone: Vec<_> = names
                .iter()
                .filter(|(_, present)| !present)
                .map(|(n, _)| format!("--{n}"))
                .collect();
            if gone.is_empty() {
                Ok(())
            } else {
                invalid(format!("{} needs {}", kind.as_str(), gone.join(", ")))
            }
        };
        match kind {
            Kind::Announce => missing(&[("what", f.what.is_some())])?,
            Kind::Ask => missing(&[
                ("to", f.to.is_some()),
                ("what", f.what.is_some()),
                ("why", f.why.is_some()),
                ("expect", f.expect.is_some()),
                ("by", f.by_secs.is_some()),
            ])?,
            Kind::Answer => missing(&[("ask", f.ask.is_some()), ("result", f.result.is_some())])?,
            Kind::Notice => missing(&[
                ("to", f.to.is_some()),
                ("what", f.what.is_some()),
                ("why", f.why.is_some()),
            ])?,
            Kind::Owner => missing(&[
                ("what", f.what.is_some()),
                ("why", f.why.is_some()),
                ("expect", f.expect.is_some()),
            ])?,
        }
        Ok(f)
    }
}

/// The text of a message, built from the fields in a fixed format.
pub(super) fn compose(kind: Kind, f: &Fields, now: i64) -> (String, Option<i64>) {
    let mut lines: Vec<(&str, String)> = Vec::new();
    let mut put = |label: &'static str, v: &Option<String>| {
        if let Some(v) = v {
            lines.push((label, v.clone()));
        }
    };
    let mut reply_by = None;
    match kind {
        Kind::Announce => {
            put("Announcing", &f.what);
            put("Why", &f.why);
        }
        Kind::Ask => {
            put("Need", &f.what);
            put("Why", &f.why);
            put("Expect in reply", &f.expect);
            let by = now + f.by_secs.unwrap_or(0);
            reply_by = Some(by);
            lines.push(("Deadline", format!("by {} UTC", format_hm(by))));
        }
        Kind::Answer => put("Answer", &f.result),
        Kind::Notice => {
            put("Changed", &f.what);
            put("Who needs to know", &f.why);
        }
        Kind::Owner => {
            put("Needed from the owner", &f.what);
            put("Why", &f.why);
            put("Expect", &f.expect);
        }
    }
    if let Some(l) = &f.link {
        lines.push(("Link", l.clone()));
    }
    let text = lines
        .iter()
        .map(|(l, v)| format!("{l}: {v}"))
        .collect::<Vec<_>>()
        .join("\n");
    (text, reply_by)
}
