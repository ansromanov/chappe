//! What a message is and which messages the service accepts.
//!
//! The kinds are the closed list of occasions from the design ("Keeping the
//! chat quiet"). The limits here apply to every sender, the owner included.

use std::fmt;

/// Longest message text, in characters. Longer material is a link.
pub const MAX_TEXT_CHARS: usize = 600;
/// Longest topic name, in characters.
pub const MAX_TOPIC_CHARS: usize = 40;
/// Longest description of a topic, in characters.
pub const MAX_ABOUT_CHARS: usize = 120;
/// The topic every project has, always open to everyone.
pub const GENERAL: &str = "general";
/// Sender name used for messages written on the page.
pub const OWNER: &str = "owner";
/// The sender of the lines the service writes from the events of the forge.
pub const FORGE: &str = "forge";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Announce,
    Ask,
    Answer,
    Notice,
    Owner,
}

impl Kind {
    pub const ALL: [Kind; 5] = [
        Kind::Announce,
        Kind::Ask,
        Kind::Answer,
        Kind::Notice,
        Kind::Owner,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Announce => "announce",
            Kind::Ask => "ask",
            Kind::Answer => "answer",
            Kind::Notice => "notice",
            Kind::Owner => "owner",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Short label for the page.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Announce => "announcement",
            Kind::Ask => "request",
            Kind::Answer => "answer",
            Kind::Notice => "notice",
            Kind::Owner => "to the owner",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    BadTopicName,
    AboutTooLong,
    EmptyText,
    TextTooLong,
    ControlChars,
}

impl fmt::Display for Invalid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let reason = match self {
            Invalid::BadTopicName => {
                format!(
                    "topic name: letters, digits and '-', no longer than {MAX_TOPIC_CHARS} characters"
                )
            }
            Invalid::AboutTooLong => {
                format!("description longer than {MAX_ABOUT_CHARS} characters")
            }
            Invalid::EmptyText => "text cannot be empty".to_string(),
            Invalid::TextTooLong => format!("text longer than {MAX_TEXT_CHARS} characters"),
            Invalid::ControlChars => "the text contains control characters".to_string(),
        };
        f.write_str(&reason)
    }
}

impl std::error::Error for Invalid {}

