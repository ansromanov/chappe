//! SQLite storage: the connection, the schema and its migrations, access tokens.
//! Projects, topics and messages are in `chat.rs`, sessions in `registry.rs`.
//!
//! One connection behind a mutex. The queries are tiny and the service has
//! one replica, so a blocking call inside a handler costs microseconds.

use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::message::Invalid;

/// Each entry moves the schema one version up; `PRAGMA user_version` records
/// how many have been applied. Never edit an entry that has shipped: add one.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE rooms (name TEXT PRIMARY KEY) STRICT;

    CREATE TABLE messages (
        id INTEGER PRIMARY KEY,
        room TEXT NOT NULL REFERENCES rooms(name),
        topic TEXT NOT NULL,
        sender TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('announce', 'ask', 'answer', 'notice', 'owner')),
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL
    ) STRICT;
    CREATE INDEX messages_by_room ON messages (room, id);

    CREATE TABLE tokens (
        name TEXT PRIMARY KEY,
        machine TEXT NOT NULL,
        tool TEXT NOT NULL,
        token_hash BLOB NOT NULL UNIQUE,
        created_at INTEGER NOT NULL,
        last_used_at INTEGER
    ) STRICT;
",
    "
    CREATE TABLE sessions (
        handle TEXT PRIMARY KEY,
        machine TEXT NOT NULL,
        tool TEXT NOT NULL,
        roles TEXT NOT NULL,
        project TEXT NOT NULL,
        issue TEXT,
        branch TEXT,
        worktree TEXT,
        session_id TEXT,
        transcript TEXT,
        registered_at INTEGER NOT NULL,
        last_seen INTEGER NOT NULL,
        left_at INTEGER
    ) STRICT;
    CREATE INDEX sessions_by_project ON sessions (project, last_seen);
",
    "
    -- Rooms become projects, and a message belongs to a topic of a project.
    ALTER TABLE rooms RENAME TO projects;
    ALTER TABLE messages RENAME COLUMN room TO project;
    DROP INDEX messages_by_room;
    CREATE INDEX messages_by_topic ON messages (project, topic, id);
    CREATE TABLE topics (
        project TEXT NOT NULL REFERENCES projects(name),
        name TEXT NOT NULL,
        about TEXT,
        created_by TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (project, name)
    ) STRICT;
    -- Every project has a general topic that is always there.
    INSERT INTO topics (project, name, about, created_by, created_at)
        SELECT name, 'general', 'Project-wide topic, open to everyone', 'system', 0 FROM projects;
    -- Free-text topics of the first versions fold into general; the old topic
    -- stays readable as a prefix of the text.
    UPDATE messages SET text = '[' || topic || '] ' || text, topic = 'general';
",
    "
    -- Messages of agents: who they are for, the answer to an ask, the delivery
    -- to each receiving session, and what the rules turned away.
    ALTER TABLE messages ADD COLUMN target TEXT;
    ALTER TABLE messages ADD COLUMN to_handle TEXT;
    ALTER TABLE messages ADD COLUMN reply_by INTEGER;
    ALTER TABLE messages ADD COLUMN answers INTEGER REFERENCES messages(id);
    ALTER TABLE messages ADD COLUMN from_project TEXT;
    CREATE UNIQUE INDEX one_answer_per_ask ON messages (answers) WHERE answers IS NOT NULL;
    CREATE INDEX messages_by_sender ON messages (sender, created_at);
    CREATE TABLE deliveries (
        message_id INTEGER NOT NULL REFERENCES messages(id),
        handle TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('queued', 'delivered', 'expired')),
        delivered_at INTEGER,
        PRIMARY KEY (message_id, handle)
    ) STRICT;
    CREATE INDEX deliveries_by_handle ON deliveries (handle, state);
    CREATE TABLE rejections (
        id INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        sender TEXT NOT NULL,
        project TEXT NOT NULL,
        kind TEXT NOT NULL,
        reason TEXT NOT NULL
    ) STRICT;
    CREATE INDEX rejections_by_time ON rejections (project, at);
