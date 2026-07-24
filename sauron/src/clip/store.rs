//! SQLite-compatible storage for the Agent Clipboard.
//!
//! The schema and value semantics intentionally match `alvinlu7/forge` so the
//! Python and Rust CLIs can safely share one database.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension, Row};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub type ClipResult<T> = Result<T, String>;

const VALID_KINDS: &[&str] = &["fact", "checkpoint", "rollback", "artifact-ref"];
const SCHEMA_VERSION: i64 = 1;
const ITEM_COLUMNS: &[(&str, &str)] = &[
    ("namespace", "TEXT NOT NULL DEFAULT 'default'"),
    ("kind", "TEXT NOT NULL DEFAULT 'fact'"),
    ("tags", "TEXT NOT NULL DEFAULT '[]'"),
    ("source", "TEXT"),
    ("created_at", "TEXT NOT NULL DEFAULT ''"),
    ("updated_at", "TEXT NOT NULL DEFAULT ''"),
    ("pinned", "INTEGER NOT NULL DEFAULT 0"),
    ("validated", "INTEGER NOT NULL DEFAULT 0"),
    ("supersedes_key", "TEXT"),
    ("expires_at", "TEXT"),
    ("checksum", "TEXT NOT NULL DEFAULT ''"),
    ("metadata", "TEXT NOT NULL DEFAULT '{}'"),
    ("version", "INTEGER NOT NULL DEFAULT 1"),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: i64,
    pub key: String,
    pub value: String,
    pub namespace: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub pinned: bool,
    pub validated: bool,
    pub supersedes_key: Option<String>,
    pub expires_at: Option<String>,
    pub checksum: String,
    pub metadata: Value,
    pub version: i64,
}

impl Item {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "key": self.key,
            "namespace": self.namespace,
            "kind": self.kind,
            "tags": self.tags,
            "source": self.source,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "pinned": self.pinned,
            "validated": self.validated,
            "supersedes_key": self.supersedes_key,
            "expires_at": self.expires_at,
            "checksum": self.checksum,
            "metadata": self.metadata,
            "version": self.version,
            "value": self.value,
        })
    }
}

#[derive(Clone, Debug)]
pub struct PutOptions {
    pub namespace: String,
    pub kind: String,
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub ttl: Option<String>,
    pub metadata: Value,
    pub validated: Option<bool>,
    pub supersedes_key: Option<String>,
    pub pinned: bool,
    pub force_pin: bool,
}

