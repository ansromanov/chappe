//! Projects, topics and the messages in them.
//!
//! A project is a body of work (`demo`, `core`; `all` is the one
//! that concerns every project). A topic is a channel inside a project. Every
//! project has `general`, always there and open to everyone; a group of agents
//! working on a part of the system makes a topic for it. A project appears
//! when it is configured or when the first session registers in it.

use rusqlite::{Connection, OptionalExtension, params};

use crate::message::{GENERAL, Kind, clean_about, clean_text, clean_topic_name};
use crate::store::{Result, Store, StoreError, now};

/// Topics a session may create in a day (the owner is not limited).
const TOPICS_PER_DAY: i64 = 3;
/// Topics in one project.
const TOPICS_PER_PROJECT: i64 = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopicInfo {
    pub name: String,
    pub about: Option<String>,
    pub messages: i64,
    pub last_at: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectNav {
    pub name: String,
    /// `general` first, then by name.
    pub topics: Vec<TopicInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub project: String,
    pub topic: String,
    pub sender: String,
    /// The sender's session name when the message was sent.
    pub sender_name: Option<String>,
    pub kind: Kind,
    pub text: String,
    pub created_at: i64,
    /// Who the message is for: `all`, `@role`, a handle or `owner`. Messages
    /// of the owner's page have none.
    pub target: Option<String>,
    /// For an ask: what became of it.
    pub ask: Option<AskInfo>,
    /// For an answer: the id of the ask it answers.
    pub answers: Option<i64>,
}

/// What is known of an ask, for the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskInfo {
    /// `queued`, `delivered` or `expired`; none when nobody was to get it.
    pub delivery: Option<String>,
    pub delivered_at: Option<i64>,
    pub answered_at: Option<i64>,
    pub reply_by: Option<i64>,
}

/// Where an ask stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskState {
    /// Waiting for the receiver to read its inbox.
    Queued,
    Delivered(i64),
    Answered(i64),
    /// The time ran out before it was delivered.
    NotDelivered,
    /// Delivered, and the time ran out without an answer.
    Expired,
}

impl AskInfo {
    pub fn state(&self, now: i64) -> AskState {
        if let Some(at) = self.answered_at {
            return AskState::Answered(at);
        }
        let late = self.reply_by.is_some_and(|b| b < now);
        match (self.delivery.as_deref(), self.delivered_at) {
            (Some("delivered"), Some(_)) if late => AskState::Expired,
            (Some("delivered"), Some(at)) => AskState::Delivered(at),
            (Some("expired"), _) => AskState::NotDelivered,
            _ if late => AskState::NotDelivered,
            _ => AskState::Queued,
        }
    }
}

pub struct NewMessage<'a> {
    pub project: &'a str,
    pub topic: &'a str,
    pub sender: &'a str,
    pub kind: Kind,
    pub text: &'a str,
}

/// Makes sure the project and its `general` topic exist.
pub(crate) fn ensure_project(conn: &Connection, project: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO projects (name) VALUES (?1)",
        [project],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO topics (project, name, about, created_by, created_at)
         VALUES (?1, ?2, 'Project-wide topic, open to everyone', 'system', 0)",
        params![project, GENERAL],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO topics (project, name, about, created_by, created_at)
         VALUES (?1, 'events', 'Forge events: leases, red main, releases, daily summary', 'system', 0)",
        [project],
    )?;
    Ok(())
}

impl Store {
    pub fn ensure_project(&self, project: &str) -> Result<()> {
        Ok(ensure_project(&self.conn(), project)?)
    }

    pub fn ensure_projects(&self, projects: &[String]) -> Result<()> {
        let conn = self.conn();
        for project in projects {
            ensure_project(&conn, project)?;
        }
        Ok(())
    }