",
    "
    -- Waking idle sessions: only an ask wakes (an answer when the ask said --wait).
    ALTER TABLE messages ADD COLUMN wait INTEGER NOT NULL DEFAULT 0;
    CREATE TABLE wakes (
        id INTEGER PRIMARY KEY,
        handle TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        not_before INTEGER NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'done', 'expired')),
        attempts INTEGER NOT NULL DEFAULT 0,
        note TEXT
    ) STRICT;
    CREATE INDEX wakes_by_handle ON wakes (handle, created_at);
    -- What the service tells a session itself, for example that an ask was not delivered.
    CREATE TABLE notes (
        id INTEGER PRIMARY KEY,
        handle TEXT NOT NULL,
        text TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        delivered_at INTEGER
    ) STRICT;
    CREATE INDEX notes_by_handle ON notes (handle, delivered_at);
    -- When the host daemon of a machine last asked for work: without one, nothing wakes.
    CREATE TABLE hosts (machine TEXT PRIMARY KEY, last_poll INTEGER NOT NULL) STRICT;
",
    "
    -- The name the tool shows for a session (\"Graphics and sound\"), next to the handle.
    -- A message keeps the name its sender had when it was sent.
    ALTER TABLE sessions ADD COLUMN name TEXT;
    ALTER TABLE messages ADD COLUMN sender_name TEXT;
",
    "
    -- Events from Gitea (webhooks): leases on shared resources, merged pull
    -- requests and claimed issues (counted, not posted), the state of main.
    CREATE TABLE gitea_deliveries (id TEXT PRIMARY KEY, at INTEGER NOT NULL) STRICT;
    CREATE TABLE leases (
        repo TEXT NOT NULL,
        issue INTEGER NOT NULL,
        comment_id INTEGER NOT NULL,
        title TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('using', 'exclusive', 'released')),
        agent TEXT NOT NULL,
        action TEXT,
        result TEXT,
        starts INTEGER,
        until INTEGER NOT NULL,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (repo, comment_id)
    ) STRICT;
    CREATE INDEX leases_by_issue ON leases (repo, issue, agent, comment_id);
    CREATE TABLE activity (
        kind TEXT NOT NULL CHECK (kind IN ('merged', 'claimed')),
        repo TEXT NOT NULL,
        ref TEXT NOT NULL,
        project TEXT NOT NULL,
        at INTEGER NOT NULL,
        PRIMARY KEY (kind, repo, ref)
    ) STRICT;
    CREATE INDEX activity_by_time ON activity (project, at);
    CREATE TABLE main_state (
        repo TEXT NOT NULL,
        workflow TEXT NOT NULL,
        red INTEGER NOT NULL,
        since INTEGER NOT NULL,
        PRIMARY KEY (repo, workflow)
    ) STRICT;
    CREATE TABLE digests (project TEXT NOT NULL, day INTEGER NOT NULL, PRIMARY KEY (project, day)) STRICT;
    -- Every project has a topic where the lines from Gitea are written.
    INSERT OR IGNORE INTO topics (project, name, about, created_by, created_at)
        SELECT name, 'events', 'Gitea events: leases, red main, releases, daily summary', 'system', 0 FROM projects;
",
    "
    -- An ask whose time ran out without an answer is closed once, and its sender told once.
    CREATE TABLE ask_closed (message_id INTEGER PRIMARY KEY, at INTEGER NOT NULL) STRICT;
",
    "
    -- Issue claims from Gitea (`agent-lock` comments): which issue a branch or a
    -- worktree works on, so that the board can say it without the agent repeating it.
    CREATE TABLE claims (
        repo TEXT NOT NULL,
        issue INTEGER NOT NULL,
        comment_id INTEGER NOT NULL,
        title TEXT NOT NULL,
        agent TEXT,
        worktree TEXT,
        branch TEXT,
        state TEXT NOT NULL CHECK (state IN ('claimed', 'released', 'merged')),
        at INTEGER NOT NULL,
        PRIMARY KEY (repo, comment_id)
    ) STRICT;
    CREATE INDEX claims_by_branch ON claims (branch);
",
    "
    -- When the host daemon of the session's machine last found no process for it. A
    -- session silent for two hours with this set is gone (it never receives a question
    -- addressed to a role or a project); any signal from the session clears it.
    ALTER TABLE sessions ADD COLUMN process_gone_at INTEGER;
