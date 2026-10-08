//! SQLite persistence: registered projects, worktree port slots, agent
//! sessions (for resume after a daemon restart) and the TUI's UI state.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Result;
use hive_core::protocol::SessionKind;
use rusqlite::{params, Connection, OptionalExtension};

const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE projects (id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, added_at INTEGER NOT NULL);
CREATE TABLE slots (path TEXT PRIMARY KEY, project TEXT NOT NULL, slot INTEGER NOT NULL);
CREATE TABLE sessions (
    id TEXT PRIMARY KEY, project TEXT NOT NULL, worktree TEXT NOT NULL, kind TEXT NOT NULL,
    title TEXT NOT NULL, cwd TEXT NOT NULL, agent_session TEXT, last_prompt TEXT, created_at INTEGER NOT NULL
);
CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#];

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: String,
    pub project: String,
    pub worktree: PathBuf,
    pub kind: SessionKind,
    pub title: String,
    pub cwd: PathBuf,
    pub agent_session: Option<String>,
    pub last_prompt: Option<String>,
    pub created_at: i64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(version as usize) {
            conn.execute_batch(m)?;
            conn.pragma_update(None, "user_version", (i + 1) as i64)?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn projects(&self) -> Result<Vec<(String, PathBuf)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT id, root FROM projects ORDER BY added_at")?;
        let rows = st.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                PathBuf::from(r.get::<_, String>(1)?),
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Returns the project's id (existing or new).
    pub fn add_project(&self, root: &Path) -> Result<String> {
        let c = self.conn.lock().unwrap();
        let root_s = root.display().to_string();
        if let Some(id) = c
            .query_row("SELECT id FROM projects WHERE root = ?1", [&root_s], |r| {
                r.get(0)
            })
            .optional()?
        {
            return Ok(id);
        }
        let id = hive_core::new_id();
        c.execute(
            "INSERT INTO projects (id, root, added_at) VALUES (?1, ?2, ?3)",
            params![id, root_s, hive_core::now_ms()],
        )?;
        Ok(id)
    }

    pub fn remove_project(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM projects WHERE id = ?1", [id])?;
        c.execute("DELETE FROM slots WHERE project = ?1", [id])?;
        c.execute("DELETE FROM sessions WHERE project = ?1", [id])?;
        Ok(())
    }

    pub fn slots(&self, project: &str) -> Result<Vec<(PathBuf, u16)>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT path, slot FROM slots WHERE project = ?1")?;
        let rows = st.query_map([project], |r| {
            Ok((PathBuf::from(r.get::<_, String>(0)?), r.get::<_, u16>(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn set_slot(&self, project: &str, path: &Path, slot: u16) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO slots (path, project, slot) VALUES (?1, ?2, ?3) ON CONFLICT(path) DO UPDATE SET slot = ?3, project = ?2",
            params![path.display().to_string(), project, slot],
        )?;
        Ok(())
    }

    pub fn free_slot(&self, path: &Path) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "DELETE FROM slots WHERE path = ?1",
            [path.display().to_string()],
        )?;
        Ok(())
    }

    pub fn save_session(&self, s: &StoredSession) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO sessions (id, project, worktree, kind, title, cwd, agent_session, last_prompt, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(id) DO UPDATE SET title = ?5, agent_session = ?7, last_prompt = ?8",
            params![
                s.id,
                s.project,
                s.worktree.display().to_string(),
                serde_json::to_string(&s.kind)?,
                s.title,
                s.cwd.display().to_string(),
                s.agent_session,
                s.last_prompt,
                s.created_at
            ],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM sessions WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn sessions(&self) -> Result<Vec<StoredSession>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT id, project, worktree, kind, title, cwd, agent_session, last_prompt, created_at FROM sessions ORDER BY created_at",
        )?;
        let rows = st.query_map([], |r| {
            let kind: String = r.get(3)?;
            Ok(StoredSession {
                id: r.get(0)?,
                project: r.get(1)?,
                worktree: PathBuf::from(r.get::<_, String>(2)?),
                kind: serde_json::from_str(&kind).unwrap_or(SessionKind::Shell),
                title: r.get(4)?,
                cwd: PathBuf::from(r.get::<_, String>(5)?),
                agent_session: r.get(6)?,
                last_prompt: r.get(7)?,
                created_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn get_kv(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        Ok(
            c.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0))
                .optional()?,
        )
    }

    pub fn set_kv(&self, key: &str, value: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2",
            [key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_slots_sessions() {
        let s = Store::in_memory().unwrap();
        let id = s.add_project(Path::new("/r")).unwrap();
        assert_eq!(s.add_project(Path::new("/r")).unwrap(), id);
        s.set_slot(&id, Path::new("/w1"), 1).unwrap();
        s.set_slot(&id, Path::new("/w1"), 2).unwrap();
        assert_eq!(s.slots(&id).unwrap(), vec![(PathBuf::from("/w1"), 2)]);
        let sess = StoredSession {
            id: "a".into(),
            project: id.clone(),
            worktree: "/w1".into(),
            kind: SessionKind::Run {
                target: "web".into(),
                proc_name: "api".into(),
            },
            title: "t".into(),
            cwd: "/w1".into(),
            agent_session: Some("x".into()),
            last_prompt: None,
            created_at: 1,
        };
        s.save_session(&sess).unwrap();
        let back = s.sessions().unwrap();
        assert_eq!(back[0].kind, sess.kind);
        s.remove_project(&id).unwrap();
        assert!(s.sessions().unwrap().is_empty());
        s.set_kv("ui", "{}").unwrap();
        assert_eq!(s.get_kv("ui").unwrap().as_deref(), Some("{}"));
    }
}
