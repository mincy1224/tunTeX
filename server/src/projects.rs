use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

pub struct Store {
    db: Connection,
    root: PathBuf,
}

impl Store {
    pub fn open(root: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
        }
        let db = Connection::open(root.join("projects.sqlite3")).map_err(|e| e.to_string())?;
        db.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(|e| e.to_string())?;
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY, key TEXT UNIQUE NOT NULL, created INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS files(project TEXT NOT NULL, path TEXT NOT NULL, hash TEXT NOT NULL, PRIMARY KEY(project,path));").map_err(|e| e.to_string())?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS heads(project TEXT PRIMARY KEY, generation TEXT NOT NULL);",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            db,
            root: root.to_path_buf(),
        })
    }

    pub fn register(&self) -> Result<String, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let key = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        self.db
            .execute(
                "INSERT INTO projects VALUES (?1,?2,unixepoch())",
                params![id, key],
            )
            .map_err(|e| e.to_string())?;
        Ok(key)
    }

    pub fn list(&self) -> Result<(), String> {
        let mut query = self
            .db
            .prepare("SELECT id,key,created FROM projects ORDER BY created,rowid")
            .map_err(|e| e.to_string())?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (id, key, created) = row.map_err(|e| e.to_string())?;
            println!("{id}\t{created}\t{key}");
        }
        Ok(())
    }

    pub fn directory(&self, key: &str) -> Result<PathBuf, String> {
        let id: String = self
            .db
            .query_row("SELECT id FROM projects WHERE key=?1", [key], |row| {
                row.get(0)
            })
            .map_err(|_| "unknown project key".to_string())?;
        Ok(self.root.join(id))
    }

    pub fn delete(&mut self, key: &str) -> Result<(), String> {
        let directory = self.directory(key)?;
        let id = directory
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM files WHERE project=?1", [&id])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM heads WHERE project=?1", [&id])
            .map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM projects WHERE id=?1", [&id])
            .map_err(|e| e.to_string())?;
        transaction.commit().map_err(|e| e.to_string())?;
        if directory.exists() {
            std::fs::remove_dir_all(directory).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn workspace(&self, directory: &Path) -> Result<PathBuf, String> {
        use rusqlite::OptionalExtension;
        let id = directory
            .file_name()
            .ok_or("invalid project directory")?
            .to_string_lossy();
        let generation: Option<String> = self
            .db
            .query_row(
                "SELECT generation FROM heads WHERE project=?1",
                [id.as_ref()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(directory
            .join("generations")
            .join(generation.unwrap_or_else(|| "empty".into())))
    }

    pub fn sources(&self, directory: &Path) -> Result<Vec<String>, String> {
        let id = directory
            .file_name()
            .ok_or("invalid project directory")?
            .to_string_lossy();
        let mut query = self
            .db
            .prepare("SELECT path FROM files WHERE project=?1")
            .map_err(|e| e.to_string())?;
        let rows = query
            .query_map([id.as_ref()], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    pub fn save_sources(
        &mut self,
        directory: &Path,
        manifest: &std::collections::BTreeMap<String, String>,
        generation: &str,
    ) -> Result<(), String> {
        let id = directory
            .file_name()
            .ok_or("invalid project directory")?
            .to_string_lossy();
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE id=?1)",
                [id.as_ref()],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !exists {
            return Err("project key was revoked".into());
        }
        transaction.execute("INSERT INTO heads VALUES (?1,?2) ON CONFLICT(project) DO UPDATE SET generation=excluded.generation", params![id.as_ref(),generation]).map_err(|e| e.to_string())?;
        transaction
            .execute("DELETE FROM files WHERE project=?1", [id.as_ref()])
            .map_err(|e| e.to_string())?;
        for (path, hash) in manifest {
            transaction
                .execute(
                    "INSERT INTO files VALUES (?1,?2,?3)",
                    params![id.as_ref(), path, hash],
                )
                .map_err(|e| e.to_string())?;
        }
        transaction.commit().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_are_isolated_and_revoked_keys_cannot_publish() {
        let root = tempfile::tempdir().unwrap();
        let mut store = Store::open(root.path()).unwrap();
        let key = store.register().unwrap();
        let other = store.register().unwrap();
        let directory = store.directory(&key).unwrap();
        assert_ne!(directory, store.directory(&other).unwrap());
        let sources = std::collections::BTreeMap::from([("main.tex".into(), "hash".into())]);
        store.save_sources(&directory, &sources, "first").unwrap();
        assert_eq!(
            store.workspace(&directory).unwrap(),
            directory.join("generations/first")
        );
        assert_eq!(store.sources(&directory).unwrap(), vec!["main.tex"]);
        drop(store);
        let mut store = Store::open(root.path()).unwrap();
        assert_eq!(store.directory(&key).unwrap(), directory);
        store.delete(&key).unwrap();
        assert!(store.directory(&key).is_err());
        assert!(store.save_sources(&directory, &sources, "second").is_err());
        assert!(store.directory(&other).is_ok());
    }
}