",
    "
    -- The webhooks come from Gitea, GitHub or GitLab: the names say so.
    ALTER TABLE gitea_deliveries RENAME TO forge_deliveries;
    UPDATE messages SET sender = 'forge' WHERE sender = 'gitea';
    UPDATE topics SET about = 'Forge events: leases, red main, releases, daily summary'
        WHERE name = 'events' AND about = 'Gitea events: leases, red main, releases, daily summary';
",
];

/// `last_used_at` is refreshed at most this often, so polling does not write.
const LAST_USED_GRANULARITY_SECS: i64 = 60;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Invalid(Invalid),
    NoSuchProject,
    NoSuchTopic,
    TopicExists,
    /// Too many topics created lately, or in the project.
    TopicLimit(&'static str),
    /// The file was written by a newer version of the service.
    NewerSchema {
        found: usize,
        known: usize,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "database error: {e}"),
            StoreError::Invalid(e) => write!(f, "{e}"),
            StoreError::NoSuchProject => f.write_str("no such project"),
            StoreError::NoSuchTopic => f.write_str("no such topic"),
            StoreError::TopicExists => f.write_str("such a topic already exists"),
            StoreError::TopicLimit(why) => f.write_str(why),
            StoreError::NewerSchema { found, known } => {
                write!(
                    f,
                    "database schema v{found} is newer than this build (v{known})"
                )
            }
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl From<Invalid> for StoreError {
    fn from(e: Invalid) -> Self {
        StoreError::Invalid(e)
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A token as the service keeps it: the hash, never the token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenEntry {
    pub name: String,
    pub machine: String,
    pub tool: String,
    pub hash: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    pub machine: String,
    pub tool: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenSync {
    pub added: usize,
    pub rotated: usize,
    pub removed: usize,
}

pub struct Store {
    conn: Mutex<Connection>,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        Store::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Store> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&mut conn)?;
        Ok(Store {
            conn: Mutex::new(conn),
        })
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        // A poisoned lock only means a handler panicked; the data is intact.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn schema_version(&self) -> Result<usize> {
        Ok(schema_version(&self.conn())?)
    }

    /// Cheap liveness check for the health endpoint.
    pub fn ping(&self) -> Result<()> {
        self.conn().query_row("SELECT 1", [], |_| Ok(()))?;
        Ok(())
    }

    /// Writes a consistent copy of the database to `dest`, which must not exist.
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        let dest = dest.to_string_lossy().into_owned();
        self.conn().execute("VACUUM INTO ?1", [dest])?;
        Ok(())
    }

    /// Makes the table equal to `entries`: adds new names, replaces the hash of
    /// a name whose token changed, removes names that are gone.
    pub fn sync_tokens(&self, entries: &[TokenEntry]) -> Result<TokenSync> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut sync = TokenSync::default();
        let ts = now();
        {
            let mut existing = tx.prepare("SELECT token_hash FROM tokens WHERE name = ?1")?;
            let mut upsert = tx.prepare(
                "INSERT INTO tokens (name, machine, tool, token_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (name) DO UPDATE SET
                     machine = excluded.machine, tool = excluded.tool,
                     token_hash = excluded.token_hash, created_at = excluded.created_at",
            )?;
            for e in entries {
                let old: Option<Vec<u8>> =
                    existing.query_row([&e.name], |r| r.get(0)).optional()?;
                match old {
                    Some(h) if h == e.hash => continue,
                    Some(_) => sync.rotated += 1,
                    None => sync.added += 1,
                }
                // A rotated token may collide with another name's old hash
                // until that name is processed, so free the hash first.
                tx.execute(
                    "DELETE FROM tokens WHERE token_hash = ?1 AND name <> ?2",
                    params![e.hash.as_slice(), e.name],
                )?;
                upsert.execute(params![e.name, e.machine, e.tool, e.hash.as_slice(), ts])?;
            }
        }
        let keep: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        let current: Vec<String> = {
            let mut stmt = tx.prepare("SELECT name FROM tokens")?;
            stmt.query_map([], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        for name in current.iter().filter(|n| !keep.contains(&n.as_str())) {
            tx.execute("DELETE FROM tokens WHERE name = ?1", [name])?;
            sync.removed += 1;
        }
        tx.commit()?;
        Ok(sync)
    }

    /// Who owns this token hash, if anyone. Notes the use.
    pub fn authenticate(&self, hash: &[u8; 32]) -> Result<Option<Agent>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                "SELECT name, machine, tool FROM tokens WHERE token_hash = ?1",
                [hash.as_slice()],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        Agent {
                            machine: r.get(1)?,
                            tool: r.get(2)?,
                        },
                    ))
                },
            )
            .optional()?;
        let Some((name, agent)) = found else {
            return Ok(None);
        };
        let ts = now();
        conn.execute(
            "UPDATE tokens SET last_used_at = ?2
             WHERE name = ?1 AND (last_used_at IS NULL OR last_used_at <= ?3)",
            params![name, ts, ts - LAST_USED_GRANULARITY_SECS],
        )?;
        Ok(Some(agent))
    }
}