impl Default for PutOptions {
    fn default() -> Self {
        Self {
            namespace: "default".into(),
            kind: "fact".into(),
            tags: Vec::new(),
            source: None,
            ttl: None,
            metadata: json!({}),
            validated: None,
            supersedes_key: None,
            pinned: false,
            force_pin: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ListOptions {
    pub namespace: Option<String>,
    pub kind: Option<String>,
    pub tags: Vec<String>,
    pub pinned: Option<bool>,
    pub validated: Option<bool>,
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
    #[allow(dead_code)] // the standalone CLI does not need to expose its path
    path: PathBuf,
}

impl Store {
    pub fn open(path: Option<&Path>) -> ClipResult<Self> {
        let path = resolve_db_path(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let conn = Connection::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(err)?;
        conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            ",
        )
        .map_err(err)?;
        ensure_schema(&conn)
            .map_err(|e| format!("initialize clipboard database {}: {e}", path.display()))?;
        Ok(Self { conn, path })
    }

    pub fn open_read_only(path: Option<&Path>) -> ClipResult<Self> {
        let path = resolve_db_path(path);
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open clipboard database {} read-only: {e}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))
            .map_err(err)?;
        conn.pragma_update(None, "query_only", true).map_err(err)?;
        ensure_read_schema(&conn, &path)?;
        Ok(Self { conn, path })
    }

    pub fn open_for_read(path: Option<&Path>) -> ClipResult<Self> {
        let path = resolve_db_path(path);
        if path.exists() {
            Self::open_read_only(Some(&path))
        } else {
            // Preserve the original CLI behavior for a brand-new store:
            // the first read command initializes an empty database.
            Self::open(Some(&path))
        }
    }

    #[allow(dead_code)] // consumed by Sauron's handoff wrapper
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn put(&mut self, key: &str, value: &str, mut options: PutOptions) -> ClipResult<Item> {
        let key = key.trim();
        if key.is_empty() {
            return Err("key is required".into());
        }
        options.namespace = options.namespace.trim().to_string();
        if options.namespace.is_empty() {
            options.namespace = "default".into();
        }
        options.kind = normalize_kind(&options.kind)?;
        options.tags = parse_tags(options.tags);
        if !options.metadata.is_object() {
            return Err("metadata must be a JSON object".into());
        }

        let existing = self.get_including_expired(key)?;
        let validated = options
            .validated
            .unwrap_or_else(|| existing.as_ref().is_some_and(|i| i.validated));
        let pinned = options.pinned || existing.as_ref().is_some_and(|i| i.pinned);
        if pinned && !validated && !options.force_pin {
            return Err("pinning requires validated=true or force_pin=true".into());
        }

        let now = sqlite_now(&self.conn)?;
        let created_at = existing
            .as_ref()
            .map(|i| i.created_at.clone())
            .unwrap_or_else(|| now.clone());
        let version = existing.as_ref().map_or(1, |i| i.version + 1);
        let expires_at = match options.ttl.as_deref() {
            Some(ttl) => Some(sqlite_expiry(&self.conn, parse_ttl_seconds(ttl)?)?),
            None => None,
        };
        let tags = serde_json::to_string(&options.tags).map_err(err)?;
        let metadata = canonical_json(&options.metadata)?;
        let checksum = checksum(value);

        self.conn
            .execute(
                "
                INSERT INTO items (
                    key, value, namespace, kind, tags, source, created_at, updated_at,
                    pinned, validated, supersedes_key, expires_at, checksum, metadata, version
                )
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ON CONFLICT(key) DO UPDATE SET
                    value=excluded.value,
                    namespace=excluded.namespace,
                    kind=excluded.kind,
                    tags=excluded.tags,
                    source=excluded.source,
                    updated_at=excluded.updated_at,
                    pinned=excluded.pinned,
                    validated=excluded.validated,
                    supersedes_key=excluded.supersedes_key,
                    expires_at=excluded.expires_at,
                    checksum=excluded.checksum,
                    metadata=excluded.metadata,
                    version=excluded.version
                ",
                params![
                    key,
                    value,
                    options.namespace,
                    options.kind,
                    tags,
                    options.source,
                    created_at,
                    now,
                    i64::from(pinned),
                    i64::from(validated),
                    options.supersedes_key,
                    expires_at,
                    checksum,
                    metadata,
                    version,
                ],
            )
            .map_err(err)?;
        let item = self
            .get_including_expired(key)?
            .ok_or_else(|| "failed to persist item".to_string())?;
        self.sync_fts(&item)?;
        Ok(item)
    }

    pub fn update(
        &mut self,
        key: &str,
        value: &str,
        patch: UpdateOptions,
    ) -> ClipResult<Option<Item>> {
        let Some(current) = self.get_including_expired(key)? else {
            return Ok(None);
        };
        let options = PutOptions {
            namespace: patch.namespace.unwrap_or(current.namespace),
            kind: patch.kind.unwrap_or(current.kind),
            tags: patch.tags.unwrap_or(current.tags),
            source: patch.source.unwrap_or(current.source),
            ttl: patch.ttl,
            metadata: patch.metadata.unwrap_or(current.metadata),
            validated: patch.validated.or(Some(current.validated)),
            supersedes_key: patch.supersedes_key.unwrap_or(current.supersedes_key),
            pinned: patch.pinned.unwrap_or(current.pinned),
            force_pin: patch.force_pin,
        };
        self.put(key, value, options).map(Some)
    }

    pub fn get(&self, key: &str) -> ClipResult<Option<Item>> {
        self.row_by_key(key, false)
    }

    pub fn get_including_expired(&self, key: &str) -> ClipResult<Option<Item>> {
        self.row_by_key(key, true)
    }

    pub fn list(&self, limit: usize, options: &ListOptions) -> ClipResult<Vec<Item>> {
        if let Some(kind) = options.kind.as_deref() {
            normalize_kind(kind)?;
        }
        let mut sql =
            String::from("SELECT * FROM items WHERE (expires_at IS NULL OR expires_at > ?)");
        let now = sqlite_now(&self.conn)?;
        let mut values = vec![rusqlite::types::Value::Text(now)];
        if let Some(namespace) = &options.namespace {
            sql.push_str(" AND namespace = ?");
            values.push(namespace.clone().into());
        }
        if let Some(kind) = &options.kind {
            sql.push_str(" AND kind = ?");
            values.push(kind.clone().into());
        }
        if let Some(pinned) = options.pinned {
            sql.push_str(" AND pinned = ?");
            values.push(i64::from(pinned).into());
        }
        if let Some(validated) = options.validated {
            sql.push_str(" AND validated = ?");
            values.push(i64::from(validated).into());
        }
        sql.push_str(" ORDER BY pinned DESC, validated DESC, updated_at DESC LIMIT ?");
        values.push(((limit.max(1) * 5) as i64).into());

        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        let rows = stmt
            .query_map(params_from_iter(values), item_from_row)
            .map_err(err)?;
        let wanted: BTreeSet<&str> = options.tags.iter().map(String::as_str).collect();
        let mut items = Vec::new();
        for row in rows {
            let item = row.map_err(err)?;
            let have: BTreeSet<&str> = item.tags.iter().map(String::as_str).collect();
            if wanted.is_subset(&have) {
                items.push(item);
            }
            if items.len() >= limit {
                break;
            }
        }
        Ok(items)
    }

    pub fn search(
        &self,
        query: Option<&str>,
        namespace: Option<&str>,
        kind: Option<&str>,
        tags: &[String],
        limit: usize,
    ) -> ClipResult<Vec<Item>> {
        let query = query.unwrap_or_default().trim();
        if let Some(kind) = kind {
            normalize_kind(kind)?;
        }
        if query.is_empty() {
            return self.list(
                limit,
                &ListOptions {
                    namespace: namespace.map(str::to_string),
                    kind: kind.map(str::to_string),
                    tags: tags.to_vec(),
                    ..ListOptions::default()
                },
            );
        }

        let candidate_limit = (limit.max(10) * 10).max(100);
        let mut ids = BTreeSet::new();
        if self.has_fts() {
            let match_query = fts_query(query);
            if !match_query.is_empty() {
                if let Ok(mut stmt) = self
                    .conn
                    .prepare("SELECT rowid FROM items_fts WHERE items_fts MATCH ? LIMIT ?")
                {
                    if let Ok(rows) = stmt.query_map(params![match_query, candidate_limit], |row| {
                        row.get::<_, i64>(0)
                    }) {
                        for id in rows.flatten() {
                            ids.insert(id);
                        }
                    }
                }
            }
        }

        let now = sqlite_now(&self.conn)?;
        let like = format!("%{query}%");
        let mut sql = String::from(
            "
            SELECT id FROM items
            WHERE (key LIKE ? OR value LIKE ? OR namespace LIKE ? OR tags LIKE ?)
              AND (expires_at IS NULL OR expires_at > ?)
            ",
        );
        let mut values: Vec<rusqlite::types::Value> = vec![
            like.clone().into(),
            like.clone().into(),
            like.clone().into(),
            like.into(),
            now.into(),
        ];
        if let Some(namespace) = namespace {
            sql.push_str(" AND namespace = ?");
            values.push(namespace.to_string().into());
        }
        if let Some(kind) = kind {
            sql.push_str(" AND kind = ?");
            values.push(kind.to_string().into());
        }
        sql.push_str(" ORDER BY pinned DESC, validated DESC, updated_at DESC LIMIT ?");
        values.push((candidate_limit as i64).into());
        let mut stmt = self.conn.prepare(&sql).map_err(err)?;
        for id in stmt
            .query_map(params_from_iter(values), |row| row.get::<_, i64>(0))
            .map_err(err)?
            .flatten()
        {
            ids.insert(id);
        }
        if let Some(item) = self.get(query)? {
            ids.insert(item.id);
        }

        // If token/substring retrieval found nothing, score a bounded recent
        // set. This is the intended fuzzy fallback and remains deterministic.
        let mut items = if ids.is_empty() {
            self.list(
                candidate_limit,
                &ListOptions {
                    namespace: namespace.map(str::to_string),
                    kind: kind.map(str::to_string),
                    ..ListOptions::default()
                },
            )?
        } else {
            let placeholders = std::iter::repeat_n("?", ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT * FROM items WHERE id IN ({placeholders})");
            let mut stmt = self.conn.prepare(&sql).map_err(err)?;
            let rows = stmt
                .query_map(params_from_iter(ids.iter()), item_from_row)
                .map_err(err)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(err)?
        };

        let wanted: BTreeSet<&str> = tags.iter().map(String::as_str).collect();
        items.retain(|item| {
            namespace.is_none_or(|n| item.namespace == n)
                && kind.is_none_or(|k| item.kind == k)
                && wanted.is_subset(&item.tags.iter().map(String::as_str).collect())
        });
        items.sort_by(|a, b| {
            score_item(b, query, namespace, tags)
                .total_cmp(&score_item(a, query, namespace, tags))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        items.truncate(limit);
        Ok(items)
    }

    pub fn pin(&mut self, key: &str, pinned: bool, force: bool) -> ClipResult<Option<Item>> {
        let Some(item) = self.get_including_expired(key)? else {
            return Ok(None);
        };
        if pinned && !item.validated && !force {
            return Err("pinning requires validated=true or force=true".into());
        }
        let now = sqlite_now(&self.conn)?;
        self.conn
            .execute(
                "UPDATE items SET pinned = ?, updated_at = ? WHERE key = ?",
                params![i64::from(pinned), now, key],
            )
            .map_err(err)?;
        let item = self
            .get_including_expired(key)?
            .ok_or_else(|| "failed to update item".to_string())?;
        self.sync_fts(&item)?;
        Ok(Some(item))
    }

    pub fn delete(&mut self, key: &str) -> ClipResult<bool> {
        let Some(item) = self.get_including_expired(key)? else {
            return Ok(false);
        };
        self.conn
            .execute("DELETE FROM items WHERE key = ?", params![key])
            .map_err(err)?;
        if self.has_fts() {
            self.conn
                .execute("DELETE FROM items_fts WHERE rowid = ?", params![item.id])
                .map_err(err)?;
        }
        Ok(true)
    }

    pub fn gc(&mut self) -> ClipResult<usize> {
        let now = sqlite_now(&self.conn)?;
        let ids = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM items WHERE expires_at IS NOT NULL AND expires_at <= ?")
                .map_err(err)?;
            let rows = stmt
                .query_map(params![now.clone()], |row| row.get::<_, i64>(0))
                .map_err(err)?
                .flatten()
                .collect::<Vec<_>>();
            rows
        };
        self.conn
            .execute(
                "DELETE FROM items WHERE expires_at IS NOT NULL AND expires_at <= ?",
                params![now],
            )
            .map_err(err)?;
        if self.has_fts() {
            for id in &ids {
                self.conn
                    .execute("DELETE FROM items_fts WHERE rowid = ?", params![id])
                    .map_err(err)?;
            }
        }
        Ok(ids.len())
    }

    pub fn stats(&self) -> ClipResult<Value> {
        let now = sqlite_now(&self.conn)?;
        let count = |where_sql: &str| -> ClipResult<i64> {
            self.conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM items WHERE {where_sql} AND (expires_at IS NULL OR expires_at > ?)"
                    ),
                    params![now.clone()],
                    |row| row.get(0),
                )
                .map_err(err)
        };
        let groups = |column: &str| -> ClipResult<Map<String, Value>> {
            let sql = format!(
                "SELECT {column}, COUNT(*) FROM items
                 WHERE expires_at IS NULL OR expires_at > ?
                 GROUP BY {column} ORDER BY COUNT(*) DESC, {column} ASC"
            );
            let mut stmt = self.conn.prepare(&sql).map_err(err)?;
            let mut map = Map::new();
            for row in stmt
                .query_map(params![now.clone()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(err)?
            {
                let (name, value) = row.map_err(err)?;
                map.insert(name, json!(value));
            }
            Ok(map)
        };
        let mut tag_counts: BTreeMap<String, i64> = BTreeMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT tags FROM items WHERE expires_at IS NULL OR expires_at > ?")
            .map_err(err)?;
        for raw in stmt
            .query_map(params![now.clone()], |row| row.get::<_, String>(0))
            .map_err(err)?
            .flatten()
        {
            for tag in tags_from_json(&raw) {
                *tag_counts.entry(tag).or_default() += 1;
            }
        }
        let recency: (Option<String>, Option<String>) = self
            .conn
            .query_row(
                "
                SELECT MIN(created_at), MAX(updated_at) FROM items
                WHERE expires_at IS NULL OR expires_at > ?
                ",
                params![now],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(err)?;
        Ok(json!({
            "items": count("1 = 1")?,
            "pinned": count("pinned = 1")?,
            "validated": count("validated = 1")?,
            "namespaces": groups("namespace")?,
            "kinds": groups("kind")?,
            "tags": tag_counts,
            "oldest_created_at": recency.0,
            "newest_updated_at": recency.1,
            "fts_enabled": self.has_fts(),
        }))
    }

    pub fn doctor(&self) -> ClipResult<Value> {
        let integrity: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(err)?;
        let schema_version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(err)?;
        Ok(json!({
            "ok": integrity == "ok",
            "database": self.path.display().to_string(),
            "schema_version": schema_version,
            "integrity_check": integrity,
            "stats": self.stats()?,
        }))
    }

    pub fn export(
        &self,
        namespace: Option<String>,
        kind: Option<String>,
        tags: Vec<String>,
        format: &str,
        limit: usize,
    ) -> ClipResult<String> {
        let items = self.list(
            limit,
            &ListOptions {
                namespace,
                kind,
                tags,
                ..ListOptions::default()
            },
        )?;
        match format {
            "json" => {
                serde_json::to_string_pretty(&items.iter().map(Item::to_json).collect::<Vec<_>>())
                    .map(|s| format!("{s}\n"))
                    .map_err(err)
            }
            "md" => {
                let mut out = String::from("# Agent Clipboard Export\n\n");
                for item in items {
                    out.push_str(&format!(
                        "## {}\n\n- namespace: `{}`\n- kind: `{}`\n- tags: `{}`\n- pinned: `{}`\n- validated: `{}`\n- updated_at: `{}`\n\n```text\n{}\n```\n\n",
                        item.key,
                        item.namespace,
                        item.kind,
                        item.tags.join(","),
                        item.pinned,
                        item.validated,
                        item.updated_at,
                        item.value
                    ));
                }
                Ok(out)
            }
            _ => Err("format must be json or md".into()),
        }
    }

    pub fn import(&mut self, payload: &str, overwrite: bool) -> ClipResult<usize> {
        let records = serde_json::from_str::<Value>(payload).map_err(err)?;
        let records = records
            .as_array()
            .ok_or_else(|| "import JSON must be a list of items".to_string())?;
        let mut count = 0;
        for record in records {
            let key = record
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| "import item is missing key".to_string())?;
            let value = record
                .get("value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("import item {key} is missing value"))?;
            if !overwrite && self.get_including_expired(key)?.is_some() {
                continue;
            }
            let pinned = record
                .get("pinned")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            self.put(
                key,
                value,
                PutOptions {
                    namespace: string_field(record, "namespace")
                        .unwrap_or_else(|| "default".into()),
                    kind: string_field(record, "kind").unwrap_or_else(|| "fact".into()),
                    tags: record
                        .get("tags")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                    source: string_field(record, "source"),
                    metadata: record.get("metadata").cloned().unwrap_or_else(|| json!({})),
                    validated: Some(
                        record
                            .get("validated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    ),
                    supersedes_key: string_field(record, "supersedes_key"),
                    pinned,
                    force_pin: pinned,
                    ..PutOptions::default()
                },
            )?;
            count += 1;
        }
        Ok(count)
    }

    fn row_by_key(&self, key: &str, include_expired: bool) -> ClipResult<Option<Item>> {
        let (sql, now) = if include_expired {
            ("SELECT * FROM items WHERE key = ?", None)
        } else {
            (
                "SELECT * FROM items WHERE key = ? AND (expires_at IS NULL OR expires_at > ?)",
                Some(sqlite_now(&self.conn)?),
            )
        };
        let item = match now {
            Some(now) => self
                .conn
                .query_row(sql, params![key, now], item_from_row)
                .optional(),
            None => self
                .conn
                .query_row(sql, params![key], item_from_row)
                .optional(),
        }
        .map_err(err)?;
        Ok(item)
    }

    fn has_fts(&self) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'items_fts'",
                [],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    fn sync_fts(&mut self, item: &Item) -> ClipResult<()> {
        if !self.has_fts() {
            return Ok(());
        }
        self.conn
            .execute("DELETE FROM items_fts WHERE rowid = ?", params![item.id])
            .map_err(err)?;
        self.conn
            .execute(
                "
                INSERT INTO items_fts(rowid, key, value, namespace, tags_text)
                VALUES (?, ?, ?, ?, ?)
                ",
                params![
                    item.id,
                    item.key,
                    item.value,
                    item.namespace,
                    item.tags.join(" ")
                ],
            )
            .map_err(err)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default)]
pub struct UpdateOptions {
    pub namespace: Option<String>,
    pub kind: Option<String>,
    pub tags: Option<Vec<String>>,
    /// `Some(None)` explicitly clears a nullable field.
    pub source: Option<Option<String>>,
    pub ttl: Option<String>,
    pub metadata: Option<Value>,
    pub validated: Option<bool>,
    pub supersedes_key: Option<Option<String>>,
    pub pinned: Option<bool>,
    pub force_pin: bool,
}

pub fn default_db_path() -> PathBuf {
    let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    db_path_from(&start)
}

fn resolve_db_path(path: Option<&Path>) -> PathBuf {
    path.map(PathBuf::from).unwrap_or_else(default_db_path)
}

/// Resolve the clipboard database as if the process were running inside
/// `start`. Workspace panes use this before launch so their storage identity
/// does not depend on a new terminal session's inherited cwd.
pub fn db_path_from(start: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("AGENT_CLIPBOARD_DB") {
        return expand_home(PathBuf::from(path));
    }
    for dir in start.ancestors() {
        let candidate = dir.join(".agent-clipboard").join("clipboard.sqlite3");
        if candidate.exists() {
            return candidate;
        }
    }
    let home = home_dir();
    if cfg!(target_os = "macos") {
        home.join("Library")
            .join("Application Support")
            .join("agent-clipboard")
            .join("clipboard.sqlite3")
    } else {
        home.join(".local")
            .join("share")
            .join("agent-clipboard")
            .join("clipboard.sqlite3")
    }
}

pub fn parse_tag_text(text: Option<&str>) -> Vec<String> {
    parse_tags(
        text.unwrap_or_default()
            .split(',')
            .map(str::to_string)
            .collect(),
    )
}

pub fn preview(value: &str, width: usize) -> String {
    let one_line = value.lines().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= width {
        return one_line;
    }
    let mut out = one_line
        .chars()
        .take(width.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn ensure_schema(conn: &Connection) -> ClipResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS items (
            id INTEGER PRIMARY KEY,
            key TEXT NOT NULL UNIQUE,
            value TEXT NOT NULL,
            namespace TEXT NOT NULL DEFAULT 'default',
            kind TEXT NOT NULL DEFAULT 'fact',
            tags TEXT NOT NULL DEFAULT '[]',
            source TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            pinned INTEGER NOT NULL DEFAULT 0,
            validated INTEGER NOT NULL DEFAULT 0,
            supersedes_key TEXT,
            expires_at TEXT,
            checksum TEXT NOT NULL,
            metadata TEXT NOT NULL DEFAULT '{}',
            version INTEGER NOT NULL DEFAULT 1
        );
        ",
    )
    .map_err(err)?;
    // Migrate the complete current row shape before creating indexes or
    // preparing queries that reference newer columns. Legacy Forge stores can
    // contain data while missing any of these later additions.
    for &(name, definition) in ITEM_COLUMNS {
        ensure_column(conn, name, definition)?;
    }
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_items_key ON items(key);
        CREATE INDEX IF NOT EXISTS idx_items_namespace ON items(namespace);
        CREATE INDEX IF NOT EXISTS idx_items_kind ON items(kind);
        CREATE INDEX IF NOT EXISTS idx_items_pinned ON items(pinned);
        CREATE INDEX IF NOT EXISTS idx_items_validated ON items(validated);
        CREATE INDEX IF NOT EXISTS idx_items_updated_at ON items(updated_at);
        CREATE INDEX IF NOT EXISTS idx_items_expires_at ON items(expires_at);
        ",
    )
    .map_err(err)?;
    // Some old Forge databases used an external-content FTS table. Replace it
    // with the current standalone index exactly as the Python implementation.
    let fts_sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'items_fts'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(err)?;
    if fts_sql
        .as_deref()
        .is_some_and(|sql| sql.contains("content='items'"))
    {
        conn.execute("DROP TABLE items_fts", []).map_err(err)?;
    }
    let _ = conn.execute_batch(
        "
        CREATE VIRTUAL TABLE IF NOT EXISTS items_fts USING fts5(
            key, value, namespace, tags_text
        );
        ",
    );
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(err)?;
    Ok(())
}

fn ensure_read_schema(conn: &Connection, path: &Path) -> ClipResult<()> {
    let columns = item_columns(conn)?;
    let missing = ITEM_COLUMNS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !columns.contains(*name))
        .collect::<Vec<_>>();
    if columns.is_empty()
        || !columns.contains("id")
        || !columns.contains("key")
        || !columns.contains("value")
    {
        return Err(format!(
            "clipboard database {} has no compatible items table; initialize it with `clip --db {} doctor` from a writable shell",
            path.display(),
            path.display()
        ));
    }
    if !missing.is_empty() {
        return Err(format!(
            "clipboard database {} requires migration (missing: {}); run `clip --db {} doctor` from a writable shell",
            path.display(),
            missing.join(", "),
            path.display()
        ));
    }
    Ok(())
}

fn item_columns(conn: &Connection) -> ClipResult<BTreeSet<String>> {
    let mut stmt = conn.prepare("PRAGMA table_info(items)").map_err(err)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(err)?
        .collect::<Result<BTreeSet<_>, _>>()
        .map_err(err)?;
    Ok(columns)
}

fn ensure_column(conn: &Connection, name: &str, definition: &str) -> ClipResult<()> {
    let columns = item_columns(conn)?;
    if !columns.contains(name) {
        conn.execute(
            &format!("ALTER TABLE items ADD COLUMN {name} {definition}"),
            [],
        )
        .map_err(err)?;
    }
    Ok(())
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<Item> {
    let tags: String = row.get("tags")?;
    let metadata: String = row.get("metadata")?;
    Ok(Item {
        id: row.get("id")?,
        key: row.get("key")?,
        value: row.get("value")?,
        namespace: row.get("namespace")?,
        kind: row.get("kind")?,
        tags: tags_from_json(&tags),
        source: row.get("source")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        pinned: row.get::<_, i64>("pinned")? != 0,
        validated: row.get::<_, i64>("validated")? != 0,
        supersedes_key: row.get("supersedes_key")?,
        expires_at: row.get("expires_at")?,
        checksum: row.get("checksum")?,
        metadata: serde_json::from_str(&metadata).unwrap_or_else(|_| json!({})),
        version: row.get("version")?,
    })
}

fn normalize_kind(kind: &str) -> ClipResult<String> {
    let kind = kind.trim();
    if VALID_KINDS.contains(&kind) {
        Ok(kind.to_string())
    } else {
        Err(format!("invalid kind: {kind}"))
    }
}

fn parse_tags(tags: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    tags.into_iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty() && seen.insert(tag.clone()))
        .collect()
}

fn tags_from_json(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn parse_ttl_seconds(ttl: &str) -> ClipResult<i64> {
    let text = ttl.trim().to_ascii_lowercase();
    if text.is_empty() {
        return Err("TTL is empty".into());
    }
    let (number, multiplier) = match text.chars().last() {
        Some('s') => (&text[..text.len() - 1], 1),
        Some('m') => (&text[..text.len() - 1], 60),
        Some('h') => (&text[..text.len() - 1], 60 * 60),
        Some('d') => (&text[..text.len() - 1], 24 * 60 * 60),
        Some(c) if c.is_ascii_alphabetic() => {
            return Err(format!("unsupported TTL unit: {c}"));
        }
        _ => (text.as_str(), 1),
    };
    let amount = number
        .parse::<i64>()
        .map_err(|_| format!("invalid TTL: {ttl}"))?;
    Ok(amount * multiplier)
}

fn sqlite_now(conn: &Connection) -> ClipResult<String> {
    conn.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now')", [], |row| {
        row.get(0)
    })
    .map_err(err)
}

fn sqlite_expiry(conn: &Connection, seconds: i64) -> ClipResult<String> {
    conn.query_row(
        "SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now', ?)",
        params![format!("{seconds:+} seconds")],
        |row| row.get(0),
    )
    .map_err(err)
}

fn checksum(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn canonical_json(value: &Value) -> ClipResult<String> {
    let object = value
        .as_object()
        .ok_or_else(|| "metadata must be a JSON object".to_string())?;
    let sorted = object
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<BTreeMap<_, _>>();
    serde_json::to_string(&sorted).map_err(err)
}

fn fts_query(text: &str) -> String {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || "_.:/-".contains(c)))
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn score_item(item: &Item, query: &str, namespace: Option<&str>, tags: &[String]) -> f64 {
    let q = query.trim().to_ascii_lowercase();
    let key = item.key.to_ascii_lowercase();
    let value = item.value.to_ascii_lowercase();
    let mut score = if key == q {
        1000.0
    } else if key.starts_with(&q) {
        500.0
    } else if key.contains(&q) {
        250.0
    } else if value.contains(&q) {
        100.0
    } else {
        let haystack = format!(
            "{key} {} {} {}",
            item.namespace.to_ascii_lowercase(),
            item.tags.join(" ").to_ascii_lowercase(),
            value.chars().take(1000).collect::<String>()
        );
        similarity(&q, &haystack) * 80.0
    };
    score += match (item.pinned, item.validated) {
        (true, true) => 220.0,
        (true, false) => 60.0,
        (false, true) => 40.0,
        _ => 0.0,
    };
    if namespace.is_some_and(|n| item.namespace == n) {
        score += 80.0;
    }
    let have: BTreeSet<&str> = item.tags.iter().map(String::as_str).collect();
    score += 50.0
        * tags
            .iter()
            .filter(|tag| have.contains(tag.as_str()))
            .count() as f64;
    score
}

/// Dice coefficient over character bigrams: cheap, bounded, and deterministic.
fn similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    let grams = |s: &str| {
        let chars = s.chars().collect::<Vec<_>>();
        chars
            .windows(2)
            .map(|pair| (pair[0], pair[1]))
            .collect::<Vec<_>>()
    };
    let a = grams(a);
    let mut b = grams(b);
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let mut matches = 0;
    for gram in &a {
        if let Some(index) = b.iter().position(|candidate| candidate == gram) {
            matches += 1;
            b.swap_remove(index);
        }
    }
    2.0 * matches as f64 / (a.len() + b.len()) as f64
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_home(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" {
        home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        path
    }
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(name: &str) -> (PathBuf, Store) {
        let path = std::env::temp_dir().join(format!(
            "sauron-clip-{name}-{}-{}.sqlite3",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);
        let store = Store::open(Some(&path)).unwrap();
        (path, store)
    }

    #[test]
    fn put_get_update_and_pin_policy() {
        let (path, mut store) = temp_store("crud");
        let first = store
            .put(
                "project.state",
                "line one\nline two\n",
                PutOptions::default(),
            )
            .unwrap();
        assert_eq!(first.version, 1);
        assert_eq!(
            store.get("project.state").unwrap().unwrap().value,
            "line one\nline two\n"
        );
        let second = store
            .update(
                "project.state",
                "updated",
                UpdateOptions {
                    validated: Some(true),
                    ..UpdateOptions::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(second.version, 2);
        assert!(
            store
                .pin("project.state", true, false)
                .unwrap()
                .unwrap()
                .pinned
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn migrates_populated_legacy_schema_before_creating_indexes() {
        let path = std::env::temp_dir().join(format!(
            "sauron-clip-legacy-{}-{}.sqlite3",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE items (
                    id INTEGER PRIMARY KEY,
                    key TEXT NOT NULL UNIQUE,
                    value TEXT NOT NULL,
                    namespace TEXT NOT NULL DEFAULT 'default',
                    tags TEXT NOT NULL DEFAULT '[]',
                    source TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    pinned INTEGER NOT NULL DEFAULT 0,
                    checksum TEXT NOT NULL,
                    metadata TEXT NOT NULL DEFAULT '{}',
                    version INTEGER NOT NULL DEFAULT 1
                );
                INSERT INTO items (
                    key, value, namespace, tags, created_at, updated_at,
                    pinned, checksum, metadata, version
                ) VALUES (
                    'legacy.key', 'legacy value', 'project', '[]',
                    '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z',
                    0, 'legacy-checksum', '{}', 4
                );
                ",
            )
            .unwrap();
        }

        let error = Store::open_read_only(Some(&path)).unwrap_err();
        assert!(error.contains(path.to_str().unwrap()));
        assert!(error.contains("expires_at"));
        assert!(error.contains("doctor"));

        let store = Store::open(Some(&path)).unwrap();
        let item = store.get("legacy.key").unwrap().unwrap();
        assert_eq!(item.value, "legacy value");
        assert_eq!(item.kind, "fact");
        assert!(!item.validated);
        assert_eq!(item.expires_at, None);
        assert_eq!(item.version, 4);
        assert_eq!(
            store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'idx_items_expires_at'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        let doctor = store.doctor().unwrap();
        assert_eq!(doctor["database"], path.to_str().unwrap());
        assert_eq!(doctor["schema_version"], SCHEMA_VERSION);
        drop(store);

        let mut read_only = Store::open_read_only(Some(&path)).unwrap();
        assert_eq!(
            read_only.get("legacy.key").unwrap().unwrap().value,
            "legacy value"
        );
        assert!(read_only
            .put("should.fail", "read-only", PutOptions::default())
            .unwrap_err()
            .contains("readonly"));
        drop(read_only);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn exact_key_and_validated_pinned_entries_rank_first() {
        let (path, mut store) = temp_store("search");
        store
            .put("alpha", "ordinary", PutOptions::default())
            .unwrap();
        store
            .put(
                "other",
                "alpha appears here",
                PutOptions {
                    validated: Some(true),
                    pinned: true,
                    ..PutOptions::default()
                },
            )
            .unwrap();
        let found = store.search(Some("alpha"), None, None, &[], 10).unwrap();
        assert_eq!(found[0].key, "alpha", "exact key must dominate boosts");
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ttl_gc_and_export_import_round_trip() {
        let (path, mut store) = temp_store("roundtrip");
        store
            .put(
                "expired",
                "gone",
                PutOptions {
                    ttl: Some("-1s".into()),
                    ..PutOptions::default()
                },
            )
            .unwrap();
        assert!(store.get("expired").unwrap().is_none());
        assert_eq!(store.gc().unwrap(), 1);
        store.put("kept", "value", PutOptions::default()).unwrap();
        let payload = store.export(None, None, vec![], "json", 10).unwrap();
        let target = path.with_extension("import.sqlite3");
        let _ = std::fs::remove_file(&target);
        let mut imported = Store::open(Some(&target)).unwrap();
        assert_eq!(imported.import(&payload, true).unwrap(), 1);
        assert_eq!(imported.get("kept").unwrap().unwrap().value, "value");
        drop((store, imported));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(target);
    }
}