    /// Projects and their topics for the sidebar: `all` first, then by name.
    pub fn nav(&self) -> Result<Vec<ProjectNav>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT t.project, t.name, t.about, COUNT(m.id), MAX(m.created_at)
             FROM topics t LEFT JOIN messages m ON m.project = t.project AND m.topic = t.name
             GROUP BY t.project, t.name
             ORDER BY t.project <> 'all', t.project, t.name <> 'general', t.name",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                TopicInfo {
                    name: row.get(1)?,
                    about: row.get(2)?,
                    messages: row.get(3)?,
                    last_at: row.get(4)?,
                },
            ))
        })?;
        let mut nav: Vec<ProjectNav> = Vec::new();
        for row in rows {
            let (project, topic) = row?;
            match nav.last_mut() {
                Some(last) if last.name == project => last.topics.push(topic),
                _ => nav.push(ProjectNav {
                    name: project,
                    topics: vec![topic],
                }),
            }
        }
        Ok(nav)
    }

    pub fn topic(&self, project: &str, topic: &str) -> Result<Option<TopicInfo>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                "SELECT t.name, t.about, COUNT(m.id), MAX(m.created_at)
                 FROM topics t LEFT JOIN messages m ON m.project = t.project AND m.topic = t.name
                 WHERE t.project = ?1 AND t.name = ?2 GROUP BY t.name",
                params![project, topic],
                |row| {
                    Ok(TopicInfo {
                        name: row.get(0)?,
                        about: row.get(1)?,
                        messages: row.get(2)?,
                        last_at: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(found)
    }

    /// Creates a topic. `created_by` is a session handle or `owner`; sessions
    /// are limited to a few new topics a day so that topics do not multiply.
    pub fn create_topic(
        &self,
        project: &str,
        name: &str,
        about: Option<&str>,
        created_by: &str,
        now: i64,
    ) -> Result<TopicInfo> {
        let name = clean_topic_name(name)?;
        let about = clean_about(about)?;
        let conn = self.conn();
        let project_exists = conn
            .query_row("SELECT 1 FROM projects WHERE name = ?1", [project], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if !project_exists {
            return Err(StoreError::NoSuchProject);
        }
        let exists = conn
            .query_row(
                "SELECT 1 FROM topics WHERE project = ?1 AND name = ?2",
                params![project, name],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            return Err(StoreError::TopicExists);
        }
        if created_by != crate::message::OWNER {
            let lately: i64 = conn.query_row(
                "SELECT COUNT(*) FROM topics WHERE created_by = ?1 AND created_at > ?2",
                params![created_by, now - 86_400],
                |r| r.get(0),
            )?;
            if lately >= TOPICS_PER_DAY {
                return Err(StoreError::TopicLimit(
                    "no more than three new topics a day: use an existing one",
                ));
            }
        }
        let in_project: i64 = conn.query_row(
            "SELECT COUNT(*) FROM topics WHERE project = ?1",
            [project],
            |r| r.get(0),
        )?;
        if in_project >= TOPICS_PER_PROJECT {
            return Err(StoreError::TopicLimit("the project already has 50 topics"));
        }
        conn.execute(
            "INSERT INTO topics (project, name, about, created_by, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![project, name, about, created_by, now],
        )?;
        Ok(TopicInfo {
            name,
            about,
            messages: 0,
            last_at: None,
        })
    }

    /// Validates and stores a message in an existing topic.
    pub fn add_message(&self, new: &NewMessage<'_>) -> Result<Message> {
        let text = clean_text(new.text)?;
        let created_at = now();
        let conn = self.conn();
        let topic_exists = conn
            .query_row(
                "SELECT 1 FROM topics WHERE project = ?1 AND name = ?2",
                params![new.project, new.topic],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !topic_exists {
            return Err(StoreError::NoSuchTopic);
        }
        conn.execute(
            "INSERT INTO messages (project, topic, sender, kind, text, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                new.project,
                new.topic,
                new.sender,
                new.kind.as_str(),
                text,
                created_at
            ],
        )?;
        Ok(Message {
            id: conn.last_insert_rowid(),
            project: new.project.to_string(),
            topic: new.topic.to_string(),
            sender: new.sender.to_string(),
            sender_name: None,
            kind: new.kind,
            text,
            created_at,
            target: None,
            ask: None,
            answers: None,
        })
    }

    /// The project and topic a message was written in.
    pub fn message_place(&self, id: i64) -> Result<Option<(String, String)>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT project, topic FROM messages WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// The last `limit` messages of a topic, oldest first.
    pub fn recent_messages(
        &self,
        project: &str,
        topic: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, project, topic, sender, kind, text, created_at, target, sender_name,
                    (SELECT d.state FROM deliveries d WHERE d.message_id = x.id LIMIT 1),
                    (SELECT d.delivered_at FROM deliveries d WHERE d.message_id = x.id LIMIT 1),
                    (SELECT a.created_at FROM messages a WHERE a.answers = x.id LIMIT 1),
                    reply_by, answers
             FROM (
                 SELECT * FROM messages WHERE project = ?1 AND topic = ?2 ORDER BY id DESC LIMIT ?3
             ) x ORDER BY id",
        )?;
        let rows = stmt.query_map(params![project, topic, limit as i64], |row| {
            let kind: String = row.get(4)?;
            Ok(Message {
                id: row.get(0)?,
                project: row.get(1)?,
                topic: row.get(2)?,
                sender: row.get(3)?,
                kind: Kind::parse(&kind).ok_or(rusqlite::Error::InvalidColumnType(
                    4,
                    "kind".into(),
                    rusqlite::types::Type::Text,
                ))?,
                text: row.get(5)?,
                created_at: row.get(6)?,
                target: row.get(7)?,
                sender_name: row.get(8)?,
                ask: (kind == "ask")
                    .then(|| -> rusqlite::Result<AskInfo> {
                        Ok(AskInfo {
                            delivery: row.get(9)?,
                            delivered_at: row.get(10)?,
                            answered_at: row.get(11)?,
                            reply_by: row.get(12)?,
                        })
                    })
                    .transpose()?,
                answers: row.get(13)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// An owner's announcement in the general topic, for tests of other modules.
    #[cfg(test)]
    pub(crate) fn post_for_test(&self, project: &str, text: &str) {
        self.add_message(&NewMessage {
            project,
            topic: GENERAL,
            sender: crate::message::OWNER,
            kind: Kind::Announce,
            text,
        })
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::OWNER;

    const T0: i64 = 1_800_000_000;

    fn store() -> Store {
        let s = Store::open_in_memory().unwrap();
        s.ensure_projects(&["demo".into(), "all".into()]).unwrap();
        s
    }

    fn post(s: &Store, project: &str, topic: &str, text: &str) -> Result<Message> {
        s.add_message(&NewMessage {
            project,
            topic,
            sender: OWNER,
            kind: Kind::Announce,
            text,
        })
    }

    #[test]
    fn every_project_has_general_and_all_comes_first() {
        let nav = store().nav().unwrap();
        let names: Vec<_> = nav.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["all", "demo"]);
        for p in &nav {
            // `general` for everyone, `events` for what the service writes from the forge.
            let topics: Vec<_> = p.topics.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(topics, [GENERAL, "events"], "{}", p.name);
            assert_eq!((p.topics[0].messages, p.topics[0].last_at), (0, None));
        }
    }

    #[test]
    fn ensuring_a_project_is_idempotent_and_keeps_its_topics() {
        let s = store();
        s.create_topic("demo", "audio", None, OWNER, T0).unwrap();
        s.ensure_projects(&["all".into(), "demo".into()]).unwrap();
        assert_eq!(
            s.nav().unwrap()[1].topics.len(),
            3,
            "general, events and audio"
        );
    }

    #[test]
    fn a_new_project_appears_with_general() {
        let s = store();
        s.ensure_project("core").unwrap();
        let nav = s.nav().unwrap();
        let names: Vec<_> = nav.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["all", "core", "demo"]);
        assert_eq!(nav[1].topics[0].name, GENERAL);
    }

    #[test]
    fn a_message_is_found_by_its_number() {
        let s = store();
        s.create_topic("demo", "audio", None, OWNER, T0).unwrap();
        let m = post(&s, "demo", "audio", "hello").unwrap();
        assert_eq!(
            s.message_place(m.id).unwrap(),
            Some(("demo".to_string(), "audio".to_string()))
        );
        assert_eq!(s.message_place(m.id + 1000).unwrap(), None);
    }

    #[test]
    fn topics_are_listed_with_general_first() {
        let s = store();
        for name in ["zeta", "audio", "browser"] {
            s.create_topic("demo", name, None, OWNER, T0).unwrap();
        }
        let nav = s.nav().unwrap();
        let topics: Vec<_> = nav[1].topics.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(topics, ["general", "audio", "browser", "events", "zeta"]);
    }

    #[test]
    fn create_topic_normalises_the_name_and_keeps_the_description() {
        let s = store();
        let t = s
            .create_topic(
                "demo",
                "  Audio Driver ",
                Some("sound driver"),
                "audio-1a2b",
                T0,
            )
            .unwrap();
        assert_eq!(t.name, "audio-driver");
        assert_eq!(
            s.topic("demo", "audio-driver")
                .unwrap()
                .unwrap()
                .about
                .as_deref(),
            Some("sound driver")
        );
        assert_eq!(s.topic("demo", "nope").unwrap(), None);
    }

    #[test]
    fn general_exists_always_and_cannot_be_created_again() {
        let s = store();
        assert!(matches!(
            s.create_topic("demo", "general", None, OWNER, T0),
            Err(StoreError::TopicExists)
        ));
        assert!(matches!(
            s.create_topic("demo", "General", None, OWNER, T0),
            Err(StoreError::TopicExists)
        ));
        assert!(s.topic("demo", GENERAL).unwrap().is_some());
    }

    #[test]
    fn a_topic_cannot_be_created_twice_or_in_a_missing_project() {
        let s = store();
        s.create_topic("demo", "audio", None, OWNER, T0).unwrap();
        assert!(matches!(
            s.create_topic("demo", "Audio", None, OWNER, T0),
            Err(StoreError::TopicExists)
        ));
        assert!(matches!(
            s.create_topic("nope", "audio", None, OWNER, T0),
            Err(StoreError::NoSuchProject)
        ));
        // The same name in another project is another topic.
        assert!(s.create_topic("all", "audio", None, OWNER, T0).is_ok());
    }

    #[test]
    fn bad_names_are_refused() {
        let s = store();
        for name in ["", "a/b", "<script>", "a#b"] {
            assert!(
                matches!(
                    s.create_topic("demo", name, None, OWNER, T0),
                    Err(StoreError::Invalid(_))
                ),
                "{name:?}"
            );
        }
        assert_eq!(
            s.nav().unwrap()[1].topics.len(),
            2,
            "general and events only"
        );
    }

    #[test]
    fn a_session_may_create_three_topics_a_day_but_the_owner_any_number() {
        let s = store();
        for i in 0..3 {
            s.create_topic("demo", &format!("t{i}"), None, "audio-1a2b", T0 + i)
                .unwrap();
        }
        let err = s
            .create_topic("demo", "t3", None, "audio-1a2b", T0 + 10)
            .unwrap_err();
        assert!(matches!(err, StoreError::TopicLimit(_)), "{err}");
        // Another session is not affected, and a day later the limit is gone.
        assert!(
            s.create_topic("demo", "t4", None, "kernel-9f9f", T0 + 10)
                .is_ok()
        );
        assert!(
            s.create_topic("demo", "t5", None, "audio-1a2b", T0 + 86_401 + 2)
                .is_ok()
        );
        for i in 0..5 {
            s.create_topic("demo", &format!("o{i}"), None, OWNER, T0)
                .unwrap();
        }
    }

    #[test]
    fn a_project_holds_at_most_fifty_topics() {
        let s = store();
        // `general` and `events` count too.
        for i in 0..48 {
            s.create_topic("demo", &format!("t{i}"), None, OWNER, T0)
                .unwrap();
        }
        assert!(matches!(
            s.create_topic("demo", "one-more", None, OWNER, T0),
            Err(StoreError::TopicLimit(_))
        ));
    }

    #[test]
    fn messages_come_back_oldest_first_and_trimmed_to_the_limit() {
        let s = store();
        for i in 0..5 {
            post(&s, "all", "general", &format!("m{i}")).unwrap();
        }
        let texts: Vec<_> = s
            .recent_messages("all", "general", 3)
            .unwrap()
            .into_iter()
            .map(|m| m.text)
            .collect();
        assert_eq!(texts, ["m2", "m3", "m4"]);
        assert_eq!(s.nav().unwrap()[0].topics[0].messages, 5);
    }

    #[test]
    fn topics_and_projects_do_not_mix() {
        let s = store();
        s.create_topic("all", "laptop", None, OWNER, T0).unwrap();
        post(&s, "all", "laptop", "a").unwrap();
        assert!(s.recent_messages("all", "general", 10).unwrap().is_empty());
        assert!(s.recent_messages("demo", "laptop", 10).unwrap().is_empty());
        assert_eq!(s.recent_messages("all", "laptop", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_message_needs_an_existing_topic_and_valid_text() {
        let s = store();
        assert!(matches!(
            post(&s, "all", "nope", "x"),
            Err(StoreError::NoSuchTopic)
        ));
        assert!(matches!(
            post(&s, "nope", "general", "x"),
            Err(StoreError::NoSuchTopic)
        ));
        assert!(matches!(
            post(&s, "all", "general", "  "),
            Err(StoreError::Invalid(_))
        ));
        assert!(matches!(
            post(&s, "all", "general", &"x".repeat(601)),
            Err(StoreError::Invalid(_))
        ));
        assert!(s.recent_messages("all", "general", 10).unwrap().is_empty());
    }

    #[test]
    fn every_kind_is_accepted_by_the_schema() {
        let s = store();
        for kind in Kind::ALL {
            let new = NewMessage {
                project: "all",
                topic: "general",
                sender: "x",
                kind,
                text: "x",
            };
            assert_eq!(s.add_message(&new).unwrap().kind, kind);
        }
        // The closed list is enforced by the database itself.
        let err = s.conn().execute(
            "INSERT INTO messages (project, topic, sender, kind, text, created_at)
             VALUES ('all', 'general', 'x', 'greeting', 'x', 0)",
            [],
        );
        assert!(err.is_err());
    }

    #[test]
    fn text_is_stored_verbatim() {
        let s = store();
        let raw = "<script>alert('x')</script> & \"quotes\"";
        post(&s, "all", "general", raw).unwrap();
        assert_eq!(s.recent_messages("all", "general", 1).unwrap()[0].text, raw);
    }
}