fn schema_version(conn: &Connection) -> rusqlite::Result<usize> {
    conn.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .map(|v| v as usize)
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let found = schema_version(conn)?;
    if found > MIGRATIONS.len() {
        return Err(StoreError::NewerSchema {
            found,
            known: MIGRATIONS.len(),
        });
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(found) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn entry(name: &str, byte: u8) -> TokenEntry {
        let (machine, tool) = name.split_once('.').unwrap();
        TokenEntry {
            name: name.into(),
            machine: machine.into(),
            tool: tool.into(),
            hash: [byte; 32],
        }
    }

    #[test]
    fn new_database_is_at_the_latest_schema() {
        assert_eq!(store().schema_version().unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn the_names_of_gitea_become_the_names_of_the_forge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.db");
        {
            let conn = old_database(&path, MIGRATIONS.len() - 1);
            conn.execute("INSERT INTO projects (name) VALUES ('demo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO topics (project, name, about, created_by, created_at)
                 VALUES ('demo', 'events', 'Gitea events: leases, red main, releases, daily summary', 'system', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (project, topic, sender, kind, text, created_at)
                 VALUES ('demo', 'events', 'gitea', 'notice', 'Lease taken', 1)",
                [],
            )
            .unwrap();
            conn.execute("INSERT INTO gitea_deliveries (id, at) VALUES ('d1', 1)", [])
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let conn = s.conn();
        let delivery: String = conn
            .query_row("SELECT id FROM forge_deliveries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(delivery, "d1");
        let sender: String = conn
            .query_row("SELECT sender FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sender, "forge");
        let about: String = conn
            .query_row("SELECT about FROM topics WHERE name = 'events'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(about.starts_with("Forge events"));
    }

    #[test]
    fn migrations_are_applied_once_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.db");
        {
            let s = Store::open(&path).unwrap();
            s.ensure_project("all").unwrap();
            s.post_for_test("all", "hello");
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len());
        assert_eq!(s.recent_messages("all", "general", 10).unwrap().len(), 1);
    }

    /// A database as an older version left it: the first `version` migrations applied.
    fn old_database(path: &Path, version: usize) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        for sql in &MIGRATIONS[..version] {
            conn.execute_batch(sql).unwrap();
        }
        conn.pragma_update(None, "user_version", version as i64)
            .unwrap();
        conn
    }

    #[test]
    fn a_v1_database_is_upgraded_and_keeps_its_data() {
        // What was deployed first: schema v1 (rooms, messages, tokens), with a message.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.db");
        {
            let conn = old_database(&path, 1);
            conn.execute("INSERT INTO rooms (name) VALUES ('all')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO messages (room, topic, sender, kind, text, created_at)
                 VALUES ('all', 'laptop', 'owner', 'announce', 'kept', 1)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len());
        let sessions: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 0);
        let messages = s.recent_messages("all", "general", 10).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0].text, "[laptop] kept",
            "the old topic is kept as a prefix"
        );
    }

    #[test]
    fn a_v2_database_gets_general_in_every_project() {
        // What is deployed now: schema v2, rooms all and demo, no messages.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.db");
        {
            let conn = old_database(&path, 2);
            conn.execute("INSERT INTO rooms (name) VALUES ('all'), ('demo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO sessions (handle, machine, tool, roles, project, registered_at, last_seen)
                 VALUES ('audio-1', 'mac', 'codex', 'audio', 'demo', 5, 5)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len());
        let nav = s.nav().unwrap();
        let names: Vec<_> = nav.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["all", "demo"]);
        for project in &nav {
            let topics: Vec<_> = project.topics.iter().map(|t| t.name.as_str()).collect();
            assert_eq!(topics, ["general", "events"], "{}", project.name);
        }
        let sessions: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sessions, 1, "sessions survive");
    }

    #[test]
    fn a_v3_database_gets_the_message_tables_and_keeps_its_messages() {
        // What is deployed now: schema v3, a message of the owner in a topic.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.db");
        {
            let conn = old_database(&path, 3);
            conn.execute("INSERT INTO projects (name) VALUES ('demo')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO topics (project, name, created_by, created_at) VALUES ('demo', 'general', 'system', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (project, topic, sender, kind, text, created_at)
                 VALUES ('demo', 'general', 'owner', 'announce', 'kept', 1)",
                [],
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), MIGRATIONS.len());
        let kept = s.recent_messages("demo", "general", 10).unwrap();
        assert_eq!(
            (kept[0].text.as_str(), kept[0].target.as_deref()),
            ("kept", None)
        );
        for table in ["deliveries", "rejections"] {
            let n: i64 = s
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
        assert!(
            s.counters(10).unwrap().is_empty(),
            "old messages have no sender project"
        );
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bus.db");
        Store::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", (MIGRATIONS.len() + 1) as i64)
            .unwrap();
        drop(conn);
        assert!(matches!(
            Store::open(&path),
            Err(StoreError::NewerSchema { .. })
        ));
    }

    #[test]
    fn tokens_authenticate_by_hash() {
        let s = store();
        let sync = s
            .sync_tokens(&[entry("host-a.claude", 1), entry("mac.codex", 2)])
            .unwrap();
        assert_eq!(
            sync,
            TokenSync {
                added: 2,
                rotated: 0,
                removed: 0
            }
        );
        let agent = s.authenticate(&[1; 32]).unwrap().unwrap();
        assert_eq!(
            agent,
            Agent {
                machine: "host-a".into(),
                tool: "claude".into()
            }
        );
        assert_eq!(s.authenticate(&[9; 32]).unwrap(), None);
    }

    #[test]
    fn sync_rotates_and_removes() {
        let s = store();
        s.sync_tokens(&[entry("a.claude", 1), entry("b.codex", 2)])
            .unwrap();
        let sync = s.sync_tokens(&[entry("a.claude", 3)]).unwrap();
        assert_eq!(
            sync,
            TokenSync {
                added: 0,
                rotated: 1,
                removed: 1
            }
        );
        assert_eq!(
            s.authenticate(&[1; 32]).unwrap(),
            None,
            "old token no longer works"
        );
        assert!(s.authenticate(&[3; 32]).unwrap().is_some());
        assert_eq!(
            s.authenticate(&[2; 32]).unwrap(),
            None,
            "removed name no longer works"
        );
    }

    #[test]
    fn sync_unchanged_is_a_no_op() {
        let s = store();
        let entries = [entry("a.claude", 1)];
        s.sync_tokens(&entries).unwrap();
        assert_eq!(s.sync_tokens(&entries).unwrap(), TokenSync::default());
    }

    #[test]
    fn last_use_is_recorded_once_per_interval() {
        let s = store();
        s.sync_tokens(&[entry("a.claude", 1)]).unwrap();
        let last = |s: &Store| -> Option<i64> {
            s.conn()
                .query_row("SELECT last_used_at FROM tokens", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(last(&s), None);
        s.authenticate(&[1; 32]).unwrap();
        let first = last(&s).unwrap();
        s.conn()
            .execute("UPDATE tokens SET last_used_at = ?1", [first + 1_000])
            .unwrap();
        s.authenticate(&[1; 32]).unwrap();
        assert_eq!(
            last(&s),
            Some(first + 1_000),
            "a recent value is not overwritten"
        );
    }

    #[test]
    fn backup_is_a_readable_copy() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("bus.db");
        let copy = dir.path().join("copy.db");
        let s = Store::open(&src).unwrap();
        s.ensure_project("all").unwrap();
        s.post_for_test("all", "kept");
        s.backup_to(&copy).unwrap();
        let restored = Store::open(&copy).unwrap();
        assert_eq!(
            restored.recent_messages("all", "general", 10).unwrap()[0].text,
            "kept"
        );
        assert!(
            s.backup_to(&copy).is_err(),
            "never overwrites an existing file"
        );
    }
}
