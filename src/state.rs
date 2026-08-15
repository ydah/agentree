use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use rusqlite::{params, Connection, OptionalExtension};

use crate::domain::{AppError, Id, Lifecycle, OperationKind, OperationStatus};

#[derive(Debug, Clone)]
pub struct TaskRecord {
    pub id: String,
    pub slug: String,
    pub branch: String,
    pub path: PathBuf,
    pub base_oid: String,
    pub head_oid: String,
    pub lifecycle: Lifecycle,
    pub config_hash: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct OperationRecord {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub task_id: Option<String>,
    pub expected: String,
    pub observed: String,
}

pub struct State {
    pub path: PathBuf,
    connection: Mutex<Connection>,
}

impl State {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection: Mutex::new(connection),
        })
    }

    pub fn create_operation(
        &self,
        kind: OperationKind,
        task_id: Option<&str>,
        expected: &str,
    ) -> Result<OperationRecord, AppError> {
        let id = Id::new("op-");
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("INSERT INTO operations(id, kind, status, task_id, expected_json, observed_json) VALUES(?1, ?2, ?3, ?4, ?5, '{}')", params![id.0, kind.as_str(), OperationStatus::Prepared.as_str(), task_id, expected])?;
        Ok(OperationRecord {
            id: id.0,
            kind: kind.as_str().to_owned(),
            status: OperationStatus::Prepared.as_str().to_owned(),
            task_id: task_id.map(str::to_owned),
            expected: expected.to_owned(),
            observed: "{}".to_owned(),
        })
    }

    pub fn update_operation(
        &self,
        id: &str,
        status: OperationStatus,
        observed: &str,
    ) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let changed = connection.execute(
            "UPDATE operations SET status=?2, observed_json=?3, updated_at=strftime('%s','now') WHERE id=?1 AND status NOT IN ('completed','failed')",
            params![id, status.as_str(), observed],
        )?;
        if changed != 1 {
            return Err(AppError::diagnostic(
                "AGT-0305",
                "operation is missing or already terminal",
                crate::domain::ErrorKind::RecoveryRequired,
            ));
        }
        Ok(())
    }

    pub fn incomplete_operations(&self) -> Result<Vec<OperationRecord>, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let mut statement = connection.prepare("SELECT id, kind, status, task_id, expected_json, observed_json FROM operations WHERE status NOT IN ('completed', 'failed') ORDER BY created_at")?;
        let rows = statement.query_map([], |row| {
            Ok(OperationRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                status: row.get(2)?,
                task_id: row.get(3)?,
                expected: row.get(4)?,
                observed: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn has_incomplete_operation_for_task(&self, task_id: &str) -> Result<bool, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM operations WHERE task_id=?1 AND status NOT IN ('completed','failed')",
            params![task_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn operation(&self, id: &str) -> Result<OperationRecord, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.query_row("SELECT id, kind, status, task_id, expected_json, observed_json FROM operations WHERE id=?1", params![id], |row| Ok(OperationRecord { id: row.get(0)?, kind: row.get(1)?, status: row.get(2)?, task_id: row.get(3)?, expected: row.get(4)?, observed: row.get(5)? })).map_err(|error| match error { rusqlite::Error::QueryReturnedNoRows => AppError::diagnostic("AGT-0304", "operation not found", crate::domain::ErrorKind::Usage), other => AppError::Sqlite(other) })
    }

    pub fn insert_task(&self, task: &TaskRecord) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("INSERT INTO tasks(id, slug, branch_ref, worktree_path, base_oid, head_oid, lifecycle, config_hash, scopes_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![task.id, task.slug, task.branch, path_text(&task.path)?, task.base_oid, task.head_oid, task.lifecycle.as_str(), task.config_hash, serde_json::to_string(&task.scopes)?])?;
        Ok(())
    }

    pub fn register_repository(
        &self,
        id: &str,
        common_dir: &Path,
        state_dir: &Path,
        object_format: &str,
    ) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("INSERT INTO repositories(id, common_dir, state_dir, object_format) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET common_dir=excluded.common_dir, state_dir=excluded.state_dir, object_format=excluded.object_format", params![id, path_text(common_dir)?, path_text(state_dir)?, object_format])?;
        Ok(())
    }

    pub fn save_config(
        &self,
        task_id: &str,
        snapshot: &crate::config::ConfigSnapshot,
    ) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("INSERT OR REPLACE INTO configs(task_id, hash, source_oid, snapshot_json) VALUES(?1,?2,?3,?4)", params![task_id, snapshot.hash, snapshot.source_oid, serde_json::to_string(snapshot)?])?;
        Ok(())
    }

    pub fn config(&self, task_id: &str) -> Result<crate::config::ConfigSnapshot, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let json: String = connection.query_row(
            "SELECT snapshot_json FROM configs WHERE task_id=?1",
            params![task_id],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_str(&json)?)
    }

    pub fn update_task_lifecycle(
        &self,
        id: &str,
        lifecycle: Lifecycle,
        head_oid: Option<&str>,
        base_oid: Option<&str>,
    ) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("UPDATE tasks SET lifecycle=?2, head_oid=COALESCE(?3, head_oid), base_oid=COALESCE(?4, base_oid), updated_at=strftime('%s','now') WHERE id=?1", params![id, lifecycle.as_str(), head_oid, base_oid])?;
        Ok(())
    }

    pub fn task(&self, selector: &str) -> Result<TaskRecord, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let mut statement = connection.prepare("SELECT id, slug, branch_ref, worktree_path, base_oid, head_oid, lifecycle, config_hash, scopes_json FROM tasks WHERE id=?1 OR slug=?1 ORDER BY created_at LIMIT 1")?;
        let task = statement
            .query_row(params![selector], |row| {
                let scopes: String = row.get(8)?;
                Ok(TaskRecord {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    branch: row.get(2)?,
                    path: PathBuf::from(row.get::<_, String>(3)?),
                    base_oid: row.get(4)?,
                    head_oid: row.get(5)?,
                    lifecycle: Lifecycle::parse(&row.get::<_, String>(6)?)
                        .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    config_hash: row.get(7)?,
                    scopes: serde_json::from_str(&scopes).unwrap_or_default(),
                })
            })
            .optional()?;
        task.ok_or_else(|| {
            AppError::diagnostic(
                "AGT-0302",
                format!("task not found: {selector}"),
                crate::domain::ErrorKind::Usage,
            )
        })
    }

    pub fn tasks(&self) -> Result<Vec<TaskRecord>, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let mut statement = connection.prepare("SELECT id, slug, branch_ref, worktree_path, base_oid, head_oid, lifecycle, config_hash, scopes_json FROM tasks ORDER BY created_at")?;
        let rows = statement.query_map([], |row| {
            let scopes: String = row.get(8)?;
            Ok(TaskRecord {
                id: row.get(0)?,
                slug: row.get(1)?,
                branch: row.get(2)?,
                path: PathBuf::from(row.get::<_, String>(3)?),
                base_oid: row.get(4)?,
                head_oid: row.get(5)?,
                lifecycle: Lifecycle::parse(&row.get::<_, String>(6)?)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?,
                config_hash: row.get(7)?,
                scopes: serde_json::from_str(&scopes).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn sessions_for_task(&self, task_id: &str) -> Result<usize, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM sessions WHERE task_id=?1 AND status IN ('starting','running')",
            params![task_id],
            |row| row.get(0),
        )?)
    }

    pub fn active_session_for_process_group(
        &self,
        pgid: u32,
    ) -> Result<Option<(String, String)>, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection
            .query_row(
                "SELECT id, task_id FROM sessions WHERE pgid=?1 AND status IN ('starting','running') ORDER BY started_at LIMIT 1",
                params![pgid],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(AppError::from)
    }

    pub fn start_session(&self, task_id: &str, pid: u32, pgid: u32) -> Result<String, AppError> {
        let id = Id::new("session-");
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute(
            "INSERT INTO sessions(id, task_id, status, pid, pgid) VALUES(?1,?2,'running',?3,?4)",
            params![id.0, task_id, pid, pgid],
        )?;
        Ok(id.0)
    }

    pub fn finish_session(
        &self,
        id: &str,
        status: &str,
        exit_code: Option<i32>,
    ) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("UPDATE sessions SET status=?2, exit_code=?3, finished_at=strftime('%s','now') WHERE id=?1", params![id, status, exit_code])?;
        Ok(())
    }

    pub fn update_session_identity(
        &self,
        id: &str,
        pid: u32,
        pgid: u32,
        birth_id: &str,
    ) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute(
            "UPDATE sessions SET pid=?2, pgid=?3, birth_id=?4 WHERE id=?1",
            params![id, pid, pgid, birth_id],
        )?;
        Ok(())
    }

    pub fn save_checkpoint(&self, record: &CheckpointRecord) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("INSERT INTO checkpoints(id, task_id, head_oid, index_tree_oid, worktree_tree_oid, metadata_oid, message, config_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![record.id, record.task_id, record.head_oid, record.index_tree_oid, record.worktree_tree_oid, record.metadata_oid, record.message, record.config_hash])?;
        Ok(())
    }

    pub fn save_check_run(&self, run: &CheckRunRecord) -> Result<(), AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        connection.execute("INSERT INTO check_runs(id, task_id, head_oid, config_hash, definition_hash, status, command_json) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![run.id, run.task_id, run.head_oid, run.config_hash, run.definition_hash, run.status, run.command_json])?;
        Ok(())
    }

    pub fn fresh_required_checks(
        &self,
        task_id: &str,
        head_oid: &str,
        config_hash: &str,
        definition_hash: &str,
        required_count: usize,
    ) -> Result<bool, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let count: i64 = connection.query_row("SELECT COUNT(*) FROM check_runs WHERE task_id=?1 AND head_oid=?2 AND config_hash=?3 AND definition_hash=?4 AND status='passed'", params![task_id, head_oid, config_hash, definition_hash], |row| row.get(0))?;
        Ok(count >= required_count as i64)
    }

    pub fn checkpoints(&self, task_id: &str) -> Result<Vec<CheckpointRecord>, AppError> {
        let connection = self.connection.lock().map_err(|_| {
            AppError::diagnostic(
                "AGT-0301",
                "state lock poisoned",
                crate::domain::ErrorKind::Database,
            )
        })?;
        let mut statement = connection.prepare("SELECT id, task_id, head_oid, index_tree_oid, worktree_tree_oid, metadata_oid, message, config_hash FROM checkpoints WHERE task_id=?1 ORDER BY created_at")?;
        let rows = statement.query_map(params![task_id], |row| {
            Ok(CheckpointRecord {
                id: row.get(0)?,
                task_id: row.get(1)?,
                head_oid: row.get(2)?,
                index_tree_oid: row.get(3)?,
                worktree_tree_oid: row.get(4)?,
                metadata_oid: row.get(5)?,
                message: row.get(6)?,
                config_hash: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckpointRecord {
    pub id: String,
    pub task_id: String,
    pub head_oid: String,
    pub index_tree_oid: String,
    pub worktree_tree_oid: String,
    pub metadata_oid: String,
    pub message: Option<String>,
    pub config_hash: String,
}

#[derive(Debug, Clone)]
pub struct CheckRunRecord {
    pub id: String,
    pub task_id: String,
    pub head_oid: String,
    pub config_hash: String,
    pub definition_hash: String,
    pub status: String,
    pub command_json: String,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repositories(id TEXT PRIMARY KEY, common_dir TEXT NOT NULL, state_dir TEXT NOT NULL, object_format TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, slug TEXT NOT NULL, branch_ref TEXT NOT NULL UNIQUE, worktree_path TEXT NOT NULL UNIQUE, base_oid TEXT NOT NULL, head_oid TEXT NOT NULL, lifecycle TEXT NOT NULL, config_hash TEXT NOT NULL, scopes_json TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')), updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
CREATE TABLE IF NOT EXISTS configs(task_id TEXT PRIMARY KEY REFERENCES tasks(id), hash TEXT NOT NULL, source_oid TEXT NOT NULL, snapshot_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), status TEXT NOT NULL, pid INTEGER, pgid INTEGER, birth_id TEXT, exit_code INTEGER, started_at INTEGER NOT NULL DEFAULT (strftime('%s','now')), finished_at INTEGER);
CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY, kind TEXT NOT NULL, status TEXT NOT NULL, task_id TEXT REFERENCES tasks(id), expected_json TEXT NOT NULL, observed_json TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')), updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
CREATE TABLE IF NOT EXISTS checkpoints(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), head_oid TEXT NOT NULL, index_tree_oid TEXT NOT NULL, worktree_tree_oid TEXT NOT NULL, metadata_oid TEXT NOT NULL, message TEXT, config_hash TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
CREATE TABLE IF NOT EXISTS check_runs(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id), head_oid TEXT NOT NULL, config_hash TEXT NOT NULL, definition_hash TEXT NOT NULL, status TEXT NOT NULL, command_json TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
CREATE TABLE IF NOT EXISTS events(id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT, kind TEXT NOT NULL, payload_json TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (strftime('%s','now')));
"#;

fn path_text(path: &Path) -> Result<String, AppError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        AppError::diagnostic(
            "AGT-0303",
            "non-UTF-8 filesystem paths are unsupported",
            crate::domain::ErrorKind::Unsupported,
        )
    })
}