/// The name of a topic as people type it, turned into the form it is kept in:
/// lowercase, words joined with `-` ("Audio driver" becomes `audio-driver`).
pub fn clean_topic_name(name: &str) -> Result<String, Invalid> {
    let mut out = String::new();
    for c in name.trim().chars() {
        let c = if c.is_whitespace() || c == '_' {
            '-'
        } else {
            c
        };
        if c == '-' {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            return Err(Invalid::BadTopicName);
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() || out.chars().count() > MAX_TOPIC_CHARS {
        return Err(Invalid::BadTopicName);
    }
    Ok(out)
}

/// An optional one-line description of a topic.
pub fn clean_about(about: Option<&str>) -> Result<Option<String>, Invalid> {
    let Some(about) = about.map(str::trim).filter(|a| !a.is_empty()) else {
        return Ok(None);
    };
    if about.chars().any(char::is_control) {
        return Err(Invalid::ControlChars);
    }
    if about.chars().count() > MAX_ABOUT_CHARS {
        return Err(Invalid::AboutTooLong);
    }
    Ok(Some(about.to_string()))
}

/// Trims the text, turns `\r\n` into `\n`, and rejects control characters
/// other than newline and tab (the text is shown as plain text, nothing else).
pub fn clean_text(text: &str) -> Result<String, Invalid> {
    let text = text.replace("\r\n", "\n");
    let text = text.trim();
    if text.is_empty() {
        return Err(Invalid::EmptyText);
    }
    if text
        .chars()
        .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(Invalid::ControlChars);
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(Invalid::TextTooLong);
    }
    Ok(text.to_string())
}

/// Whether the text holds something that looks like an access token or a
/// private key: a run of 32 or more hex digits (our tokens are 64) or the
/// words of a PEM key header. A UUID has dashes and does not match.
pub fn looks_like_secret(text: &str) -> bool {
    if text.contains("PRIVATE KEY") {
        return true;
    }
    let mut run = 0;
    for c in text.chars() {
        run = if c.is_ascii_hexdigit() { run + 1 } else { 0 };
        if run >= 32 {
            return true;
        }
    }
    false
}

/// `2026-11-12 14:03` in UTC for a Unix time in seconds.
pub fn format_utc(secs: i64) -> String {
    let (y, m, d) = date_of(secs);
    format!("{y:04}-{m:02}-{d:02} {}", format_hm(secs))
}

/// `14:03` in UTC.
pub fn format_hm(secs: i64) -> String {
    let rem = secs.rem_euclid(86_400);
    format!("{:02}:{:02}", rem / 3600, rem % 3600 / 60)
}

/// `2026-11-12T14:03:00Z`, for the `datetime` attribute of `<time>`.
pub fn iso_utc(secs: i64) -> String {
    let (y, m, d) = date_of(secs);
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The calendar day of a Unix time, as a number that changes at midnight UTC.
pub fn day_of(secs: i64) -> i64 {
    secs.div_euclid(86_400)
}

/// `12 November 2026`, for the dividers between days in a topic.
pub fn format_date(secs: i64) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (y, m, d) = date_of(secs);
    format!("{d} {} {y}", MONTHS[(m - 1) as usize])
}

fn date_of(secs: i64) -> (i64, i64, i64) {
    civil_from_days(secs.div_euclid(86_400))
}

/// Days since 1970-01-01 to a calendar date (Howard Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for kind in Kind::ALL {
            assert_eq!(Kind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(Kind::parse("greeting"), None);
    }

    #[test]
    fn topic_names_are_normalised() {
        for (raw, clean) in [
            ("audio", "audio"),
            ("  Audio driver ", "audio-driver"),
            ("sound_in__browser", "sound-in-browser"),
            ("-edge-", "edge"),
            ("Zürich in browser", "zürich-in-browser"),
            ("471", "471"),
            ("a\nb", "a-b"),
        ] {
            assert_eq!(clean_topic_name(raw).unwrap(), clean, "{raw:?}");
        }
    }

    #[test]
    fn bad_topic_names_are_refused() {
        for raw in ["", "   ", "---", "a/b", "a#b", "<b>", "a\u{1b}b", "💥"] {
            assert_eq!(clean_topic_name(raw), Err(Invalid::BadTopicName), "{raw:?}");
        }
        let longest = "ü".repeat(MAX_TOPIC_CHARS);
        assert_eq!(clean_topic_name(&longest).unwrap(), longest);
        assert_eq!(
            clean_topic_name(&format!("{longest}ü")),
            Err(Invalid::BadTopicName)
        );
    }

    #[test]
    fn about_is_optional_one_line_and_short() {
        assert_eq!(clean_about(None), Ok(None));
        assert_eq!(clean_about(Some("  ")), Ok(None));
        assert_eq!(clean_about(Some(" driver ")), Ok(Some("driver".into())));
        assert_eq!(clean_about(Some("a\nb")), Err(Invalid::ControlChars));
        assert_eq!(
            clean_about(Some(&"x".repeat(MAX_ABOUT_CHARS + 1))),
            Err(Invalid::AboutTooLong)
        );
    }

    #[test]
    fn text_limit_counts_characters_not_bytes() {
        let at_limit = "é".repeat(MAX_TEXT_CHARS);
        assert!(at_limit.len() > MAX_TEXT_CHARS);
        assert!(clean_text(&at_limit).is_ok());
        assert_eq!(
            clean_text(&format!("{at_limit}é")),
            Err(Invalid::TextTooLong)
        );
    }

    #[test]
    fn text_normalises_newlines_and_rejects_control_characters() {
        assert_eq!(clean_text("a\r\nb\tc").unwrap(), "a\nb\tc");
        assert_eq!(clean_text(" \n "), Err(Invalid::EmptyText));
        assert_eq!(clean_text("a\u{1b}[31mb"), Err(Invalid::ControlChars));
        assert_eq!(clean_text("a\0b"), Err(Invalid::ControlChars));
    }

    #[test]
    fn secrets_are_recognised() {
        assert!(looks_like_secret(&"a1b2c3d4".repeat(8)));
        assert!(looks_like_secret(&format!(
            "token {} here",
            "0f".repeat(16)
        )));
        assert!(looks_like_secret("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(!looks_like_secret("3f2b8c1e-9d4a-4e7b-a1c2-5d6e7f8a9b0c"));
        assert!(!looks_like_secret(&"0f".repeat(15)));
        assert!(!looks_like_secret("claude/fix-deadbeef refs #471 svcd"));
    }

    #[test]
    fn date_helpers() {
        assert_eq!(format_hm(1_794_491_000), "13:43");
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_utc(1_794_491_000), "2026-11-12T13:43:20Z");
        assert_eq!(format_date(1_794_491_000), "12 November 2026");
        assert_eq!(format_date(951_782_400), "29 February 2000");
        assert_eq!(day_of(86_399), 0);
        assert_eq!(day_of(86_400), 1);
        assert_eq!(day_of(-1), -1);
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(
            format_utc(951_782_400 + 3600 * 23 + 60 * 59),
            "2000-02-29 23:59"
        );
        assert_eq!(format_utc(1_794_491_000), "2026-11-12 13:43");
        assert_eq!(format_utc(-1), "1969-12-31 23:59");
    }
}
