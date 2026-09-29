//! Channels network state: one SQLite database at `~/AgentArea/state.db`.
//!
//! Rust owns it. Everything else goes through here:
//! - the app (Inventory view) and plugins that declared "state" call
//!   `state_call` (lib.rs) → [`StateDb::call`];
//! - agents in local threads use the `agentarea_data` MCP tools
//!   ([`mcp_tools`] / [`mcp_call`], wired in data_mcp.rs).
//!
//! Every mutation runs in one transaction and writes an `audit_log` row with
//! its actor (`user`, `plugin:<id>`, `agent:<thread>`, `import`). After a
//! commit, read-only JSON snapshots are written (debounced) to
//! `~/AgentArea/channels/export/` so diffs, backups and the standalone
//! grid.html keep working. Long texts (SOUL.md) stay in the persona folders.
//!
//! Secrets never enter the database: an inventory item points at a vault
//! secret by name (`secret_ref`); the value stays in the Keychain.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value as Sql, ValueRef};
use rusqlite::{params, params_from_iter, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Map, Value};

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

pub const PLATFORMS: &[&str] = &["tiktok", "instagram", "youtube", "facebook", "x", "threads", "telegram"];
pub const STAGES: &[&str] = &["idea", "setup", "setup_done", "live", "paused", "stopped"];
pub const ROLES: &[&str] = &["creator", "community", "engager"];
pub const ITEM_TYPES: &[&str] =
    &["proxy", "phone", "email", "device_profile", "api_key", "account", "domain", "card", "other"];
pub const ITEM_STATUSES: &[&str] = &["active", "reserved", "broken", "retired"];
pub const SUBJECT_TYPES: &[&str] = &["persona", "channel", "mcp"];
pub const PURPOSES: &[&str] = &["egress", "login", "2fa", "api", "device", "other"];
pub const POST_STATUSES: &[&str] = &["draft", "scheduled", "published"];
pub const POST_KINDS: &[&str] = &["video_script", "caption", "text", "telegram_post", "carousel", "comment_reply"];
pub const REVIEW_STATUSES: &[&str] = &["pending", "liked", "disliked", "needs_changes", "approved"];
pub const VERDICTS: &[&str] = &["like", "dislike", "needs_changes", "approve"];
const BUDGET_KEYS: [&str; 5] = ["proxy", "account", "content", "ads", "other"];
/// Columns holding JSON text; parsed back into JSON on the way out.
const JSON_COLS: &[&str] = &["languages", "personality", "refs", "meta", "diff", "hashtags"];

pub const QUERY_ROWS_DEFAULT: usize = 200;
pub const QUERY_ROWS_MAX: usize = 1000;
const QUERY_TIME: Duration = Duration::from_secs(3);
const TEXT_MAX: usize = 20_000;
const EXPORT_FILES: &[&str] =
    &["personas.json", "channels.json", "inventory.json", "bindings.json", "posts.json", "metrics.json", "gaps.json"];

/// The purpose a binding gets when none is given.
pub fn default_purpose(item_type: &str) -> &'static str {
    match item_type {
        "proxy" => "egress",
        "email" => "login",
        "phone" => "2fa",
        "device_profile" => "device",
        "api_key" => "api",
        "account" => "login",
        _ => "other",
    }
}

// ── time (UTC, no extra crates) ──────────────────────────────────────────────

fn civil(secs: i64) -> (i64, i64, i64, i64, i64, i64) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// `2026-09-29T12:00:00Z`
pub fn now_iso() -> String {
    let (y, m, d, hh, mm, ss) = civil(now_secs());
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// `2026-09`, the period monthly actuals are kept under.
pub fn this_month() -> String {
    let (y, m, ..) = civil(now_secs());
    format!("{y:04}-{m:02}")
}

// ── schema ───────────────────────────────────────────────────────────────────

/// Versioned migrations; `schema_version` records the ones applied. Append
/// only — never edit a shipped entry.
const MIGRATIONS: &[&str] = &[r#"
CREATE TABLE personas (
  handle          TEXT PRIMARY KEY,
  name            TEXT NOT NULL DEFAULT '',
  emoji           TEXT NOT NULL DEFAULT '',
  niche           TEXT NOT NULL DEFAULT '',
  category        TEXT NOT NULL DEFAULT '',
  country         TEXT NOT NULL DEFAULT '',
  city            TEXT NOT NULL DEFAULT '',
  languages       TEXT NOT NULL DEFAULT '[]',
  target_audience TEXT NOT NULL DEFAULT '',
  hypothesis      TEXT NOT NULL DEFAULT '',
  personality     TEXT NOT NULL DEFAULT '{}',
  refs            TEXT NOT NULL DEFAULT '[]',
  soul_path       TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
CREATE TABLE channels (
  id                INTEGER PRIMARY KEY,
  persona           TEXT NOT NULL REFERENCES personas(handle) ON UPDATE CASCADE ON DELETE CASCADE,
  platform          TEXT NOT NULL,
  handle_registered TEXT NOT NULL DEFAULT '',
  url               TEXT NOT NULL DEFAULT '',
  stage             TEXT NOT NULL DEFAULT 'idea' CHECK (stage IN ('idea','setup','setup_done','live','paused','stopped')),
  role              TEXT NOT NULL DEFAULT 'creator' CHECK (role IN ('creator','community','engager')),
  plan              TEXT NOT NULL DEFAULT '',
  kpi_metric        TEXT NOT NULL DEFAULT '',
  kpi_target        REAL,
  mcp               TEXT NOT NULL DEFAULT '',
  notes             TEXT NOT NULL DEFAULT '',
  created_at        TEXT NOT NULL,
  updated_at        TEXT NOT NULL,
  UNIQUE (persona, platform)
);
CREATE TABLE channel_tasks (
  id         INTEGER PRIMARY KEY,
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  position   INTEGER NOT NULL DEFAULT 0,
  type       TEXT NOT NULL,
  cadence    TEXT NOT NULL DEFAULT '',
  notes      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX channel_tasks_channel ON channel_tasks(channel_id);
CREATE TABLE budget (
  channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  kind       TEXT NOT NULL CHECK (kind IN ('planned','spent')),
  period     TEXT NOT NULL,
  currency   TEXT NOT NULL DEFAULT 'EUR',
  proxy      REAL NOT NULL DEFAULT 0,
  account    REAL NOT NULL DEFAULT 0,
  content    REAL NOT NULL DEFAULT 0,
  ads        REAL NOT NULL DEFAULT 0,
  other      REAL NOT NULL DEFAULT 0,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (channel_id, kind, period)
);
CREATE TABLE metrics (
  id              INTEGER PRIMARY KEY,
  channel_id      INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  date            TEXT NOT NULL,
  reach           REAL,
  followers       REAL,
  posts_published REAL,
  cost            REAL,
  currency        TEXT NOT NULL DEFAULT 'EUR',
  UNIQUE (channel_id, date)
);
CREATE TABLE posts (
  id           INTEGER PRIMARY KEY,
  channel_id   INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
  scheduled_at TEXT NOT NULL DEFAULT '',
  status       TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft','scheduled','published')),
  title        TEXT NOT NULL DEFAULT '',
  body_or_path TEXT NOT NULL DEFAULT '',
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);
CREATE INDEX posts_channel ON posts(channel_id);
CREATE TABLE inventory_items (
  id         INTEGER PRIMARY KEY,
  type       TEXT NOT NULL CHECK (type IN ('proxy','phone','email','device_profile','api_key','domain','card','other')),
  label      TEXT NOT NULL,
  provider   TEXT NOT NULL DEFAULT '',
  geo        TEXT NOT NULL DEFAULT '',
  cost_month REAL,
  currency   TEXT NOT NULL DEFAULT 'EUR',
  status     TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','reserved','broken','retired')),
  capacity   INTEGER CHECK (capacity IS NULL OR capacity >= 1),
  secret_ref TEXT,
  meta       TEXT NOT NULL DEFAULT '{}',
  notes      TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
CREATE TABLE bindings (
  id           INTEGER PRIMARY KEY,
  item_id      INTEGER NOT NULL REFERENCES inventory_items(id) ON DELETE CASCADE,
  subject_type TEXT NOT NULL CHECK (subject_type IN ('persona','channel','mcp')),
  subject_id   TEXT NOT NULL,
  purpose      TEXT NOT NULL CHECK (purpose IN ('egress','login','2fa','api','device','other')),
  since        TEXT NOT NULL,
  until        TEXT,
  notes        TEXT NOT NULL DEFAULT ''
);
CREATE UNIQUE INDEX bindings_active ON bindings(item_id, subject_type, subject_id, purpose) WHERE until IS NULL;
CREATE INDEX bindings_subject ON bindings(subject_type, subject_id) WHERE until IS NULL;
CREATE TABLE inventory_requirements (
  id        INTEGER PRIMARY KEY,
  platform  TEXT NOT NULL DEFAULT '*',
  type      TEXT NOT NULL CHECK (type IN ('proxy','phone','email','device_profile','api_key','domain','card','other')),
  scope     TEXT NOT NULL CHECK (scope IN ('persona','channel')),
  purpose   TEXT NOT NULL CHECK (purpose IN ('egress','login','2fa','api','device','other')),
  count     INTEGER NOT NULL DEFAULT 1 CHECK (count >= 1),
  geo_match INTEGER NOT NULL DEFAULT 0,
  notes     TEXT NOT NULL DEFAULT '',
  UNIQUE (platform, type, scope, purpose)
);
INSERT INTO inventory_requirements (platform, type, scope, purpose, geo_match, notes) VALUES
  ('*', 'proxy',          'persona', 'egress', 1, 'one residential IP in the persona''s own country, shared by its channels'),
  ('*', 'email',          'persona', 'login',  0, 'sign-up / login address'),
  ('*', 'phone',          'persona', '2fa',    0, 'SMS / 2FA number'),
  ('*', 'device_profile', 'persona', 'device', 0, 'browser or device profile the accounts run in');
CREATE TABLE audit_log (
  id        INTEGER PRIMARY KEY,
  at        TEXT NOT NULL,
  actor     TEXT NOT NULL,
  action    TEXT NOT NULL,
  entity    TEXT NOT NULL,
  entity_id TEXT NOT NULL DEFAULT '',
  diff      TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_entity ON audit_log(entity, entity_id);
"#,
// v2: inventory type 'account' (a registered / purchased platform account; binds to
// a channel for login). SQLite can't alter a CHECK, so both tables are rebuilt;
// rows stored as type 'other' with meta.kind = 'account' become 'account'.
r#"
CREATE TABLE inventory_items_v2 (
  id         INTEGER PRIMARY KEY,
  type       TEXT NOT NULL CHECK (type IN ('proxy','phone','email','device_profile','api_key','account','domain','card','other')),
  label      TEXT NOT NULL,
  provider   TEXT NOT NULL DEFAULT '',
  geo        TEXT NOT NULL DEFAULT '',
  cost_month REAL,
  currency   TEXT NOT NULL DEFAULT 'EUR',
  status     TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','reserved','broken','retired')),
  capacity   INTEGER CHECK (capacity IS NULL OR capacity >= 1),
  secret_ref TEXT,
  meta       TEXT NOT NULL DEFAULT '{}',
  notes      TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);
INSERT INTO inventory_items_v2
  SELECT id,
         CASE WHEN type = 'other' AND json_valid(meta) AND json_extract(meta, '$.kind') = 'account' THEN 'account' ELSE type END,
         label, provider, geo, cost_month, currency, status, capacity, secret_ref, meta, notes, created_at, updated_at
  FROM inventory_items;
INSERT INTO audit_log (at, actor, action, entity, entity_id, diff)
  SELECT strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), 'migration', 'update', 'inventory_item', CAST(n.id AS TEXT),
         '{"type":{"from":"other","to":"account"}}'
  FROM inventory_items_v2 n JOIN inventory_items o ON o.id = n.id WHERE o.type = 'other' AND n.type = 'account';
DROP TABLE inventory_items;
ALTER TABLE inventory_items_v2 RENAME TO inventory_items;
CREATE TABLE inventory_requirements_v2 (
  id        INTEGER PRIMARY KEY,
  platform  TEXT NOT NULL DEFAULT '*',
  type      TEXT NOT NULL CHECK (type IN ('proxy','phone','email','device_profile','api_key','account','domain','card','other')),
  scope     TEXT NOT NULL CHECK (scope IN ('persona','channel')),
  purpose   TEXT NOT NULL CHECK (purpose IN ('egress','login','2fa','api','device','other')),
  count     INTEGER NOT NULL DEFAULT 1 CHECK (count >= 1),
  geo_match INTEGER NOT NULL DEFAULT 0,
  notes     TEXT NOT NULL DEFAULT '',
  UNIQUE (platform, type, scope, purpose)
);
INSERT INTO inventory_requirements_v2 SELECT id, platform, type, scope, purpose, count, geo_match, notes FROM inventory_requirements;
DROP TABLE inventory_requirements;
ALTER TABLE inventory_requirements_v2 RENAME TO inventory_requirements;
"#,
// v3: content review. Posts carry the generated content (kind, hook, script,
// caption, hashtags, media brief, language), who generated it and where
// feedback goes (context_ref = a desktop thread id), a revision chain and a
// review status; post_reviews holds each verdict until it is resolved.
r#"
ALTER TABLE posts ADD COLUMN kind TEXT NOT NULL DEFAULT 'text'
  CHECK (kind IN ('video_script','caption','text','telegram_post','carousel','comment_reply'));
ALTER TABLE posts ADD COLUMN hook TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN script TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN caption TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN hashtags TEXT NOT NULL DEFAULT '[]';
ALTER TABLE posts ADD COLUMN media_brief TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN language TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN generated_by TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN context_ref TEXT NOT NULL DEFAULT '';
ALTER TABLE posts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;
ALTER TABLE posts ADD COLUMN parent_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL;
ALTER TABLE posts ADD COLUMN review_status TEXT NOT NULL DEFAULT 'pending'
  CHECK (review_status IN ('pending','liked','disliked','needs_changes','approved'));
CREATE INDEX posts_parent ON posts(parent_post_id);
CREATE INDEX posts_review ON posts(review_status);
CREATE TABLE post_reviews (
  id                 INTEGER PRIMARY KEY,
  post_id            INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE,
  verdict            TEXT NOT NULL CHECK (verdict IN ('like','dislike','needs_changes','approve')),
  comment            TEXT NOT NULL DEFAULT '',
  actor              TEXT NOT NULL DEFAULT '',
  at                 TEXT NOT NULL,
  delivered_at       TEXT,
  delivered_to       TEXT NOT NULL DEFAULT '',
  resolved_at        TEXT,
  resolution_post_id INTEGER REFERENCES posts(id) ON DELETE SET NULL,
  resolution_note    TEXT NOT NULL DEFAULT ''
);
CREATE INDEX post_reviews_post ON post_reviews(post_id);
CREATE INDEX post_reviews_open ON post_reviews(post_id) WHERE resolved_at IS NULL;
"#,
];

fn migrate(conn: &mut Connection) -> R<usize> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL, applied_at TEXT NOT NULL)")
        .map_err(e)?;
    let current: i64 =
        conn.query_row("SELECT coalesce(max(version), 0) FROM schema_version", [], |r| r.get(0)).map_err(e)?;
    let current = usize::try_from(current).unwrap_or(0);
    if current > MIGRATIONS.len() {
        return Err(format!(
            "state.db has schema version {current}, newer than this app knows ({}); update AgentArea Desktop",
            MIGRATIONS.len()
        ));
    }
    migrate_range(conn, current, MIGRATIONS.len())
}

/// Apply migrations `from+1 ..= to`. Table rebuilds need foreign keys off
/// (they can't be switched inside a transaction); each migration checks
/// them before it commits.
fn migrate_range(conn: &mut Connection, from: usize, to: usize) -> R<usize> {
    if from >= to {
        return Ok(to);
    }
    conn.execute_batch("PRAGMA foreign_keys = OFF").map_err(e)?;
    let result = (|| -> R<()> {
        for (i, sql) in MIGRATIONS.iter().enumerate().take(to).skip(from) {
            let tx = conn.transaction().map_err(e)?;
            tx.execute_batch(sql).map_err(|err| format!("migration {}: {err}", i + 1))?;
            let broken: i64 = tx
                .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))
                .map_err(e)?;
            if broken > 0 {
                return Err(format!("migration {}: {broken} rows break foreign keys", i + 1));
            }
            tx.execute("INSERT INTO schema_version (version, applied_at) VALUES (?1, ?2)", params![i as i64 + 1, now_iso()])
                .map_err(e)?;
            tx.commit().map_err(e)?;
        }
        Ok(())
    })();
    conn.execute_batch("PRAGMA foreign_keys = ON").map_err(e)?;
    result.map(|_| to)
}

// ── the database ─────────────────────────────────────────────────────────────

pub struct StateDb {
    /// `~/AgentArea` (tests: a temp dir): state.db, channels/, secrets.json.
    root: PathBuf,
    conn: Mutex<Connection>,
    /// Wakes the debounced exporter; `None` = export only on `export_now`.
    exporter: Mutex<Option<mpsc::Sender<()>>>,
}

static USER: Mutex<Option<Arc<StateDb>>> = Mutex::new(None);

/// The user's database, opened (and migrated) on first use. The first open of
/// an empty database imports the legacy files in `~/AgentArea/channels`.
pub fn user() -> R<Arc<StateDb>> {
    let mut slot = USER.lock().map_err(|_| "state db lock poisoned")?;
    if let Some(db) = slot.as_ref() {
        return Ok(db.clone());
    }
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let db = Arc::new(StateDb::open(&PathBuf::from(home).join("AgentArea"))?);
    db.start_exporter(Duration::from_secs(1));
    let legacy = db.channels_dir().join("channels.json");
    if db.is_empty()? && legacy.is_file() {
        if let Err(err) = db.import_dir("import", &db.channels_dir(), true) {
            eprintln!("state.db: import of {} failed: {err}", legacy.display());
        }
    }
    // Fresh snapshots on every start, and whenever another process (sqlite3, a
    // script) commits to the file: its writes don't pass through `write`.
    db.changed();
    db.watch_external(Duration::from_secs(2));
    *slot = Some(db.clone());
    Ok(db)
}

impl StateDb {
    pub fn open(root: &Path) -> R<StateDb> {
        std::fs::create_dir_all(root).map_err(|err| format!("{}: {err}", root.display()))?;
        let mut conn = Connection::open(root.join("state.db")).map_err(e)?;
        conn.busy_timeout(Duration::from_secs(5)).map_err(e)?;
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0)).map_err(e)?;
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;").map_err(e)?;
        migrate(&mut conn)?;
        Ok(StateDb { root: root.to_path_buf(), conn: Mutex::new(conn), exporter: Mutex::new(None) })
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("state.db")
    }

    pub fn channels_dir(&self) -> PathBuf {
        self.root.join("channels")
    }

    pub fn export_dir(&self) -> PathBuf {
        self.channels_dir().join("export")
    }

    /// Export snapshots `debounce` after the last change (a burst of writes
    /// makes one export).
    pub fn start_exporter(self: &Arc<Self>, debounce: Duration) {
        let (tx, rx) = mpsc::channel::<()>();
        let db = Arc::downgrade(self);
        std::thread::spawn(move || {
            while rx.recv().is_ok() {
                loop {
                    match rx.recv_timeout(debounce) {
                        Ok(()) => continue,
                        Err(mpsc::RecvTimeoutError::Timeout) => break,
                        Err(mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                }
                let Some(db) = db.upgrade() else { return };
                if let Err(err) = db.export_now() {
                    eprintln!("state.db export: {err}");
                }
            }
        });
        if let Ok(mut slot) = self.exporter.lock() {
            *slot = Some(tx);
        }
    }

    /// Poll `PRAGMA data_version`, which moves only when another connection
    /// commits, and export when it does.
    pub fn watch_external(self: &Arc<Self>, every: Duration) {
        let db = Arc::downgrade(self);
        std::thread::spawn(move || {
            let mut last: Option<i64> = None;
            loop {
                std::thread::sleep(every);
                let Some(db) = db.upgrade() else { return };
                let v = db.read(|c| c.query_row("PRAGMA data_version", [], |r| r.get::<_, i64>(0)).map_err(e));
                if let Ok(v) = v {
                    if last.is_some_and(|l| l != v) {
                        db.changed();
                    }
                    last = Some(v);
                }
            }
        });
    }

    fn changed(&self) {
        if let Ok(slot) = self.exporter.lock() {
            if let Some(tx) = slot.as_ref() {
                let _ = tx.send(());
            }
        }
    }

    fn read<T>(&self, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
        let conn = self.conn.lock().map_err(|_| "state db lock poisoned")?;
        f(&conn)
    }

    /// One transaction; the export runs after a successful commit.
    fn write<T>(&self, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
        let mut conn = self.conn.lock().map_err(|_| "state db lock poisoned")?;
        let tx = conn.transaction().map_err(e)?;
        let out = f(&tx)?;
        tx.commit().map_err(e)?;
        drop(conn);
        self.changed();
        Ok(out)
    }

    pub fn is_empty(&self) -> R<bool> {
        self.read(|c| Ok(c.query_row("SELECT count(*) FROM personas", [], |r| r.get::<_, i64>(0)).map_err(e)? == 0))
    }

    /// Names of the vault's secrets (metadata only), for `secret_ref` checks.
    fn secret_names(&self) -> HashSet<String> {
        std::fs::read_to_string(self.root.join("secrets.json"))
            .ok()
            .and_then(|json| crate::vault::parse_secrets(&json).ok())
            .map(|list| list.into_iter().map(|s| s.name).collect())
            .unwrap_or_default()
    }

    /// Write the read-only JSON snapshots now.
    pub fn export_now(&self) -> R<()> {
        let secrets = self.secret_names();
        let snap = self.read(|c| snapshot(c, &secrets))?;
        let dir = self.export_dir();
        std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
        for file in EXPORT_FILES {
            let key = file.trim_end_matches(".json");
            let body = serde_json::to_string_pretty(&snap[key]).map_err(e)?;
            write_readonly(&dir.join(file), &body)?;
        }
        Ok(())
    }

    /// The one entry point for the app, plugins and agents. `actor` lands in
    /// the audit log of every mutation.
    pub fn call(&self, actor: &str, method: &str, p: &Value) -> R<Value> {
        match method {
            "rev" => self.read(rev).map(|r| json!({ "rev": r })),
            "summary" => self.read(summary),
            "snapshot" => {
                let secrets = self.secret_names();
                self.read(|c| snapshot(c, &secrets))
            }
            "personas" => self.read(|c| {
                query_json(
                    c,
                    "SELECT handle, name, emoji, niche, category, country, city FROM personas ORDER BY handle",
                    [],
                )
                .map(Value::Array)
            }),
            "persona" => self.read(|c| persona_full(c, &req_str(p, "handle")?)),
            "channels" => self.read(|c| channels_list(c, p)),
            "channel" => self.read(|c| {
                let id = channel_ref(c, p)?;
                channel_full(c, id)
            }),
            "inventory" => {
                let secrets = self.secret_names();
                self.read(|c| inventory(c, p, &secrets))
            }
            "inventory_summary" => self.read(inventory_summary),
            "bindings" => self.read(|c| bindings_list(c, p)),
            "requirements" => self.read(|c| query_json(c, "SELECT * FROM inventory_requirements ORDER BY id", []).map(Value::Array)),
            "gaps" => self.read(|c| gaps(c).map(|g| trim_gaps(g, p))),
            "audit" => self.read(|c| audit_list(c, p)),
            "query" => {
                let sql = req_str(p, "sql")?;
                let args = p.get("params").and_then(Value::as_array).cloned().unwrap_or_default();
                let limit = p.get("limit").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(QUERY_ROWS_DEFAULT);
                self.query_readonly(&sql, &args, limit)
            }
            "persona_update" => self.write(|tx| persona_update(tx, actor, p)),
            "channel_update" => self.write(|tx| channel_update(tx, actor, p)),
            "inventory_add" => self.write(|tx| inventory_add(tx, actor, p)),
            "inventory_update" => self.write(|tx| inventory_update(tx, actor, p)),
            "inventory_delete" => self.write(|tx| inventory_delete(tx, actor, p)),
            "inventory_bind" => self.write(|tx| bind(tx, actor, p)),
            "inventory_unbind" => self.write(|tx| unbind(tx, actor, p)),
            "inventory_autobind" => self.write(|tx| autobind(tx, actor, p)),
            "review_feed" => self.read(|c| review_feed(c, p)),
            "review_feedback" => self.read(|c| review_feedback(c, p)),
            "post_history" => self.read(|c| post_history(c, p)),
            "post_upsert" => self.write(|tx| post_upsert(tx, actor, p)),
            "post_revise" => self.write(|tx| post_revise(tx, actor, p)),
            "post_review_add" => self.write(|tx| post_review_add(tx, actor, p)),
            "post_review_undo" => self.write(|tx| post_review_undo(tx, actor, p)),
            "post_review_delivered" => self.write(|tx| post_review_delivered(tx, actor, p)),
            "post_review_resolve" => self.write(|tx| post_review_resolve(tx, actor, p)),
            "post_delete" => self.write(|tx| post_delete(tx, actor, p)),
            "metrics_add" => self.write(|tx| metrics_add(tx, actor, p)),
            "requirement_add" => self.write(|tx| requirement_add(tx, actor, p)),
            "requirement_delete" => self.write(|tx| requirement_delete(tx, actor, p)),
            "import" => {
                let dir = p.get("dir").and_then(Value::as_str).map(PathBuf::from).unwrap_or_else(|| self.channels_dir());
                let move_legacy = p.get("move_legacy").and_then(Value::as_bool).unwrap_or(true);
                self.import_dir(actor, &dir, move_legacy)
            }
            other => Err(format!("unknown state method: {other}")),
        }
    }

    /// Read-only SQL for agents and plugins: SELECT / WITH only, one
    /// statement, on a read-only connection with `query_only`, an authorizer
    /// that denies anything but reads, a row cap and a time limit.
    pub fn query_readonly(&self, sql: &str, args: &[Value], limit: usize) -> R<Value> {
        check_select(sql)?;
        let limit = limit.clamp(1, QUERY_ROWS_MAX);
        let conn = Connection::open_with_flags(
            self.path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(e)?;
        conn.busy_timeout(Duration::from_secs(2)).map_err(e)?;
        conn.execute_batch("PRAGMA query_only = ON").map_err(e)?;
        conn.authorizer(Some(read_only_authorizer)).map_err(e)?;
        let started = Instant::now();
        conn.progress_handler(10_000, Some(move || started.elapsed() > QUERY_TIME)).map_err(e)?;

        let mut stmt = conn.prepare(sql).map_err(|err| format!("query rejected: {err}"))?;
        if !stmt.readonly() {
            return Err("query rejected: only read-only SELECT statements are allowed".into());
        }
        let columns: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let sql_args: Vec<Sql> = args.iter().map(json_to_sql).collect::<R<_>>()?;
        let mut rows = stmt.query(params_from_iter(sql_args.iter())).map_err(|err| format!("query failed: {err}"))?;
        let mut out = Vec::new();
        let mut truncated = false;
        while let Some(row) = rows.next().map_err(|err| {
            if started.elapsed() > QUERY_TIME {
                format!("query stopped after {}s", QUERY_TIME.as_secs())
            } else {
                format!("query failed: {err}")
            }
        })? {
            if out.len() == limit {
                truncated = true;
                break;
            }
            let mut obj = Map::new();
            for (i, name) in columns.iter().enumerate() {
                obj.insert(name.clone(), sql_to_json(row.get_ref(i).map_err(e)?));
            }
            out.push(Value::Object(obj));
        }
        let mut res = json!({ "columns": columns, "rows": out, "row_count": out.len() });
        if truncated {
            res["truncated"] = json!(format!("stopped at {limit} rows; add LIMIT/OFFSET or aggregate"));
        }
        Ok(res)
    }
}

fn read_only_authorizer(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select | AuthAction::Read { .. } | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Function { function_name } => {
            let f = function_name.to_ascii_lowercase();
            if ["load_extension", "readfile", "writefile", "edit", "fts3_tokenizer"].contains(&f.as_str()) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }
        // Table-valued schema pragmas inside a SELECT (pragma_table_info('channels')).
        AuthAction::Pragma { pragma_name, .. }
            if ["table_info", "table_xinfo", "index_list", "index_info", "foreign_key_list"]
                .contains(&pragma_name.to_ascii_lowercase().as_str()) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }
}

/// First keyword SELECT or WITH (after comments), and a single statement.
fn check_select(sql: &str) -> R<()> {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.split_once('\n').map(|(_, r)| r).unwrap_or("").trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.split_once("*/").map(|(_, r)| r).unwrap_or("").trim_start();
        } else {
            break;
        }
    }
    let word: String = s.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase();
    if word != "SELECT" && word != "WITH" {
        return Err("query rejected: state_query runs read-only SELECT / WITH statements only; use the typed tools to change data".into());
    }
    let body = s.trim_end().trim_end_matches(';');
    if body.contains(';') && !body.split(';').skip(1).all(|part| part.trim().is_empty() || part.trim().starts_with("--")) {
        return Err("query rejected: one statement per call".into());
    }
    Ok(())
}

fn write_readonly(path: &Path, body: &str) -> R<()> {
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::fs::write(&tmp, body).map_err(|err| format!("{}: {err}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444));
    }
    std::fs::rename(&tmp, path).map_err(|err| format!("{}: {err}", path.display()))
}

// ── JSON ↔ SQL ───────────────────────────────────────────────────────────────

fn sql_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!(format!("<{} bytes>", b.len())),
    }
}

fn json_to_sql(v: &Value) -> R<Sql> {
    Ok(match v {
        Value::Null => Sql::Null,
        Value::Bool(b) => Sql::Integer(i64::from(*b)),
        Value::Number(n) => n.as_i64().map(Sql::Integer).unwrap_or_else(|| Sql::Real(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => Sql::Text(s.clone()),
        other => Sql::Text(other.to_string()),
    })
}

/// Rows as JSON objects; known JSON columns parsed back.
fn query_json<P: rusqlite::Params>(conn: &Connection, sql: &str, args: P) -> R<Vec<Value>> {
    let mut stmt = conn.prepare(sql).map_err(e)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(args).map_err(e)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(e)? {
        let mut obj = Map::new();
        for (i, name) in cols.iter().enumerate() {
            let mut v = sql_to_json(row.get_ref(i).map_err(e)?);
            if JSON_COLS.contains(&name.as_str()) {
                if let Some(parsed) = v.as_str().and_then(|s| serde_json::from_str::<Value>(s).ok()) {
                    v = parsed;
                }
            }
            obj.insert(name.clone(), v);
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

fn query_one<P: rusqlite::Params>(conn: &Connection, sql: &str, args: P) -> R<Option<Value>> {
    Ok(query_json(conn, sql, args)?.into_iter().next())
}

fn req_str(p: &Value, key: &str) -> R<String> {
    p.get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("\"{key}\" is required"))
}

fn opt_str(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(Value::as_str).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Accepts 12, 12.5 and "12.5"; empty / null → None.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().replace(',', ".").parse().ok(),
        _ => None,
    }
}

fn audit(conn: &Connection, actor: &str, action: &str, entity: &str, entity_id: &str, diff: &Value) -> R<()> {
    conn.execute(
        "INSERT INTO audit_log (at, actor, action, entity, entity_id, diff) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![now_iso(), actor, action, entity, entity_id, diff.to_string()],
    )
    .map_err(e)?;
    Ok(())
}

fn rev(conn: &Connection) -> R<String> {
    let max: i64 = conn.query_row("SELECT coalesce(max(id), 0) FROM audit_log", [], |r| r.get(0)).map_err(e)?;
    let dv: i64 = conn.query_row("PRAGMA data_version", [], |r| r.get(0)).map_err(e)?;
    Ok(format!("{max}.{dv}"))
}

// ── field patches ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum Kind {
    Text,
    /// non-empty text
    Required,
    Num,
    /// integer ≥ 1 or null
    Capacity,
    Array,
    Object,
    Enum(&'static [&'static str]),
    /// "" or two uppercase letters
    Country,
    /// "" or a two-letter language code (uk, ru, en…)
    Lang,
    SecretRef,
    Meta,
}

/// A patch value checked against its field kind → the SQL value to store.
fn field_value(name: &str, kind: Kind, v: &Value) -> R<Sql> {
    let text = |v: &Value| -> R<String> {
        match v {
            Value::String(s) if s.len() <= TEXT_MAX => Ok(s.trim().to_string()),
            Value::String(_) => Err(format!("{name} is longer than {TEXT_MAX} characters")),
            Value::Null => Ok(String::new()),
            Value::Number(n) => Ok(n.to_string()),
            _ => Err(format!("{name} must be a string")),
        }
    };
    Ok(match kind {
        Kind::Text => Sql::Text(text(v)?),
        Kind::Required => {
            let s = text(v)?;
            if s.is_empty() {
                return Err(format!("{name} can't be empty"));
            }
            Sql::Text(s)
        }
        Kind::Num => match v {
            Value::Null => Sql::Null,
            Value::String(s) if s.trim().is_empty() => Sql::Null,
            _ => Sql::Real(num(v).ok_or_else(|| format!("{name} must be a number or null"))?),
        },
        Kind::Capacity => match v {
            Value::Null => Sql::Null,
            _ => match num(v) {
                Some(n) if n >= 1.0 && n.fract() == 0.0 => Sql::Integer(n as i64),
                _ => return Err(format!("{name} must be a whole number ≥ 1, or null for unlimited")),
            },
        },
        Kind::Array => match v {
            Value::Array(_) => Sql::Text(v.to_string()),
            _ => return Err(format!("{name} must be an array")),
        },
        Kind::Object => match v {
            Value::Object(_) => Sql::Text(v.to_string()),
            _ => return Err(format!("{name} must be an object")),
        },
        Kind::Enum(allowed) => {
            let s = text(v)?.to_ascii_lowercase();
            if !allowed.contains(&s.as_str()) {
                return Err(format!("{name} must be one of: {}", allowed.join(", ")));
            }
            Sql::Text(s)
        }
        Kind::Country => {
            let s = text(v)?.to_ascii_uppercase();
            if !(s.is_empty() || (s.len() == 2 && s.chars().all(|c| c.is_ascii_uppercase()))) {
                return Err(format!("{name} must be a two-letter country code like DE"));
            }
            Sql::Text(s)
        }
        Kind::Lang => {
            let s = text(v)?.to_ascii_lowercase();
            if !(s.is_empty() || (s.len() == 2 && s.chars().all(|c| c.is_ascii_lowercase()))) {
                return Err(format!("{name} must be a two-letter language code like uk, ru or en"));
            }
            Sql::Text(s)
        }
        Kind::SecretRef => match v {
            Value::Null => Sql::Null,
            Value::String(s) if s.trim().is_empty() => Sql::Null,
            Value::String(s) => {
                let s = s.trim();
                let ok = s.len() <= 64
                    && s.starts_with(|c: char| c.is_ascii_uppercase())
                    && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
                if !ok {
                    return Err(format!(
                        "{name} is the NAME of a vault secret (UPPER_SNAKE_CASE, e.g. PROXY_DE_01) — never the secret itself"
                    ));
                }
                Sql::Text(s.to_string())
            }
            _ => return Err(format!("{name} must be a vault secret name")),
        },
        Kind::Meta => {
            let Value::Object(map) = v else { return Err(format!("{name} must be an object")) };
            check_meta(map)?;
            Sql::Text(v.to_string())
        }
    })
}

/// `meta` holds plain facts (host, port, address, profile id…). Credentials
/// go to the vault; refuse the obvious ways of smuggling them in here.
fn check_meta(map: &Map<String, Value>) -> R<()> {
    const BANNED: &[&str] = &["password", "passwd", "pwd", "secret", "token", "cookie", "otp", "cvv", "cvc", "pin"];
    for (k, v) in map {
        let key = k.to_ascii_lowercase();
        if BANNED.iter().any(|b| key == *b || key.split(['_', '-', '.']).any(|part| part == *b)) {
            return Err(format!(
                "meta.{k} looks like a credential; store it in the vault (Secrets) and put its name in secret_ref"
            ));
        }
        if let Some(s) = v.as_str() {
            // scheme://user:pass@host
            if let Some((_, rest)) = s.split_once("://") {
                if rest.split('/').next().is_some_and(|auth| auth.contains('@') && auth.split('@').next().is_some_and(|u| u.contains(':'))) {
                    return Err(format!("meta.{k} contains a login in the URL; keep credentials in the vault"));
                }
            }
        }
        if v.is_object() || v.is_array() {
            if v.to_string().len() > TEXT_MAX {
                return Err(format!("meta.{k} is too large"));
            }
        }
    }
    Ok(())
}

/// Apply `set` (whitelisted fields) to one row; returns the diff
/// `{field: {from, to}}` (only fields that changed). Bumps `updated_at`.
fn apply_patch(
    conn: &Connection,
    table: &str,
    key_col: &str,
    key: &Sql,
    fields: &[(&str, Kind)],
    set: &Map<String, Value>,
) -> R<Map<String, Value>> {
    let old = query_one(conn, &format!("SELECT * FROM {table} WHERE {key_col} = ?1"), [key])?
        .ok_or_else(|| format!("{table}: no row with {key_col} = {}", sql_display(key)))?;
    let mut diff = Map::new();
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<Sql> = Vec::new();
    for (k, v) in set {
        let Some((name, kind)) = fields.iter().find(|(n, _)| n == k) else {
            let allowed: Vec<&str> = fields.iter().map(|(n, _)| *n).collect();
            return Err(format!("{table}: unknown field \"{k}\" (allowed: {})", allowed.join(", ")));
        };
        let new = field_value(name, *kind, v)?;
        let new_json = match (&new, kind) {
            (Sql::Text(s), Kind::Array | Kind::Object | Kind::Meta) => serde_json::from_str(s).unwrap_or(Value::Null),
            (other, _) => sql_to_json(ValueRef::from(other)),
        };
        let before = old.get(*name).cloned().unwrap_or(Value::Null);
        if !same(&before, &new_json) {
            diff.insert(name.to_string(), json!({ "from": before, "to": new_json }));
            sets.push(format!("{name} = ?{}", vals.len() + 1));
            vals.push(new);
        }
    }
    if !sets.is_empty() {
        sets.push(format!("updated_at = ?{}", vals.len() + 1));
        vals.push(Sql::Text(now_iso()));
        vals.push(key.clone());
        let sql = format!("UPDATE {table} SET {} WHERE {key_col} = ?{}", sets.join(", "), vals.len());
        conn.execute(&sql, params_from_iter(vals.iter())).map_err(e)?;
    }
    Ok(diff)
}

/// Equal as stored: 12 == 12.0, "" == null for text.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Null, Value::String(s)) | (Value::String(s), Value::Null) => s.is_empty(),
        _ => a == b,
    }
}

fn sql_display(v: &Sql) -> String {
    match v {
        Sql::Text(s) => s.clone(),
        Sql::Integer(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

fn set_obj(p: &Value) -> R<Map<String, Value>> {
    match p.get("set") {
        Some(Value::Object(m)) => Ok(m.clone()),
        None => Ok(Map::new()),
        Some(_) => Err("\"set\" must be an object of fields to change".into()),
    }
}

// ── lookups ──────────────────────────────────────────────────────────────────

/// A channel by `channel_id`, `persona` + `platform`, or `channel` as an id
/// or "persona/platform".
fn channel_ref(conn: &Connection, p: &Value) -> R<i64> {
    let by_pair = |persona: &str, platform: &str| -> R<i64> {
        conn.query_row(
            "SELECT id FROM channels WHERE persona = ?1 AND platform = ?2",
            params![persona.trim().trim_start_matches('@'), platform.trim().to_ascii_lowercase()],
            |r| r.get(0),
        )
        .optional()
        .map_err(e)?
        .ok_or_else(|| format!("no channel {persona}/{platform}"))
    };
    let by_id = |id: i64| -> R<i64> {
        conn.query_row("SELECT id FROM channels WHERE id = ?1", [id], |r| r.get(0))
            .optional()
            .map_err(e)?
            .ok_or_else(|| format!("no channel with id {id}"))
    };
    if let Some(id) = p.get("channel_id").and_then(num) {
        return by_id(id as i64);
    }
    if let (Some(persona), Some(platform)) = (opt_str(p, "persona"), opt_str(p, "platform")) {
        return by_pair(&persona, &platform);
    }
    match p.get("channel") {
        Some(v) if num(v).is_some() => by_id(num(v).unwrap_or_default() as i64),
        Some(Value::String(s)) => match s.split_once('/') {
            Some((a, b)) => by_pair(a, b),
            None => Err(format!("channel \"{s}\": use an id or persona/platform")),
        },
        _ => Err("name the channel: channel_id, or persona + platform".into()),
    }
}

fn persona_exists(conn: &Connection, handle: &str) -> R<bool> {
    conn.query_row("SELECT 1 FROM personas WHERE handle = ?1", [handle], |_| Ok(()))
        .optional()
        .map(|o| o.is_some())
        .map_err(e)
}

/// (subject_type, canonical subject_id): personas by handle, channels by id
/// (also "persona/platform"), MCP servers by name.
fn subject_ref(conn: &Connection, p: &Value) -> R<(String, String)> {
    let st = req_str(p, "subject_type")?.to_ascii_lowercase();
    let raw = match p.get("subject_id") {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err("\"subject_id\" is required (persona handle, channel id or persona/platform, MCP name)".into()),
    };
    match st.as_str() {
        "persona" => {
            let h = raw.trim_start_matches('@').to_string();
            if !persona_exists(conn, &h)? {
                return Err(format!("no persona {h}"));
            }
            Ok((st, h))
        }
        "channel" => {
            let id = if raw.contains('/') {
                channel_ref(conn, &json!({ "channel": raw }))?
            } else {
                channel_ref(conn, &json!({ "channel_id": raw.parse::<i64>().map_err(|_| format!("bad channel id: {raw}"))? }))?
            };
            Ok((st, id.to_string()))
        }
        "mcp" => {
            if raw.is_empty() || raw.len() > 128 {
                return Err("MCP subject_id must be the server name".into());
            }
            Ok((st, raw))
        }
        _ => Err(format!("subject_type must be one of: {}", SUBJECT_TYPES.join(", "))),
    }
}

/// Human labels for subjects: "@handle", "@handle · tiktok", MCP name.
struct Labels {
    personas: HashMap<String, Value>,
    channels: HashMap<String, (String, String)>,
}

impl Labels {
    fn load(conn: &Connection) -> R<Labels> {
        let personas = query_json(conn, "SELECT handle, name, emoji, country, city, category FROM personas", [])?
            .into_iter()
            .map(|p| (p["handle"].as_str().unwrap_or_default().to_string(), p))
            .collect();
        let mut channels = HashMap::new();
        let mut stmt = conn.prepare("SELECT id, persona, platform FROM channels").map_err(e)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .map_err(e)?;
        for row in rows {
            let (id, persona, platform) = row.map_err(e)?;
            channels.insert(id.to_string(), (persona, platform));
        }
        Ok(Labels { personas, channels })
    }

    fn describe(&self, subject_type: &str, subject_id: &str) -> Value {
        match subject_type {
            "persona" => json!({ "label": format!("@{subject_id}"), "persona": subject_id, "platform": null }),
            "channel" => match self.channels.get(subject_id) {
                Some((persona, platform)) => {
                    json!({ "label": format!("@{persona} · {platform}"), "persona": persona, "platform": platform })
                }
                None => json!({ "label": format!("channel #{subject_id}"), "persona": null, "platform": null }),
            },
            _ => json!({ "label": subject_id, "persona": null, "platform": null }),
        }
    }

    /// The persona's country for a persona or channel subject.
    fn country(&self, subject_type: &str, subject_id: &str) -> Option<String> {
        let handle = match subject_type {
            "persona" => subject_id.to_string(),
            "channel" => self.channels.get(subject_id)?.0.clone(),
            _ => return None,
        };
        self.personas
            .get(&handle)
            .and_then(|p| p["country"].as_str())
            .filter(|c| !c.is_empty())
            .map(|c| c.to_ascii_uppercase())
    }
}

/// Country code from an item's `geo`: "DE", "DE-BY", "de", "Munich, DE".
pub fn geo_country(geo: &str) -> Option<String> {
    let g = geo.trim();
    if g.is_empty() {
        return None;
    }
    let is_cc = |s: &str| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic());
    if let Some((_, tail)) = g.rsplit_once(',') {
        let tail = tail.trim();
        if is_cc(tail) {
            return Some(tail.to_ascii_uppercase());
        }
    }
    let head: String = g.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    is_cc(&head).then(|| head.to_ascii_uppercase())
}

// ── queries ──────────────────────────────────────────────────────────────────

fn summary(conn: &Connection) -> R<Value> {
    let count = |sql: &str| -> R<i64> { conn.query_row(sql, [], |r| r.get(0)).map_err(e) };
    Ok(json!({
        "personas": count("SELECT count(*) FROM personas")?,
        "channels": count("SELECT count(*) FROM channels")?,
        "tasks": count("SELECT count(*) FROM channel_tasks")?,
        "posts": count("SELECT count(*) FROM posts")?,
        "metrics": count("SELECT count(*) FROM metrics")?,
        "inventory_items": count("SELECT count(*) FROM inventory_items")?,
        "active_bindings": count("SELECT count(*) FROM bindings WHERE until IS NULL")?,
        "audit_rows": count("SELECT count(*) FROM audit_log")?,
        "rev": rev(conn)?,
    }))
}

fn persona_full(conn: &Connection, handle: &str) -> R<Value> {
    let handle = handle.trim_start_matches('@');
    let mut p = query_one(conn, "SELECT * FROM personas WHERE handle = ?1", [handle])?
        .ok_or_else(|| format!("no persona {handle}"))?;
    p["channels"] = Value::Array(query_json(
        conn,
        "SELECT id, platform, stage, role, handle_registered, url, mcp FROM channels WHERE persona = ?1 ORDER BY id",
        [handle],
    )?);
    p["bindings"] = bindings_list(conn, &json!({ "persona": handle }))?;
    Ok(p)
}

fn channels_list(conn: &Connection, p: &Value) -> R<Value> {
    let mut sql = String::from(
        "SELECT id, persona, platform, stage, role, handle_registered, url, plan, kpi_metric, kpi_target, mcp, notes, updated_at FROM channels WHERE 1=1",
    );
    let mut args: Vec<Sql> = Vec::new();
    for key in ["persona", "platform", "stage", "role"] {
        if let Some(v) = opt_str(p, key) {
            args.push(Sql::Text(v));
            sql.push_str(&format!(" AND {key} = ?{}", args.len()));
        }
    }
    sql.push_str(" ORDER BY id");
    query_json(conn, &sql, params_from_iter(args.iter())).map(Value::Array)
}

fn channel_full(conn: &Connection, id: i64) -> R<Value> {
    let mut c = query_one(conn, "SELECT * FROM channels WHERE id = ?1", [id])?.ok_or_else(|| format!("no channel {id}"))?;
    c["tasks"] = Value::Array(query_json(
        conn,
        "SELECT type, cadence, notes FROM channel_tasks WHERE channel_id = ?1 ORDER BY position, id",
        [id],
    )?);
    c["budget"] = Value::Array(query_json(
        conn,
        "SELECT kind, period, currency, proxy, account, content, ads, other FROM budget WHERE channel_id = ?1 ORDER BY kind, period",
        [id],
    )?);
    c["posts"] = Value::Array(query_json(conn, "SELECT * FROM posts WHERE channel_id = ?1 ORDER BY scheduled_at", [id])?);
    c["metrics"] = Value::Array(query_json(
        conn,
        "SELECT date, reach, followers, posts_published, cost, currency FROM metrics WHERE channel_id = ?1 ORDER BY date DESC LIMIT 60",
        [id],
    )?);
    let persona = c["persona"].as_str().unwrap_or_default().to_string();
    c["bindings"] = bindings_list(conn, &json!({ "persona": persona, "channel_id": id }))?;
    Ok(c)
}

/// Active bindings (or all with `include_ended`), with item and subject
/// labels. Filters: item_id, subject_type + subject_id, persona (the
/// persona's own bindings and its channels'), channel_id (with `persona`,
/// only that channel plus the persona's).
fn bindings_list(conn: &Connection, p: &Value) -> R<Value> {
    let labels = Labels::load(conn)?;
    let include_ended = p.get("include_ended").and_then(Value::as_bool).unwrap_or(false);
    let rows = query_json(
        conn,
        "SELECT b.*, i.type AS item_type, i.label AS item_label, i.geo AS item_geo, i.status AS item_status, i.provider AS item_provider, i.meta AS meta
         FROM bindings b JOIN inventory_items i ON i.id = b.item_id ORDER BY b.id",
        [],
    )?;
    let item_id = p.get("item_id").and_then(num).map(|n| n as i64);
    let st = opt_str(p, "subject_type");
    let sid = p.get("subject_id").map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()));
    let persona = opt_str(p, "persona").map(|s| s.trim_start_matches('@').to_string());
    let channel = p.get("channel_id").and_then(num).map(|n| (n as i64).to_string());
    let out: Vec<Value> = rows
        .into_iter()
        .filter(|b| include_ended || b["until"].is_null())
        .filter(|b| item_id.is_none_or(|i| b["item_id"].as_i64() == Some(i)))
        .filter(|b| st.as_deref().is_none_or(|s| b["subject_type"] == s))
        .filter(|b| sid.as_deref().is_none_or(|s| b["subject_id"] == s))
        .filter(|b| {
            let Some(handle) = persona.as_deref() else { return true };
            let (t, id) = (b["subject_type"].as_str().unwrap_or(""), b["subject_id"].as_str().unwrap_or(""));
            match t {
                "persona" => id == handle,
                "channel" => {
                    labels.channels.get(id).is_some_and(|(pp, _)| pp == handle)
                        && channel.as_deref().is_none_or(|c| c == id)
                }
                _ => false,
            }
        })
        .map(|mut b| {
            let d = labels.describe(b["subject_type"].as_str().unwrap_or(""), b["subject_id"].as_str().unwrap_or(""));
            b["subject_label"] = d["label"].clone();
            b["persona"] = d["persona"].clone();
            b["platform"] = d["platform"].clone();
            b
        })
        .collect();
    Ok(Value::Array(out))
}

fn active_subjects(conn: &Connection, item_id: i64) -> R<HashSet<(String, String)>> {
    let mut stmt = conn
        .prepare("SELECT subject_type, subject_id FROM bindings WHERE item_id = ?1 AND until IS NULL")
        .map_err(e)?;
    let rows = stmt.query_map([item_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(e)?;
    rows.collect::<Result<_, _>>().map_err(e)
}

/// Items with who uses them and whether their vault secret exists.
/// Filters: type, status, geo, provider, q (label / provider / geo / notes / meta).
fn inventory(conn: &Connection, p: &Value, secrets: &HashSet<String>) -> R<Value> {
    let items = query_json(conn, "SELECT * FROM inventory_items ORDER BY type, label, id", [])?;
    let bindings = bindings_list(conn, &json!({}))?;
    let mut by_item: HashMap<i64, Vec<Value>> = HashMap::new();
    for b in bindings.as_array().into_iter().flatten() {
        by_item.entry(b["item_id"].as_i64().unwrap_or_default()).or_default().push(json!({
            "id": b["id"], "subject_type": b["subject_type"], "subject_id": b["subject_id"],
            "label": b["subject_label"], "persona": b["persona"], "platform": b["platform"],
            "purpose": b["purpose"], "since": b["since"],
        }));
    }
    let q = opt_str(p, "q").map(|s| s.to_lowercase());
    let out: Vec<Value> = items
        .into_iter()
        .filter(|i| ["type", "status", "provider"].iter().all(|k| opt_str(p, k).is_none_or(|v| i[*k] == v.as_str())))
        .filter(|i| opt_str(p, "geo").is_none_or(|g| {
            let geo = i["geo"].as_str().unwrap_or("");
            geo.eq_ignore_ascii_case(&g) || geo_country(geo) == geo_country(&g)
        }))
        .filter(|i| {
            q.as_deref().is_none_or(|q| {
                ["label", "provider", "geo", "notes", "secret_ref"]
                    .iter()
                    .any(|k| i[*k].as_str().is_some_and(|s| s.to_lowercase().contains(q)))
                    || i["meta"].to_string().to_lowercase().contains(q)
            })
        })
        .map(|mut i| {
            let id = i["id"].as_i64().unwrap_or_default();
            let bs = by_item.remove(&id).unwrap_or_default();
            let subjects: HashSet<String> =
                bs.iter().map(|b| format!("{}:{}", b["subject_type"].as_str().unwrap_or(""), b["subject_id"].as_str().unwrap_or(""))).collect();
            i["used"] = json!(subjects.len());
            i["over_capacity"] = json!(i["capacity"].as_i64().is_some_and(|c| subjects.len() as i64 > c));
            i["secret"] = match i["secret_ref"].as_str() {
                Some(name) if secrets.contains(name) => json!("ok"),
                Some(_) => json!("missing"),
                None => Value::Null,
            };
            i["bindings"] = Value::Array(bs);
            i
        })
        .collect();
    Ok(Value::Array(out))
}

/// Per type: total / in use / free / reserved / broken / retired, shared
/// slots used of the capacity, monthly cost per currency (retired excluded).
fn inventory_summary(conn: &Connection) -> R<Value> {
    let items = inventory(conn, &json!({}), &HashSet::new())?;
    let mut by: BTreeMap<String, Value> = BTreeMap::new();
    for i in items.as_array().into_iter().flatten() {
        let t = i["type"].as_str().unwrap_or("other").to_string();
        let s = by.entry(t.clone()).or_insert_with(|| {
            json!({ "type": t, "total": 0, "in_use": 0, "free": 0, "reserved": 0, "broken": 0, "retired": 0,
                    "slots_used": 0, "slots_total": 0, "unlimited": 0, "over_capacity": 0, "cost_month": {} })
        });
        let used = i["used"].as_i64().unwrap_or(0);
        let status = i["status"].as_str().unwrap_or("active");
        let inc = |s: &mut Value, k: &str, n: i64| s[k] = json!(s[k].as_i64().unwrap_or(0) + n);
        inc(s, "total", 1);
        if used > 0 {
            inc(s, "in_use", 1);
        }
        match status {
            "active" if used == 0 => inc(s, "free", 1),
            "reserved" | "broken" | "retired" => inc(s, status, 1),
            _ => {}
        }
        if status != "retired" {
            match i["capacity"].as_i64() {
                Some(cap) => {
                    inc(s, "slots_total", cap);
                    inc(s, "slots_used", used.min(cap));
                }
                None => inc(s, "unlimited", 1),
            }
            if let Some(cost) = i["cost_month"].as_f64() {
                let cur = i["currency"].as_str().unwrap_or("EUR").to_string();
                let prev = s["cost_month"][&cur].as_f64().unwrap_or(0.0);
                s["cost_month"][cur] = json!(((prev + cost) * 100.0).round() / 100.0);
            }
        }
        if i["over_capacity"].as_bool() == Some(true) {
            inc(s, "over_capacity", 1);
        }
    }
    Ok(Value::Array(by.into_values().collect()))
}

/// What's missing per requirement, plus warnings: geo mismatch (a bound
/// item's geo country ≠ the persona's country), capacity overflow, and
/// bindings on broken/retired items (those don't count as covering).
fn gaps(conn: &Connection) -> R<Value> {
    let labels = Labels::load(conn)?;
    let reqs = query_json(conn, "SELECT * FROM inventory_requirements ORDER BY id", [])?;
    let personas = query_json(conn, "SELECT handle, name, emoji, country, city, category FROM personas ORDER BY handle", [])?;
    let channels = query_json(conn, "SELECT id, persona, platform, stage FROM channels ORDER BY id", [])?;
    let binds = query_json(
        conn,
        "SELECT b.id, b.item_id, b.subject_type, b.subject_id, b.purpose, i.type, i.status, i.geo, i.label, i.capacity
         FROM bindings b JOIN inventory_items i ON i.id = b.item_id WHERE b.until IS NULL",
        [],
    )?;
    // (subject_type, subject_id, type, purpose) → usable bindings
    let mut have: HashMap<(String, String, String, String), i64> = HashMap::new();
    let mut warnings: Vec<Value> = Vec::new();
    let mut subjects_per_item: HashMap<i64, HashSet<String>> = HashMap::new();
    for b in &binds {
        let s = |k: &str| b[k].as_str().unwrap_or("").to_string();
        let (st, sid) = (s("subject_type"), s("subject_id"));
        let item = b["item_id"].as_i64().unwrap_or_default();
        subjects_per_item.entry(item).or_default().insert(format!("{st}:{sid}"));
        let who = labels.describe(&st, &sid);
        if ["broken", "retired"].contains(&s("status").as_str()) {
            warnings.push(json!({
                "kind": "bad_status", "item_id": item, "item_label": b["label"], "binding_id": b["id"],
                "subject_type": st, "subject_id": sid, "subject_label": who["label"],
                "message": format!("{} is bound to {} but is {}", s("label"), who["label"].as_str().unwrap_or(""), s("status")),
            }));
            continue;
        }
        *have.entry((st.clone(), sid.clone(), s("type"), s("purpose"))).or_default() += 1;
        if let (Some(item_cc), Some(home)) = (geo_country(&s("geo")), labels.country(&st, &sid)) {
            if item_cc != home {
                warnings.push(json!({
                    "kind": "geo_mismatch", "item_id": item, "item_label": b["label"], "binding_id": b["id"],
                    "subject_type": st, "subject_id": sid, "subject_label": who["label"],
                    "item_country": item_cc, "persona_country": home,
                    "message": format!("{} exits in {item_cc}, but {} lives in {home}", s("label"), who["label"].as_str().unwrap_or("")),
                }));
            }
        }
    }
    let mut caps: Vec<(i64, i64, String)> = Vec::new();
    for b in &binds {
        if let (Some(item), Some(cap)) = (b["item_id"].as_i64(), b["capacity"].as_i64()) {
            if !caps.iter().any(|(i, ..)| *i == item) {
                caps.push((item, cap, b["label"].as_str().unwrap_or("").to_string()));
            }
        }
    }
    for (item, cap, label) in caps {
        let used = subjects_per_item.get(&item).map(|s| s.len() as i64).unwrap_or(0);
        if used > cap {
            warnings.push(json!({
                "kind": "capacity", "item_id": item, "item_label": label, "used": used, "capacity": cap,
                "message": format!("{label} is shared by {used} subjects, capacity {cap}"),
            }));
        }
    }

    let mut by_subject: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut out = Vec::new();
    for r in &reqs {
        let s = |k: &str| r[k].as_str().unwrap_or("").to_string();
        let (typ, purpose, scope, platform) = (s("type"), s("purpose"), s("scope"), s("platform"));
        let need = r["count"].as_i64().unwrap_or(1);
        let subjects: Vec<(String, String)> = if scope == "persona" {
            personas.iter().map(|p| ("persona".to_string(), p["handle"].as_str().unwrap_or("").to_string())).collect()
        } else {
            channels
                .iter()
                .filter(|c| platform == "*" || c["platform"] == platform.as_str())
                .map(|c| ("channel".to_string(), c["id"].as_i64().unwrap_or_default().to_string()))
                .collect()
        };
        let total = subjects.len();
        let mut missing = Vec::new();
        for (st, sid) in subjects {
            let got = have.get(&(st.clone(), sid.clone(), typ.clone(), purpose.clone())).copied().unwrap_or(0);
            if got >= need {
                continue;
            }
            let d = labels.describe(&st, &sid);
            let handle = d["persona"].as_str().unwrap_or("").to_string();
            let p = labels.personas.get(&handle);
            let field = |k: &str| p.and_then(|p| p[k].as_str()).unwrap_or("").to_string();
            by_subject.entry(format!("{st}:{sid}")).or_default().push(typ.clone());
            missing.push(json!({
                "subject_type": st, "subject_id": sid, "label": d["label"], "persona": handle, "platform": d["platform"],
                "country": field("country"), "city": field("city"), "category": field("category"), "emoji": field("emoji"),
                "have": got, "need": need,
            }));
        }
        let mut row = r.clone();
        row["total"] = json!(total);
        row["missing_count"] = json!(missing.len());
        row["missing"] = Value::Array(missing);
        out.push(row);
    }
    Ok(json!({ "requirements": out, "warnings": warnings, "by_subject": by_subject }))
}

/// `gaps` narrowed for callers: `type`, `scope`, `requirement_id`, and at
/// most `limit` missing subjects per requirement (default: all).
fn trim_gaps(mut g: Value, p: &Value) -> Value {
    let limit = p.get("limit").and_then(Value::as_u64).map(|n| n as usize);
    if let Some(reqs) = g["requirements"].as_array_mut() {
        reqs.retain(|r| {
            opt_str(p, "type").is_none_or(|t| r["type"] == t.as_str())
                && opt_str(p, "scope").is_none_or(|s| r["scope"] == s.as_str())
                && p.get("requirement_id").and_then(num).is_none_or(|id| r["id"].as_i64() == Some(id as i64))
        });
        if let Some(limit) = limit {
            for r in reqs.iter_mut() {
                if let Some(m) = r["missing"].as_array_mut() {
                    m.truncate(limit);
                }
            }
        }
    }
    if p.get("by_subject").and_then(Value::as_bool) == Some(false) {
        if let Some(o) = g.as_object_mut() {
            o.remove("by_subject");
        }
    }
    g
}

fn audit_list(conn: &Connection, p: &Value) -> R<Value> {
    let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(100).min(1000) as i64;
    let mut sql = String::from("SELECT * FROM audit_log WHERE 1=1");
    let mut args: Vec<Sql> = Vec::new();
    for key in ["entity", "entity_id", "actor"] {
        if let Some(v) = opt_str(p, key) {
            args.push(Sql::Text(v));
            sql.push_str(&format!(" AND {key} = ?{}", args.len()));
        }
    }
    args.push(Sql::Integer(limit));
    sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", args.len()));
    query_json(conn, &sql, params_from_iter(args.iter())).map(Value::Array)
}

// ── snapshot / export ────────────────────────────────────────────────────────

/// Everything the Channels plugin shows, in the shape of the old files
/// (channels.json rows, posts.json, metrics rows) plus inventory. Also what
/// `export/` holds.
fn snapshot(conn: &Connection, secrets: &HashSet<String>) -> R<Value> {
    let month = this_month();
    let mut platforms_of: HashMap<String, Vec<String>> = HashMap::new();
    let channels_raw = query_json(conn, "SELECT * FROM channels ORDER BY id", [])?;
    for c in &channels_raw {
        platforms_of
            .entry(c["persona"].as_str().unwrap_or("").to_string())
            .or_default()
            .push(c["platform"].as_str().unwrap_or("").to_string());
    }
    let personas: Vec<Value> = query_json(conn, "SELECT * FROM personas ORDER BY handle", [])?
        .into_iter()
        .map(|mut p| {
            let handle = p["handle"].as_str().unwrap_or("").to_string();
            let refs = p.as_object_mut().and_then(|o| o.remove("refs")).unwrap_or(json!([]));
            p["references"] = refs;
            p["platforms"] = json!(platforms_of.remove(&handle).unwrap_or_default());
            p
        })
        .collect();

    let mut tasks: HashMap<i64, Vec<Value>> = HashMap::new();
    for t in query_json(conn, "SELECT channel_id, type, cadence, notes FROM channel_tasks ORDER BY channel_id, position, id", [])? {
        tasks.entry(t["channel_id"].as_i64().unwrap_or_default()).or_default().push(json!({
            "type": t["type"], "cadence": t["cadence"], "notes": t["notes"],
        }));
    }
    // planned: the 'monthly' row; spent: this month's row, else the latest.
    let mut planned: HashMap<i64, Value> = HashMap::new();
    let mut spent: HashMap<i64, (String, Value)> = HashMap::new();
    for b in query_json(conn, "SELECT * FROM budget ORDER BY channel_id, kind, period", [])? {
        let id = b["channel_id"].as_i64().unwrap_or_default();
        let money = json!({
            "currency": b["currency"], "proxy": b["proxy"], "account": b["account"],
            "content": b["content"], "ads": b["ads"], "other": b["other"],
        });
        let period = b["period"].as_str().unwrap_or("").to_string();
        if b["kind"] == "planned" {
            if period == "monthly" || !planned.contains_key(&id) {
                planned.insert(id, money);
            }
        } else {
            let keep = match spent.get(&id) {
                Some((p, _)) => *p != month && (period == month || period > *p),
                None => true,
            };
            if keep {
                spent.insert(id, (period, money));
            }
        }
    }

    let inventory = inventory(conn, &json!({}), secrets)?;
    let items: HashMap<i64, &Value> =
        inventory.as_array().into_iter().flatten().map(|i| (i["id"].as_i64().unwrap_or_default(), i)).collect();
    // subject → [(item, purpose)]
    let mut bound: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for i in items.values() {
        for b in i["bindings"].as_array().into_iter().flatten() {
            bound
                .entry(format!("{}:{}", b["subject_type"].as_str().unwrap_or(""), b["subject_id"].as_str().unwrap_or("")))
                .or_default()
                .push((i["id"].as_i64().unwrap_or_default(), b["purpose"].as_str().unwrap_or("").to_string()));
        }
    }
    let inv_of = |key: &str, via: &str| -> Vec<Value> {
        let mut v: Vec<Value> = bound
            .get(key)
            .into_iter()
            .flatten()
            .filter_map(|(id, purpose)| {
                let i = items.get(id)?;
                Some(json!({
                    "item_id": id, "type": i["type"], "label": i["label"], "provider": i["provider"], "geo": i["geo"],
                    "status": i["status"], "ip": i["meta"]["ip"], "purpose": purpose, "via": via,
                }))
            })
            .collect();
        v.sort_by_key(|x| (x["type"].as_str().unwrap_or("").to_string(), x["item_id"].as_i64()));
        v
    };

    let channels: Vec<Value> = channels_raw
        .iter()
        .map(|c| {
            let id = c["id"].as_i64().unwrap_or_default();
            let persona = c["persona"].as_str().unwrap_or("");
            // Channel's own items first, then what it inherits from its persona.
            let mut inv = inv_of(&format!("channel:{id}"), "channel");
            inv.extend(inv_of(&format!("persona:{persona}"), "persona"));
            let pick = |t: &str| inv.iter().find(|x| x["type"] == t && !["broken", "retired"].contains(&x["status"].as_str().unwrap_or("")));
            let proxy = pick("proxy");
            let device = pick("device_profile");
            let s = |k: &str| c[k].clone();
            json!({
                "id": id, "persona": s("persona"), "platform": s("platform"), "handle": s("handle_registered"),
                "stage": s("stage"), "role": s("role"), "plan": s("plan"),
                "tasks": tasks.remove(&id).unwrap_or_default(),
                "kpi_metric": s("kpi_metric"), "kpi_target": s("kpi_target"),
                "budget": planned.remove(&id).unwrap_or(Value::Null),
                "spent": spent.get(&id).map(|(_, m)| m.clone()).unwrap_or(Value::Null),
                "spent_period": spent.get(&id).map(|(p, _)| json!(p)).unwrap_or(Value::Null),
                "mcp": s("mcp"),
                "network": {
                    "egress": proxy.map(|p| {
                        let prov = p["provider"].as_str().unwrap_or("");
                        let label = p["label"].as_str().unwrap_or("");
                        if prov.is_empty() { label.to_string() } else { format!("{label} · {prov}") }
                    }).unwrap_or_default(),
                    "ip": proxy.and_then(|p| p["ip"].as_str()).unwrap_or(""),
                    "geo": proxy.and_then(|p| p["geo"].as_str()).unwrap_or(""),
                    "device_profile": device.and_then(|d| d["label"].as_str()).unwrap_or(""),
                    "url": s("url"),
                },
                "inventory": inv,
                "notes": s("notes"), "created": s("created_at"), "updated": s("updated_at"),
            })
        })
        .collect();

    let posts = with_reviews(conn, query_json(conn, &format!("{POST_SELECT} ORDER BY p.scheduled_at, p.id"), [])?, true)?;
    let metrics = query_json(
        conn,
        "SELECT m.date, c.persona, c.platform, m.reach, m.cost, m.currency, m.followers, m.posts_published
         FROM metrics m JOIN channels c ON c.id = m.channel_id ORDER BY m.date, c.persona, c.platform",
        [],
    )?;
    let bindings = bindings_list(conn, &json!({ "include_ended": true }))?;
    let mut gaps = gaps(conn)?;
    gaps["summary"] = inventory_summary(conn)?;
    Ok(json!({
        "rev": rev(conn)?,
        "personas": personas,
        "channels": channels,
        "posts": posts,
        "metrics": metrics,
        "inventory": inventory,
        "bindings": bindings,
        "gaps": gaps,
    }))
}

// ── mutations ────────────────────────────────────────────────────────────────

const PERSONA_FIELDS: &[(&str, Kind)] = &[
    ("name", Kind::Text),
    ("emoji", Kind::Text),
    ("niche", Kind::Text),
    ("category", Kind::Text),
    ("country", Kind::Country),
    ("city", Kind::Text),
    ("languages", Kind::Array),
    ("target_audience", Kind::Text),
    ("hypothesis", Kind::Text),
    ("personality", Kind::Object),
    ("refs", Kind::Array),
    ("soul_path", Kind::Text),
];

fn persona_update(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let handle = req_str(p, "handle")?.trim_start_matches('@').to_string();
    let mut set = set_obj(p)?;
    if let Some(v) = set.remove("references") {
        set.insert("refs".into(), v);
    }
    let diff = apply_patch(conn, "personas", "handle", &Sql::Text(handle.clone()), PERSONA_FIELDS, &set)?;
    if !diff.is_empty() {
        audit(conn, actor, "update", "persona", &handle, &Value::Object(diff.clone()))?;
    }
    Ok(json!({ "handle": handle, "changed": diff }))
}

const CHANNEL_FIELDS: &[(&str, Kind)] = &[
    ("handle_registered", Kind::Text),
    ("url", Kind::Text),
    ("stage", Kind::Enum(STAGES)),
    ("role", Kind::Enum(ROLES)),
    ("plan", Kind::Text),
    ("kpi_metric", Kind::Text),
    ("kpi_target", Kind::Num),
    ("mcp", Kind::Text),
    ("notes", Kind::Text),
];

/// `set` takes the channel's columns plus `tasks` (full replacement),
/// `budget` (planned per month) and `spent` (`period` defaults to this month).
fn channel_update(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = channel_ref(conn, p)?;
    let mut set = set_obj(p)?;
    if let Some(v) = set.remove("handle") {
        set.insert("handle_registered".into(), v);
    }
    let tasks = set.remove("tasks");
    let planned = set.remove("budget");
    let spent = set.remove("spent");
    let mut diff = apply_patch(conn, "channels", "id", &Sql::Integer(id), CHANNEL_FIELDS, &set)?;
    if let Some(tasks) = tasks {
        let before = query_json(conn, "SELECT type, cadence, notes FROM channel_tasks WHERE channel_id = ?1 ORDER BY position, id", [id])?;
        let after = write_tasks(conn, id, &tasks)?;
        if Value::Array(before.clone()) != after {
            diff.insert("tasks".into(), json!({ "from": before, "to": after }));
        }
    }
    if let Some(b) = planned {
        if let Some(d) = write_budget(conn, id, "planned", "monthly", &b)? {
            diff.insert("budget".into(), d);
        }
    }
    if let Some(b) = spent {
        let period = b.get("period").and_then(Value::as_str).map(str::to_string).unwrap_or_else(this_month);
        if !(period.len() == 7 && period.as_bytes()[4] == b'-' && period.chars().filter(char::is_ascii_digit).count() == 6) {
            return Err(format!("spent.period must be YYYY-MM, got {period}"));
        }
        if let Some(d) = write_budget(conn, id, "spent", &period, &b)? {
            diff.insert(format!("spent:{period}"), d);
        }
    }
    if !diff.is_empty() {
        conn.execute("UPDATE channels SET updated_at = ?1 WHERE id = ?2", params![now_iso(), id]).map_err(e)?;
        audit(conn, actor, "update", "channel", &id.to_string(), &Value::Object(diff.clone()))?;
    }
    Ok(json!({ "channel_id": id, "changed": diff }))
}

fn write_tasks(conn: &Connection, id: i64, tasks: &Value) -> R<Value> {
    let list = tasks.as_array().ok_or("tasks must be an array of {type, cadence, notes}")?;
    conn.execute("DELETE FROM channel_tasks WHERE channel_id = ?1", [id]).map_err(e)?;
    let mut out = Vec::new();
    for (i, t) in list.iter().enumerate() {
        let typ = t.get("type").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).ok_or("each task needs a type")?;
        let cadence = t.get("cadence").and_then(Value::as_str).unwrap_or("").trim();
        let notes = t.get("notes").and_then(Value::as_str).unwrap_or("").trim();
        conn.execute(
            "INSERT INTO channel_tasks (channel_id, position, type, cadence, notes) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, i as i64, typ.to_ascii_lowercase(), cadence, notes],
        )
        .map_err(e)?;
        out.push(json!({ "type": typ.to_ascii_lowercase(), "cadence": cadence, "notes": notes }));
    }
    Ok(Value::Array(out))
}

/// Upsert one budget row; keys not given keep their value. Returns the diff.
fn write_budget(conn: &Connection, id: i64, kind: &str, period: &str, b: &Value) -> R<Option<Value>> {
    let Value::Object(map) = b else { return Err(format!("{kind} must be an object like {{currency, proxy, account, content, ads, other}}")) };
    for k in map.keys() {
        if !(BUDGET_KEYS.contains(&k.as_str()) || k == "currency" || k == "period") {
            return Err(format!("{kind}: unknown key {k} (use currency, {})", BUDGET_KEYS.join(", ")));
        }
    }
    let old = query_one(
        conn,
        "SELECT currency, proxy, account, content, ads, other FROM budget WHERE channel_id = ?1 AND kind = ?2 AND period = ?3",
        params![id, kind, period],
    )?;
    let base = old.clone().unwrap_or_else(|| json!({ "currency": "EUR", "proxy": 0, "account": 0, "content": 0, "ads": 0, "other": 0 }));
    let mut new = base.clone();
    if let Some(c) = map.get("currency").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        new["currency"] = json!(c.to_ascii_uppercase());
    }
    for k in BUDGET_KEYS {
        if let Some(v) = map.get(k) {
            new[k] = json!(num(v).ok_or_else(|| format!("{kind}.{k} must be a number"))?);
        }
    }
    let changed = old.is_none() || BUDGET_KEYS.iter().any(|k| !same(&base[*k], &new[*k])) || base["currency"] != new["currency"];
    if !changed {
        return Ok(None);
    }
    let f = |k: &str| new[k].as_f64().unwrap_or(0.0);
    conn.execute(
        "INSERT INTO budget (channel_id, kind, period, currency, proxy, account, content, ads, other, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (channel_id, kind, period) DO UPDATE SET currency = excluded.currency, proxy = excluded.proxy,
           account = excluded.account, content = excluded.content, ads = excluded.ads, other = excluded.other,
           updated_at = excluded.updated_at",
        params![id, kind, period, new["currency"].as_str().unwrap_or("EUR"), f("proxy"), f("account"), f("content"), f("ads"), f("other"), now_iso()],
    )
    .map_err(e)?;
    Ok(Some(json!({ "from": old, "to": new })))
}

const ITEM_FIELDS: &[(&str, Kind)] = &[
    ("type", Kind::Enum(ITEM_TYPES)),
    ("label", Kind::Required),
    ("provider", Kind::Text),
    ("geo", Kind::Text),
    ("cost_month", Kind::Num),
    ("currency", Kind::Text),
    ("status", Kind::Enum(ITEM_STATUSES)),
    ("capacity", Kind::Capacity),
    ("secret_ref", Kind::SecretRef),
    ("meta", Kind::Meta),
    ("notes", Kind::Text),
];

fn inventory_add(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let Value::Object(fields) = p.get("item").unwrap_or(p) else { return Err("item must be an object".into()) };
    if !fields.contains_key("type") || !fields.contains_key("label") {
        return Err(format!("inventory_add needs type ({}) and label", ITEM_TYPES.join(", ")));
    }
    let now = now_iso();
    conn.execute(
        "INSERT INTO inventory_items (type, label, created_at, updated_at) VALUES ('other', '-', ?1, ?1)",
        [&now],
    )
    .map_err(e)?;
    let id = conn.last_insert_rowid();
    let diff = apply_patch(conn, "inventory_items", "id", &Sql::Integer(id), ITEM_FIELDS, fields)?;
    audit(conn, actor, "create", "inventory_item", &id.to_string(), &Value::Object(diff))?;
    query_one(conn, "SELECT * FROM inventory_items WHERE id = ?1", [id])?.ok_or_else(|| "item vanished".into())
}

fn item_id(p: &Value, key: &str) -> R<i64> {
    p.get(key).and_then(num).map(|n| n as i64).ok_or_else(|| format!("\"{key}\" (inventory item id) is required"))
}

fn inventory_update(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = item_id(p, "id").or_else(|_| item_id(p, "item_id"))?;
    let set = set_obj(p)?;
    let diff = apply_patch(conn, "inventory_items", "id", &Sql::Integer(id), ITEM_FIELDS, &set)?;
    if !diff.is_empty() {
        audit(conn, actor, "update", "inventory_item", &id.to_string(), &Value::Object(diff.clone()))?;
    }
    let used = active_subjects(conn, id)?.len() as i64;
    let cap: Option<i64> = conn.query_row("SELECT capacity FROM inventory_items WHERE id = ?1", [id], |r| r.get(0)).map_err(e)?;
    let mut out = json!({ "id": id, "changed": diff });
    if cap.is_some_and(|c| used > c) {
        out["warning"] = json!(format!("item #{id} is now shared by {used} subjects, over its capacity {}", cap.unwrap_or(0)));
    }
    Ok(out)
}

fn inventory_delete(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = item_id(p, "id").or_else(|_| item_id(p, "item_id"))?;
    let item = query_one(conn, "SELECT * FROM inventory_items WHERE id = ?1", [id])?.ok_or_else(|| format!("no inventory item {id}"))?;
    let active = active_subjects(conn, id)?.len();
    if active > 0 && p.get("force").and_then(Value::as_bool) != Some(true) {
        return Err(format!("item #{id} is bound to {active} subject(s); unbind first, set status retired, or pass force"));
    }
    conn.execute("DELETE FROM inventory_items WHERE id = ?1", [id]).map_err(e)?;
    audit(conn, actor, "delete", "inventory_item", &id.to_string(), &json!({ "item": item }))?;
    Ok(json!({ "deleted": id }))
}

fn bind(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let item = item_id(p, "item_id")?;
    let (typ, status, label, cap): (String, String, String, Option<i64>) = conn
        .query_row("SELECT type, status, label, capacity FROM inventory_items WHERE id = ?1", [item], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()
        .map_err(e)?
        .ok_or_else(|| format!("no inventory item {item}"))?;
    if status == "broken" || status == "retired" {
        return Err(format!("{label} (#{item}) is {status}; set it active before binding"));
    }
    let (st, sid) = subject_ref(conn, p)?;
    if typ == "account" {
        // An account is one platform login: it belongs to one channel of that platform.
        if st != "channel" {
            return Err(format!("{label} is a platform account: bind it to a channel (persona/platform), not a {st}"));
        }
        let want: Option<String> = conn
            .query_row("SELECT json_extract(meta, '$.platform') FROM inventory_items WHERE id = ?1 AND json_valid(meta)", [item], |r| r.get(0))
            .optional()
            .map_err(e)?
            .flatten();
        let have: String = conn.query_row("SELECT platform FROM channels WHERE id = ?1", [sid.parse::<i64>().unwrap_or(0)], |r| r.get(0)).map_err(e)?;
        if let Some(want) = want.filter(|w| !w.is_empty()) {
            if want.to_ascii_lowercase() != have {
                return Err(format!("{label} is a {want} account; it can't log in to a {have} channel"));
            }
        }
    }
    let purpose = opt_str(p, "purpose").map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| default_purpose(&typ).to_string());
    if !PURPOSES.contains(&purpose.as_str()) {
        return Err(format!("purpose must be one of: {}", PURPOSES.join(", ")));
    }
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM bindings WHERE item_id = ?1 AND subject_type = ?2 AND subject_id = ?3 AND purpose = ?4 AND until IS NULL",
            params![item, st, sid, purpose],
            |r| r.get(0),
        )
        .optional()
        .map_err(e)?;
    if let Some(id) = existing {
        return Ok(json!({ "binding_id": id, "already": true }));
    }
    let subjects = active_subjects(conn, item)?;
    let new_subject = !subjects.contains(&(st.clone(), sid.clone()));
    if let Some(cap) = cap {
        if new_subject && subjects.len() as i64 >= cap {
            return Err(format!(
                "capacity full: {label} (#{item}) is already shared by {} of {cap}; raise its capacity or unbind someone first",
                subjects.len()
            ));
        }
    }
    let notes = opt_str(p, "notes").unwrap_or_default();
    conn.execute(
        "INSERT INTO bindings (item_id, subject_type, subject_id, purpose, since, notes) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![item, st, sid, purpose, now_iso(), notes],
    )
    .map_err(e)?;
    let id = conn.last_insert_rowid();
    audit(
        conn,
        actor,
        "bind",
        "binding",
        &id.to_string(),
        &json!({ "item_id": item, "item_label": label, "subject_type": st, "subject_id": sid, "purpose": purpose }),
    )?;
    Ok(json!({ "binding_id": id, "item_id": item, "subject_type": st, "subject_id": sid, "purpose": purpose }))
}

/// End bindings: one by `binding_id`, or all active ones of `item_id` for a
/// subject (optionally one `purpose`). History stays (until = now).
fn unbind(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let ids: Vec<i64> = if let Some(id) = p.get("binding_id").and_then(num) {
        vec![id as i64]
    } else {
        let item = item_id(p, "item_id")?;
        let (st, sid) = subject_ref(conn, p)?;
        let purpose = opt_str(p, "purpose");
        let mut stmt = conn
            .prepare(
                "SELECT id FROM bindings WHERE item_id = ?1 AND subject_type = ?2 AND subject_id = ?3 AND until IS NULL AND (?4 IS NULL OR purpose = ?4)",
            )
            .map_err(e)?;
        let rows = stmt.query_map(params![item, st, sid, purpose], |r| r.get(0)).map_err(e)?;
        rows.collect::<Result<_, _>>().map_err(e)?
    };
    let now = now_iso();
    let mut ended = Vec::new();
    for id in ids {
        let row = query_one(conn, "SELECT * FROM bindings WHERE id = ?1 AND until IS NULL", [id])?;
        let Some(row) = row else { continue };
        conn.execute("UPDATE bindings SET until = ?1 WHERE id = ?2", params![now, id]).map_err(e)?;
        audit(conn, actor, "unbind", "binding", &id.to_string(), &row)?;
        ended.push(id);
    }
    if ended.is_empty() {
        return Err("no active binding matched".into());
    }
    Ok(json!({ "ended": ended }))
}

/// Cover one requirement's missing subjects from free items of its type:
/// status active (then reserved), room under capacity, and (for geo_match) the persona's
/// country. Spreads over the least-used items. `limit` caps new bindings.
fn autobind(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let req_id = p.get("requirement_id").and_then(num).map(|n| n as i64).ok_or("\"requirement_id\" is required")?;
    let limit = p.get("limit").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(usize::MAX);
    let g = trim_gaps(gaps(conn)?, &json!({ "requirement_id": req_id }));
    let req = g["requirements"].get(0).cloned().ok_or_else(|| format!("no requirement {req_id}"))?;
    let typ = req["type"].as_str().unwrap_or("").to_string();
    let purpose = req["purpose"].as_str().unwrap_or("").to_string();
    let geo_match = req["geo_match"].as_i64() == Some(1);
    // candidates: (id, geo country, capacity, used, account platform)
    let mut cands: Vec<(i64, Option<String>, Option<i64>, i64, Option<String>)> = Vec::new();
    for i in query_json(conn, "SELECT id, geo, capacity, meta FROM inventory_items WHERE type = ?1 AND status IN ('active', 'reserved') ORDER BY status = 'reserved', id", [&typ])? {
        let id = i["id"].as_i64().unwrap_or_default();
        let used = active_subjects(conn, id)?.len() as i64;
        let platform = i["meta"]["platform"].as_str().filter(|p| !p.is_empty()).map(str::to_ascii_lowercase);
        cands.push((id, geo_country(i["geo"].as_str().unwrap_or("")), i["capacity"].as_i64(), used, platform));
    }
    let mut bound = Vec::new();
    let mut unassigned = Vec::new();
    for m in req["missing"].as_array().into_iter().flatten() {
        let (st, sid) = (m["subject_type"].as_str().unwrap_or(""), m["subject_id"].as_str().unwrap_or(""));
        if bound.len() >= limit {
            unassigned.push(m["label"].clone());
            continue;
        }
        let want = m["country"].as_str().filter(|c| !c.is_empty()).map(str::to_ascii_uppercase);
        let on = m["platform"].as_str().map(str::to_string);
        let mut need = m["need"].as_i64().unwrap_or(1) - m["have"].as_i64().unwrap_or(0);
        while need > 0 {
            let pick = cands
                .iter_mut()
                .filter(|(_, cc, cap, used, plat)| {
                    cap.is_none_or(|c| *used < c)
                        && (!geo_match || want.is_none() || *cc == want)
                        && (typ != "account" || plat.is_none() || *plat == on)
                })
                .min_by_key(|(_, _, _, used, _)| *used);
            let Some(c) = pick else { break };
            let r = bind(conn, actor, &json!({ "item_id": c.0, "subject_type": st, "subject_id": sid, "purpose": purpose }))?;
            if r["already"].as_bool() != Some(true) {
                c.3 += 1;
            }
            bound.push(json!({ "item_id": c.0, "subject": m["label"], "binding_id": r["binding_id"] }));
            need -= 1;
        }
        if need > 0 {
            unassigned.push(m["label"].clone());
        }
    }
    Ok(json!({ "bound": bound.len(), "bindings": bound, "unassigned_count": unassigned.len(), "unassigned": unassigned }))
}

/// Post fields an upsert may set. `revision`, `parent_post_id` and
/// `review_status` move only through post_revise and the reviews.
const POST_FIELDS: &[(&str, Kind)] = &[
    ("scheduled_at", Kind::Text),
    ("status", Kind::Enum(POST_STATUSES)),
    ("title", Kind::Text),
    ("body_or_path", Kind::Text),
    ("kind", Kind::Enum(POST_KINDS)),
    ("hook", Kind::Text),
    ("script", Kind::Text),
    ("caption", Kind::Text),
    ("hashtags", Kind::Array),
    ("media_brief", Kind::Text),
    ("language", Kind::Lang),
    ("generated_by", Kind::Text),
    ("context_ref", Kind::Text),
];

/// The settable post fields present in `p`; hashtags may come as "#a #b".
fn post_set(p: &Value) -> Map<String, Value> {
    let mut set = Map::new();
    for (k, _) in POST_FIELDS {
        if let Some(v) = p.get(*k) {
            set.insert(k.to_string(), v.clone());
        }
    }
    if let Some(Value::String(tags)) = set.get("hashtags") {
        let list: Vec<Value> =
            tags.split(|c: char| c.is_whitespace() || c == ',').filter(|t| !t.is_empty()).map(|t| json!(t)).collect();
        set.insert("hashtags".into(), Value::Array(list));
    }
    set
}

/// A new post made by an agent in a local thread sends its feedback back to
/// that thread unless it says otherwise.
fn default_origin(set: &mut Map<String, Value>, actor: &str) {
    if !set.contains_key("generated_by") {
        set.insert("generated_by".into(), json!(actor));
    }
    if let Some(thread) = actor.strip_prefix("agent:") {
        if !set.contains_key("context_ref") {
            set.insert("context_ref".into(), json!(thread));
        }
    }
}

fn post_upsert(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    if let Some(k) = ["review_status", "revision", "parent_post_id"].into_iter().find(|k| p.get(*k).is_some()) {
        return Err(format!("{k} can't be set here: review_status follows the reviews, a new revision comes from post_revise"));
    }
    let now = now_iso();
    let mut set = post_set(p);
    let (id, action) = match p.get("id").and_then(num) {
        Some(id) => {
            let id = id as i64;
            conn.query_row("SELECT 1 FROM posts WHERE id = ?1", [id], |_| Ok(())).optional().map_err(e)?.ok_or_else(|| format!("no post {id}"))?;
            if p.get("channel_id").is_some() || p.get("persona").is_some() || p.get("channel").is_some() {
                let ch = channel_ref(conn, p)?;
                conn.execute("UPDATE posts SET channel_id = ?1 WHERE id = ?2", params![ch, id]).map_err(e)?;
            }
            (id, "update")
        }
        None => {
            let ch = channel_ref(conn, p)?;
            conn.execute("INSERT INTO posts (channel_id, created_at, updated_at) VALUES (?1, ?2, ?2)", params![ch, now]).map_err(e)?;
            default_origin(&mut set, actor);
            (conn.last_insert_rowid(), "create")
        }
    };
    let diff = apply_patch(conn, "posts", "id", &Sql::Integer(id), POST_FIELDS, &set)?;
    if action == "create" || !diff.is_empty() {
        audit(conn, actor, action, "post", &id.to_string(), &Value::Object(diff))?;
    }
    query_one(conn, "SELECT * FROM posts WHERE id = ?1", [id])?.ok_or_else(|| "post vanished".into())
}

// ── reviews ──────────────────────────────────────────────────────────────────

fn post_id(p: &Value, key: &str) -> R<i64> {
    let id = p.get(key).and_then(num).map(|n| n as i64).ok_or_else(|| format!("\"{key}\" (post id) is required"))?;
    Ok(id)
}

fn post_row(conn: &Connection, id: i64) -> R<Value> {
    query_one(conn, "SELECT * FROM posts WHERE id = ?1", [id])?.ok_or_else(|| format!("no post {id}"))
}

fn review_row(conn: &Connection, id: i64) -> R<Value> {
    query_one(conn, "SELECT * FROM post_reviews WHERE id = ?1", [id])?.ok_or_else(|| format!("no review {id}"))
}

/// The post's review_status follows its latest verdict ('pending' without one).
fn sync_review_status(conn: &Connection, actor: &str, post: i64) -> R<String> {
    let verdict: Option<String> = conn
        .query_row("SELECT verdict FROM post_reviews WHERE post_id = ?1 ORDER BY at DESC, id DESC LIMIT 1", [post], |r| r.get(0))
        .optional()
        .map_err(e)?;
    let status = match verdict.as_deref() {
        Some("like") => "liked",
        Some("dislike") => "disliked",
        Some("needs_changes") => "needs_changes",
        Some("approve") => "approved",
        _ => "pending",
    };
    let before: String = conn.query_row("SELECT review_status FROM posts WHERE id = ?1", [post], |r| r.get(0)).map_err(e)?;
    if before != status {
        conn.execute("UPDATE posts SET review_status = ?1, updated_at = ?2 WHERE id = ?3", params![status, now_iso(), post]).map_err(e)?;
        audit(conn, actor, "update", "post", &post.to_string(), &json!({ "review_status": { "from": before, "to": status } }))?;
    }
    Ok(status.to_string())
}

/// `{post_id, verdict like|dislike|needs_changes|approve, comment?}` → `{review, post}`.
fn post_review_add(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let post = post_id(p, "post_id")?;
    post_row(conn, post)?;
    let verdict = match field_value("verdict", Kind::Enum(VERDICTS), p.get("verdict").unwrap_or(&Value::Null))? {
        Sql::Text(v) => v,
        _ => unreachable!(),
    };
    let comment = match field_value("comment", Kind::Text, p.get("comment").unwrap_or(&Value::Null))? {
        Sql::Text(c) => c,
        _ => String::new(),
    };
    conn.execute(
        "INSERT INTO post_reviews (post_id, verdict, comment, actor, at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![post, verdict, comment, actor, now_iso()],
    )
    .map_err(e)?;
    let id = conn.last_insert_rowid();
    audit(conn, actor, "create", "post_review", &id.to_string(), &json!({ "post_id": post, "verdict": verdict, "comment": comment }))?;
    sync_review_status(conn, actor, post)?;
    Ok(json!({ "review": review_row(conn, id)?, "post": post_row(conn, post)? }))
}

/// Take a verdict back (the plugin's undo): the row goes, the post's status
/// falls back to the previous verdict.
fn post_review_undo(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = p.get("id").and_then(num).map(|n| n as i64).ok_or("\"id\" (review id) is required")?;
    let row = review_row(conn, id)?;
    if !row["resolved_at"].is_null() {
        return Err(format!("review {id} is already resolved; add a new verdict instead"));
    }
    let post = row["post_id"].as_i64().unwrap_or_default();
    conn.execute("DELETE FROM post_reviews WHERE id = ?1", [id]).map_err(e)?;
    audit(conn, actor, "delete", "post_review", &id.to_string(), &row)?;
    sync_review_status(conn, actor, post)?;
    Ok(json!({ "deleted": id, "was_delivered": !row["delivered_at"].is_null(), "post": post_row(conn, post)? }))
}

/// The plugin sent a review to the generating thread: `{id, to}`.
fn post_review_delivered(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = p.get("id").and_then(num).map(|n| n as i64).ok_or("\"id\" (review id) is required")?;
    review_row(conn, id)?;
    let to = opt_str(p, "to").unwrap_or_default();
    let at = now_iso();
    conn.execute("UPDATE post_reviews SET delivered_at = ?1, delivered_to = ?2 WHERE id = ?3", params![at, to, id]).map_err(e)?;
    audit(conn, actor, "update", "post_review", &id.to_string(), &json!({ "delivered_at": { "from": null, "to": at }, "delivered_to": { "from": "", "to": to } }))?;
    review_row(conn, id)
}

/// Close open reviews: `{id}` one review, or `{post_id}` all of a post's open
/// ones; optional `resolution_post_id` (the revision that answers them) and `note`.
fn post_review_resolve(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let resolution = p.get("resolution_post_id").and_then(num).map(|n| n as i64);
    if let Some(r) = resolution {
        post_row(conn, r)?;
    }
    let note = opt_str(p, "note").unwrap_or_default();
    let ids: Vec<i64> = if let Some(id) = p.get("id").and_then(num) {
        let row = review_row(conn, id as i64)?;
        if !row["resolved_at"].is_null() {
            return Err(format!("review {} is already resolved", id as i64));
        }
        vec![id as i64]
    } else {
        let post = post_id(p, "post_id").map_err(|_| "give the review \"id\" or a \"post_id\"".to_string())?;
        post_row(conn, post)?;
        let mut stmt = conn.prepare("SELECT id FROM post_reviews WHERE post_id = ?1 AND resolved_at IS NULL ORDER BY id").map_err(e)?;
        let ids = stmt.query_map([post], |r| r.get(0)).map_err(e)?.collect::<Result<Vec<i64>, _>>().map_err(e)?;
        ids
    };
    let at = now_iso();
    for id in &ids {
        conn.execute(
            "UPDATE post_reviews SET resolved_at = ?1, resolution_post_id = ?2, resolution_note = ?3 WHERE id = ?4",
            params![at, resolution, note, id],
        )
        .map_err(e)?;
        audit(conn, actor, "update", "post_review", &id.to_string(), &json!({ "resolved_at": { "from": null, "to": at }, "resolution_post_id": { "from": null, "to": resolution } }))?;
    }
    Ok(json!({ "resolved": ids.len(), "review_ids": ids }))
}

/// A new revision of a post: `{post_id, …fields to change}`. The copy keeps
/// the channel and every content field not given, gets revision + 1 and
/// parent_post_id, starts pending, and resolves the parent's open reviews.
fn post_revise(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let parent_id = post_id(p, "post_id")?;
    let parent = post_row(conn, parent_id)?;
    let newer: Option<i64> = conn
        .query_row("SELECT id FROM posts WHERE parent_post_id = ?1 ORDER BY id DESC LIMIT 1", [parent_id], |r| r.get(0))
        .optional()
        .map_err(e)?;
    if let Some(n) = newer {
        return Err(format!("post {parent_id} already has a newer revision (post {n}); revise that one"));
    }
    let now = now_iso();
    conn.execute(
        "INSERT INTO posts (channel_id, scheduled_at, status, title, body_or_path, kind, hook, script, caption, hashtags,
                            media_brief, language, generated_by, context_ref, revision, parent_post_id, review_status, created_at, updated_at)
         SELECT channel_id, scheduled_at, status, title, body_or_path, kind, hook, script, caption, hashtags,
                media_brief, language, generated_by, context_ref, revision + 1, id, 'pending', ?2, ?2
         FROM posts WHERE id = ?1",
        params![parent_id, now],
    )
    .map_err(e)?;
    let id = conn.last_insert_rowid();
    let mut set = post_set(p);
    if !set.contains_key("generated_by") {
        set.insert("generated_by".into(), json!(actor));
    }
    if let (Some(thread), false) = (actor.strip_prefix("agent:"), set.contains_key("context_ref")) {
        if parent["context_ref"].as_str().unwrap_or("").is_empty() {
            set.insert("context_ref".into(), json!(thread));
        }
    }
    let diff = apply_patch(conn, "posts", "id", &Sql::Integer(id), POST_FIELDS, &set)?;
    let mut d = diff;
    d.insert("parent_post_id".into(), json!({ "from": null, "to": parent_id }));
    d.insert("revision".into(), json!({ "from": parent["revision"], "to": parent["revision"].as_i64().unwrap_or(1) + 1 }));
    audit(conn, actor, "create", "post", &id.to_string(), &Value::Object(d))?;
    let note = opt_str(p, "note").unwrap_or_else(|| format!("revised as post {id}"));
    let resolved = post_review_resolve(conn, actor, &json!({ "post_id": parent_id, "resolution_post_id": id, "note": note }))?;
    Ok(json!({ "post": post_row(conn, id)?, "parent_post_id": parent_id, "resolved": resolved["resolved"] }))
}

/// Reviews by post id, oldest first.
fn reviews_by_post(conn: &Connection, post_ids: Option<&[i64]>) -> R<HashMap<i64, Vec<Value>>> {
    let rows = match post_ids {
        None => query_json(conn, "SELECT * FROM post_reviews ORDER BY at, id", [])?,
        Some([]) => Vec::new(),
        Some(ids) => {
            let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
            query_json(conn, &format!("SELECT * FROM post_reviews WHERE post_id IN ({list}) ORDER BY at, id"), [])?
        }
    };
    let mut out: HashMap<i64, Vec<Value>> = HashMap::new();
    for r in rows {
        out.entry(r["post_id"].as_i64().unwrap_or_default()).or_default().push(r);
    }
    Ok(out)
}

/// Posts as the plugin and posts.json see them: the legacy keys (datetime,
/// text_or_file) plus the content fields, persona facts and `reviews`.
const POST_SELECT: &str = "SELECT p.id, p.channel_id, c.persona, c.platform, p.scheduled_at AS datetime, p.status, p.title,
       p.body_or_path AS text_or_file, p.kind, p.hook, p.script, p.caption, p.hashtags, p.media_brief, p.language,
       p.generated_by, p.context_ref, p.revision, p.parent_post_id, p.review_status, p.created_at, p.updated_at,
       (SELECT min(k.id) FROM posts k WHERE k.parent_post_id = p.id) AS child_post_id,
       ps.name AS persona_name, ps.emoji AS persona_emoji, ps.country AS persona_country
  FROM posts p JOIN channels c ON c.id = p.channel_id JOIN personas ps ON ps.handle = c.persona";

fn with_reviews(conn: &Connection, mut posts: Vec<Value>, all: bool) -> R<Vec<Value>> {
    let ids: Vec<i64> = posts.iter().filter_map(|p| p["id"].as_i64()).collect();
    let mut reviews = reviews_by_post(conn, if all { None } else { Some(&ids) })?;
    for p in &mut posts {
        let id = p["id"].as_i64().unwrap_or_default();
        p["reviews"] = json!(reviews.remove(&id).unwrap_or_default());
    }
    Ok(posts)
}

/// The Review tab's feed: head revisions (no newer revision), filtered by
/// persona, country, platform, review_status ('all' = any), kind, date range
/// (`from` / `to` on scheduled_at, YYYY-MM-DD); `include_ids` always listed
/// (posts reviewed in this session stay put). Counts ignore the status filter.
fn review_feed(conn: &Connection, p: &Value) -> R<Value> {
    let mut wh: Vec<String> = vec!["NOT EXISTS (SELECT 1 FROM posts k WHERE k.parent_post_id = p.id)".into()];
    let mut args: Vec<Sql> = Vec::new();
    let mut arg = |wh: &mut Vec<String>, sql: &str, v: String| {
        args.push(Sql::Text(v));
        wh.push(sql.replace('?', &format!("?{}", args.len())));
    };
    let pick = |k: &str| opt_str(p, k).filter(|v| v != "all");
    if let Some(v) = pick("persona") {
        arg(&mut wh, "c.persona = ?", v.trim_start_matches('@').to_string());
    }
    if let Some(v) = pick("country") {
        arg(&mut wh, "ps.country = ?", v.to_ascii_uppercase());
    }
    if let Some(v) = pick("platform") {
        arg(&mut wh, "c.platform = ?", v.to_ascii_lowercase());
    }
    if let Some(v) = pick("kind") {
        arg(&mut wh, "p.kind = ?", v);
    }
    if let Some(v) = pick("from") {
        arg(&mut wh, "p.scheduled_at <> '' AND substr(p.scheduled_at, 1, 10) >= ?", v);
    }
    if let Some(v) = pick("to") {
        arg(&mut wh, "p.scheduled_at <> '' AND substr(p.scheduled_at, 1, 10) <= ?", v);
    }
    let base = wh.join(" AND ");
    let mut counts = Map::new();
    for s in REVIEW_STATUSES {
        counts.insert(s.to_string(), json!(0));
    }
    let mut total = 0;
    {
        let sql = format!(
            "SELECT p.review_status, count(*) FROM posts p JOIN channels c ON c.id = p.channel_id JOIN personas ps ON ps.handle = c.persona WHERE {base} GROUP BY 1"
        );
        let mut stmt = conn.prepare(&sql).map_err(e)?;
        let mut rows = stmt.query(params_from_iter(args.iter())).map_err(e)?;
        while let Some(r) = rows.next().map_err(e)? {
            let (s, n): (String, i64) = (r.get(0).map_err(e)?, r.get(1).map_err(e)?);
            total += n;
            counts.insert(s, json!(n));
        }
    }
    counts.insert("total".into(), json!(total));
    let status = pick("review_status").or_else(|| pick("status"));
    let mut filtered = base.clone();
    let mut fargs = args.clone();
    if let Some(s) = status {
        if !REVIEW_STATUSES.contains(&s.as_str()) {
            return Err(format!("review_status must be all or one of: {}", REVIEW_STATUSES.join(", ")));
        }
        fargs.push(Sql::Text(s));
        filtered = format!("{filtered} AND p.review_status = ?{}", fargs.len());
    }
    let include: Vec<i64> =
        p.get("include_ids").and_then(Value::as_array).map(|a| a.iter().filter_map(num).map(|n| n as i64).collect()).unwrap_or_default();
    if !include.is_empty() {
        let list = include.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        filtered = format!("(({filtered}) OR (p.id IN ({list}) AND {base}))");
    }
    let limit = p.get("limit").and_then(num).map(|n| n.clamp(1.0, 5000.0) as i64).unwrap_or(2000);
    let sql = format!(
        "{POST_SELECT} WHERE {filtered} ORDER BY p.scheduled_at = '', p.scheduled_at, p.id LIMIT {limit}"
    );
    let posts = with_reviews(conn, query_json(conn, &sql, params_from_iter(fargs.iter()))?, false)?;
    Ok(json!({ "posts": posts, "counts": counts }))
}

/// A post's revision chain, oldest first (ancestors, the post, newer
/// revisions), each with its reviews.
fn post_history(conn: &Connection, p: &Value) -> R<Value> {
    let id = post_id(p, "id").or_else(|_| post_id(p, "post_id"))?;
    post_row(conn, id)?;
    let mut chain = vec![id];
    let mut cur = id;
    while let Some(parent) = conn
        .query_row("SELECT parent_post_id FROM posts WHERE id = ?1", [cur], |r| r.get::<_, Option<i64>>(0))
        .map_err(e)?
    {
        if chain.contains(&parent) || chain.len() > 100 {
            break;
        }
        chain.insert(0, parent);
        cur = parent;
    }
    cur = id;
    while let Some(child) = conn
        .query_row("SELECT min(id) FROM posts WHERE parent_post_id = ?1", [cur], |r| r.get::<_, Option<i64>>(0))
        .map_err(e)?
    {
        if chain.contains(&child) || chain.len() > 100 {
            break;
        }
        chain.push(child);
        cur = child;
    }
    let list = chain.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
    let posts = query_json(conn, &format!("{POST_SELECT} WHERE p.id IN ({list}) ORDER BY p.revision, p.id"), [])?;
    Ok(json!(with_reviews(conn, posts, false)?))
}

/// For agents: posts with their reviews (open ones only by default), so the
/// next generation run can act on them. Filters: persona, platform, context_ref.
fn review_feedback(conn: &Connection, p: &Value) -> R<Value> {
    let unresolved = p.get("unresolved_only").and_then(Value::as_bool).unwrap_or(true);
    let mut wh = vec![format!(
        "EXISTS (SELECT 1 FROM post_reviews r WHERE r.post_id = p.id{})",
        if unresolved { " AND r.resolved_at IS NULL" } else { "" }
    )];
    let mut args: Vec<Sql> = Vec::new();
    for (key, col) in [("persona", "c.persona"), ("platform", "c.platform"), ("context_ref", "p.context_ref")] {
        if let Some(v) = opt_str(p, key) {
            let v = if key == "persona" { v.trim_start_matches('@').to_string() } else { v };
            args.push(Sql::Text(v));
            wh.push(format!("{col} = ?{}", args.len()));
        }
    }
    let limit = p.get("limit").and_then(num).map(|n| n.clamp(1.0, 500.0) as i64).unwrap_or(100);
    let sql = format!("{POST_SELECT} WHERE {} ORDER BY p.id LIMIT {limit}", wh.join(" AND "));
    let mut posts = with_reviews(conn, query_json(conn, &sql, params_from_iter(args.iter()))?, false)?;
    for post in &mut posts {
        if let Some(obj) = post.as_object_mut() {
            for k in ["persona_name", "persona_emoji", "persona_country", "created_at", "updated_at"] {
                obj.remove(k);
            }
            if unresolved {
                if let Some(Value::Array(rs)) = obj.get_mut("reviews") {
                    rs.retain(|r| r["resolved_at"].is_null());
                }
            }
        }
    }
    Ok(json!({ "posts": posts, "count": posts.len(),
        "next": "Revise a post with post_revise {post_id, …changed fields} (resolves its open reviews); acknowledge likes/approvals with post_review_resolve {post_id, note}." }))
}

fn post_delete(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = p.get("id").and_then(num).map(|n| n as i64).ok_or("\"id\" (post id) is required")?;
    let row = query_one(conn, "SELECT * FROM posts WHERE id = ?1", [id])?.ok_or_else(|| format!("no post {id}"))?;
    conn.execute("DELETE FROM posts WHERE id = ?1", [id]).map_err(e)?;
    audit(conn, actor, "delete", "post", &id.to_string(), &row)?;
    Ok(json!({ "deleted": id }))
}

/// One row per channel per date (a second call for the same date replaces
/// it). `rows: [...]` records several at once.
fn metrics_add(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let rows: Vec<Value> = match p.get("rows") {
        Some(Value::Array(r)) => r.clone(),
        Some(_) => return Err("rows must be an array".into()),
        None => vec![p.clone()],
    };
    let mut written = Vec::new();
    for r in &rows {
        let ch = channel_ref(conn, r)?;
        let date = req_str(r, "date")?;
        let valid = date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-' && date.chars().filter(char::is_ascii_digit).count() == 8;
        if !valid {
            return Err(format!("date must be YYYY-MM-DD, got {date}"));
        }
        let n = |k: &str| r.get(k).and_then(num);
        let currency = opt_str(r, "currency").map(|c| c.to_ascii_uppercase()).unwrap_or_else(|| "EUR".into());
        let before = query_one(conn, "SELECT reach, followers, posts_published, cost, currency FROM metrics WHERE channel_id = ?1 AND date = ?2", params![ch, date])?;
        conn.execute(
            "INSERT INTO metrics (channel_id, date, reach, followers, posts_published, cost, currency) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (channel_id, date) DO UPDATE SET reach = excluded.reach, followers = excluded.followers,
               posts_published = excluded.posts_published, cost = excluded.cost, currency = excluded.currency",
            params![ch, date, n("reach"), n("followers"), n("posts_published"), n("cost"), currency],
        )
        .map_err(e)?;
        let after = json!({ "reach": n("reach"), "followers": n("followers"), "posts_published": n("posts_published"), "cost": n("cost"), "currency": currency });
        audit(conn, actor, if before.is_some() { "update" } else { "create" }, "metrics", &format!("{ch}:{date}"), &json!({ "from": before, "to": after }))?;
        written.push(json!({ "channel_id": ch, "date": date }));
    }
    Ok(json!({ "written": written.len(), "rows": written }))
}

fn requirement_add(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let platform = opt_str(p, "platform").map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "*".into());
    if platform != "*" && !PLATFORMS.contains(&platform.as_str()) {
        return Err(format!("platform must be * or one of: {}", PLATFORMS.join(", ")));
    }
    let typ = req_str(p, "type")?.to_ascii_lowercase();
    if !ITEM_TYPES.contains(&typ.as_str()) {
        return Err(format!("type must be one of: {}", ITEM_TYPES.join(", ")));
    }
    let scope = opt_str(p, "scope").unwrap_or_else(|| "persona".into());
    if scope != "persona" && scope != "channel" {
        return Err("scope must be persona or channel".into());
    }
    let purpose = opt_str(p, "purpose").unwrap_or_else(|| default_purpose(&typ).into());
    if !PURPOSES.contains(&purpose.as_str()) {
        return Err(format!("purpose must be one of: {}", PURPOSES.join(", ")));
    }
    let count = p.get("count").and_then(num).map(|n| n.max(1.0) as i64).unwrap_or(1);
    let geo_match = p.get("geo_match").and_then(Value::as_bool).unwrap_or(false);
    let notes = opt_str(p, "notes").unwrap_or_default();
    conn.execute(
        "INSERT INTO inventory_requirements (platform, type, scope, purpose, count, geo_match, notes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![platform, typ, scope, purpose, count, geo_match, notes],
    )
    .map_err(|err| format!("requirement: {err}"))?;
    let id = conn.last_insert_rowid();
    let row = query_one(conn, "SELECT * FROM inventory_requirements WHERE id = ?1", [id])?.unwrap_or(Value::Null);
    audit(conn, actor, "create", "requirement", &id.to_string(), &row)?;
    Ok(row)
}

fn requirement_delete(conn: &Connection, actor: &str, p: &Value) -> R<Value> {
    let id = p.get("id").and_then(num).map(|n| n as i64).ok_or("\"id\" is required")?;
    let row = query_one(conn, "SELECT * FROM inventory_requirements WHERE id = ?1", [id])?.ok_or_else(|| format!("no requirement {id}"))?;
    conn.execute("DELETE FROM inventory_requirements WHERE id = ?1", [id]).map_err(e)?;
    audit(conn, actor, "delete", "requirement", &id.to_string(), &row)?;
    Ok(json!({ "deleted": id }))
}

// ── import (legacy files) ────────────────────────────────────────────────────

impl StateDb {
    /// Import `dir` (the old `~/AgentArea/channels` layout): `<handle>/config.json`,
    /// `channels.json`, `posts.json`, `metrics.csv`. Idempotent: rows are
    /// upserted by their natural keys. After a verified import with
    /// `move_legacy`, the files move to `dir/_legacy/` (SOUL.md stays).
    pub fn import_dir(&self, actor: &str, dir: &Path, move_legacy: bool) -> R<Value> {
        let read_json = |name: &str| -> R<Option<Value>> {
            match std::fs::read_to_string(dir.join(name)) {
                Ok(s) if s.trim().is_empty() => Ok(None),
                Ok(s) => serde_json::from_str(&s).map(Some).map_err(|err| format!("{name}: {err}")),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => Err(format!("{name}: {err}")),
            }
        };
        // Persona folders: any sub-folder with a config.json.
        let mut configs: Vec<(String, Value, bool)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name.starts_with('_') || name == "export" || !entry.path().is_dir() {
                    continue;
                }
                let cfg = entry.path().join("config.json");
                if let Ok(s) = std::fs::read_to_string(&cfg) {
                    let v: Value = serde_json::from_str(&s).map_err(|err| format!("{name}/config.json: {err}"))?;
                    configs.push((name.clone(), v, entry.path().join("SOUL.md").is_file()));
                }
            }
        }
        configs.sort_by(|a, b| a.0.cmp(&b.0));
        let channels = read_json("channels.json")?.unwrap_or(json!([]));
        let channels = channels.as_array().ok_or("channels.json: expected an array")?.clone();
        let posts = read_json("posts.json")?.unwrap_or(json!([]));
        let posts = posts.as_array().ok_or("posts.json: expected an array")?.clone();
        let metrics = match std::fs::read_to_string(dir.join("metrics.csv")) {
            Ok(s) => parse_csv(&s),
            Err(_) => Vec::new(),
        };
        if configs.is_empty() && channels.is_empty() && posts.is_empty() && metrics.is_empty() {
            return Ok(json!({ "imported": false, "reason": format!("nothing to import in {}", dir.display()) }));
        }

        let month = this_month();
        let report = self.write(|tx| {
            let now = now_iso();
            let mut n_personas = 0;
            for (folder, cfg, has_soul) in &configs {
                let s = |k: &str| cfg.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                let handle = folder.clone();
                let created = channels
                    .iter()
                    .find(|c| c["persona"] == handle.as_str())
                    .and_then(|c| c["created"].as_str())
                    .unwrap_or(&now)
                    .to_string();
                tx.execute(
                    "INSERT INTO personas (handle, name, emoji, niche, category, country, city, languages, target_audience, hypothesis, personality, refs, soul_path, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)
                     ON CONFLICT (handle) DO UPDATE SET name = excluded.name, emoji = excluded.emoji, niche = excluded.niche,
                       category = excluded.category, country = excluded.country, city = excluded.city, languages = excluded.languages,
                       target_audience = excluded.target_audience, hypothesis = excluded.hypothesis, personality = excluded.personality,
                       refs = excluded.refs, soul_path = excluded.soul_path, updated_at = ?15",
                    params![
                        handle,
                        s("name"),
                        s("emoji"),
                        s("niche"),
                        s("category"),
                        s("country").to_ascii_uppercase(),
                        s("city"),
                        cfg.get("languages").cloned().unwrap_or(json!([])).to_string(),
                        s("target_audience"),
                        s("hypothesis"),
                        cfg.get("personality").cloned().unwrap_or(json!({})).to_string(),
                        cfg.get("references").cloned().unwrap_or(json!([])).to_string(),
                        has_soul.then(|| format!("{handle}/SOUL.md")),
                        created,
                        now,
                    ],
                )
                .map_err(|err| format!("persona {handle}: {err}"))?;
                n_personas += 1;
            }

            let mut n_channels = 0;
            let mut n_tasks = 0;
            let mut n_budget = 0;
            let mut stubs = Vec::new();
            let mut seen = HashSet::new();
            for c in &channels {
                let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                let persona = s("persona").trim_start_matches('@').to_string();
                let platform = s("platform").to_ascii_lowercase();
                if persona.is_empty() || platform.is_empty() {
                    continue;
                }
                seen.insert((persona.clone(), platform.clone()));
                if !persona_exists(tx, &persona)? {
                    tx.execute("INSERT INTO personas (handle, created_at, updated_at) VALUES (?1, ?2, ?2)", params![persona, now]).map_err(e)?;
                    stubs.push(persona.clone());
                }
                let stage = Some(s("stage")).filter(|x| STAGES.contains(&x.as_str())).unwrap_or_else(|| "idea".into());
                let role = Some(s("role")).filter(|x| ROLES.contains(&x.as_str())).unwrap_or_else(|| "creator".into());
                let net = c.get("network").cloned().unwrap_or(json!({}));
                let ns = |k: &str| net.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                // Network fields now come from inventory bindings; keep any
                // hand-written values visible in the notes instead of losing them.
                let mut notes = s("notes");
                let legacy: Vec<String> = [("egress", ns("egress")), ("ip", ns("ip")), ("device_profile", ns("device_profile"))]
                    .into_iter()
                    .filter(|(_, v)| !v.is_empty())
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect();
                if !legacy.is_empty() {
                    let line = format!("[legacy network] {}", legacy.join("; "));
                    if !notes.contains(&line) {
                        notes = if notes.is_empty() { line } else { format!("{notes}\n{line}") };
                    }
                }
                let created = Some(s("created")).filter(|x| !x.is_empty()).unwrap_or_else(|| now.clone());
                let updated = Some(s("updated")).filter(|x| !x.is_empty()).unwrap_or_else(|| created.clone());
                tx.execute(
                    "INSERT INTO channels (persona, platform, handle_registered, url, stage, role, plan, kpi_metric, kpi_target, mcp, notes, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                     ON CONFLICT (persona, platform) DO UPDATE SET handle_registered = excluded.handle_registered, url = excluded.url,
                       stage = excluded.stage, role = excluded.role, plan = excluded.plan, kpi_metric = excluded.kpi_metric,
                       kpi_target = excluded.kpi_target, mcp = excluded.mcp, notes = excluded.notes, updated_at = excluded.updated_at",
                    params![
                        persona, platform, s("handle"), ns("url"), stage, role, s("plan"), s("kpi_metric"),
                        c.get("kpi_target").and_then(num), s("mcp"), notes, created, updated
                    ],
                )
                .map_err(|err| format!("channel {persona}/{platform}: {err}"))?;
                let id: i64 = tx
                    .query_row("SELECT id FROM channels WHERE persona = ?1 AND platform = ?2", params![persona, platform], |r| r.get(0))
                    .map_err(e)?;
                n_channels += 1;
                if let Some(tasks) = c.get("tasks").filter(|t| t.is_array()) {
                    let tasks: Vec<Value> = tasks.as_array().into_iter().flatten().filter(|t| t.get("type").and_then(Value::as_str).is_some_and(|x| !x.trim().is_empty())).cloned().collect();
                    n_tasks += write_tasks(tx, id, &Value::Array(tasks))?.as_array().map(Vec::len).unwrap_or(0);
                }
                let money = |b: &Value| -> Value {
                    let mut m = Map::new();
                    if let Some(cur) = b.get("currency").and_then(Value::as_str).filter(|x| !x.trim().is_empty()) {
                        m.insert("currency".into(), json!(cur));
                    }
                    for k in BUDGET_KEYS {
                        m.insert(k.into(), json!(b.get(k).and_then(num).unwrap_or(0.0)));
                    }
                    Value::Object(m)
                };
                if let Some(b) = c.get("budget").filter(|b| b.is_object()) {
                    write_budget(tx, id, "planned", "monthly", &money(b))?;
                    n_budget += 1;
                }
                if let Some(b) = c.get("spent").filter(|b| b.is_object()) {
                    // Zero actuals aren't history worth keeping.
                    if BUDGET_KEYS.iter().any(|k| b.get(*k).and_then(num).unwrap_or(0.0) != 0.0) {
                        write_budget(tx, id, "spent", &month, &money(b))?;
                        n_budget += 1;
                    }
                }
            }

            let mut n_posts = 0;
            for p in &posts {
                let s = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                let ch = channel_ref(tx, &json!({ "persona": s("persona"), "platform": s("platform") }))
                    .map_err(|err| format!("posts.json: {err}"))?;
                let when = Some(s("datetime")).filter(|x| !x.is_empty()).unwrap_or_else(|| s("date"));
                let status = Some(s("status")).filter(|x| POST_STATUSES.contains(&x.as_str())).unwrap_or_else(|| "draft".into());
                let body = Some(s("text_or_file")).filter(|x| !x.is_empty()).unwrap_or_else(|| s("text"));
                let existing: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM posts WHERE channel_id = ?1 AND scheduled_at = ?2 AND title = ?3",
                        params![ch, when, s("title")],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(e)?;
                match existing {
                    Some(id) => tx
                        .execute("UPDATE posts SET status = ?1, body_or_path = ?2, updated_at = ?3 WHERE id = ?4", params![status, body, now, id])
                        .map_err(e)?,
                    None => tx
                        .execute(
                            "INSERT INTO posts (channel_id, scheduled_at, status, title, body_or_path, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                            params![ch, when, status, s("title"), body, now],
                        )
                        .map_err(e)?,
                };
                n_posts += 1;
            }

            let mut n_metrics = 0;
            for m in &metrics {
                let g = |k: &str| m.get(k).cloned().unwrap_or_default();
                let ch = channel_ref(tx, &json!({ "persona": g("persona"), "platform": g("platform") }))
                    .map_err(|err| format!("metrics.csv: {err}"))?;
                let n = |k: &str| num(&json!(g(k)));
                let cur = Some(g("currency")).filter(|x| !x.trim().is_empty()).unwrap_or_else(|| "EUR".into());
                tx.execute(
                    "INSERT INTO metrics (channel_id, date, reach, followers, posts_published, cost, currency) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT (channel_id, date) DO UPDATE SET reach = excluded.reach, followers = excluded.followers,
                       posts_published = excluded.posts_published, cost = excluded.cost, currency = excluded.currency",
                    params![ch, g("date"), n("reach"), n("followers"), n("posts_published"), n("cost"), cur.to_ascii_uppercase()],
                )
                .map_err(e)?;
                n_metrics += 1;
            }

            // Verify before anything moves: every file row is in the database.
            let count = |sql: &str| -> R<i64> { tx.query_row(sql, [], |r| r.get(0)).map_err(e) };
            let db_personas = count("SELECT count(*) FROM personas")?;
            let db_channels = count("SELECT count(*) FROM channels")?;
            if db_personas < n_personas as i64 + stubs.len() as i64 || db_channels < seen.len() as i64 {
                return Err(format!(
                    "import check failed: {db_personas} personas / {db_channels} channels in the database, expected at least {} / {}",
                    n_personas + stubs.len(),
                    seen.len()
                ));
            }
            let report = json!({
                "imported": true,
                "dir": dir.display().to_string(),
                "personas": n_personas,
                "stub_personas": stubs,
                "channels": n_channels,
                "distinct_channels": seen.len(),
                "tasks": n_tasks,
                "budget_rows": n_budget,
                "posts": n_posts,
                "metrics": n_metrics,
                "db_personas": db_personas,
                "db_channels": db_channels,
            });
            audit(tx, actor, "import", "state", "", &report)?;
            Ok(report)
        })?;

        let mut report = report;
        if move_legacy {
            report["moved"] = json!(move_to_legacy(dir, &configs.iter().map(|c| c.0.clone()).collect::<Vec<_>>())?);
        }
        Ok(report)
    }
}

/// Move the imported files under `dir/_legacy/` (same relative paths). An
/// existing backup is never overwritten: the new copy gets a timestamp.
fn move_to_legacy(dir: &Path, personas: &[String]) -> R<Vec<String>> {
    let legacy = dir.join("_legacy");
    let stamp = now_iso().replace([':', '-'], "");
    let mut files: Vec<String> = ["channels.json", "posts.json", "metrics.csv"].iter().map(|s| s.to_string()).collect();
    for p in personas {
        files.push(format!("{p}/config.json"));
        files.push(format!("{p}/plan.json"));
    }
    let mut moved = Vec::new();
    for rel in files {
        let src = dir.join(&rel);
        if !src.is_file() {
            continue;
        }
        let mut dst = legacy.join(&rel);
        if dst.exists() {
            dst = dst.with_file_name(format!("{}.{stamp}", dst.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()));
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|err| format!("{}: {err}", parent.display()))?;
        }
        std::fs::rename(&src, &dst).map_err(|err| format!("move {rel}: {err}"))?;
        moved.push(rel);
    }
    Ok(moved)
}

/// CSV → rows keyed by lower-cased header ("quoted, fields" and "" escapes).
fn parse_csv(src: &str) -> Vec<HashMap<String, String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let (mut row, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut chars = src.chars().filter(|c| *c != '\r').peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                cur.push(ch);
            }
        } else {
            match ch {
                '"' => quoted = true,
                ',' => row.push(std::mem::take(&mut cur)),
                '\n' => {
                    row.push(std::mem::take(&mut cur));
                    rows.push(std::mem::take(&mut row));
                }
                _ => cur.push(ch),
            }
        }
    }
    if !cur.is_empty() || !row.is_empty() {
        row.push(cur);
        rows.push(row);
    }
    let rows: Vec<Vec<String>> = rows.into_iter().filter(|r| r.iter().any(|c| !c.trim().is_empty())).collect();
    let Some((head, body)) = rows.split_first() else { return Vec::new() };
    let head: Vec<String> = head.iter().map(|h| h.trim().to_lowercase()).collect();
    body.iter()
        .map(|r| head.iter().enumerate().map(|(i, h)| (h.clone(), r.get(i).map(|s| s.trim().to_string()).unwrap_or_default())).collect())
        .collect()
}

// ── Tauri command ────────────────────────────────────────────────────────────

/// The app's (and, with `plugin`, a plugin bridge's) way in. Plugins need the
/// "state" permission, act as `plugin:<id>` and can't run the import.
#[tauri::command]
pub async fn state_call(method: String, params: Option<Value>, plugin: Option<String>) -> Result<Value, String> {
    let actor = match &plugin {
        Some(id) => {
            crate::plugins::require_permission(id, "state")?;
            if method == "import" {
                return Err("plugins can't run the import".into());
            }
            format!("plugin:{id}")
        }
        None => "user".to_string(),
    };
    let params = params.unwrap_or_else(|| json!({}));
    tauri::async_runtime::spawn_blocking(move || user()?.call(&actor, &method, &params)).await.map_err(e)?
}

// ── MCP tools (agentarea_data) ───────────────────────────────────────────────

/// Tool definitions merged into the agentarea_data server's tools/list.
pub fn mcp_tools() -> Value {
    let chan = json!({
        "channel_id": { "type": "integer", "description": "Channel id (or give persona + platform)" },
        "persona": { "type": "string", "description": "Persona handle, e.g. vital.repairs.leipzig" },
        "platform": { "type": "string", "enum": PLATFORMS },
    });
    let with = |extra: Value| -> Value {
        let mut m = chan.as_object().cloned().unwrap_or_default();
        if let Value::Object(x) = extra {
            m.extend(x);
        }
        Value::Object(m)
    };
    json!([
        {
            "name": "state_query",
            "description": "Read-only SQL (SQLite) over the Channels network state in ~/AgentArea/state.db. One SELECT or WITH statement; writes, PRAGMA and ATTACH are rejected; at most 1000 rows (default 200). Tables: personas(handle PK, name, emoji, niche, category, country DE|PL|IT, city, languages JSON, target_audience, hypothesis, personality JSON, refs JSON, soul_path → ~/AgentArea/channels/<soul_path>), channels(id, persona, platform tiktok|instagram|youtube|facebook|x|threads|telegram, handle_registered, url, stage idea|setup|setup_done|live|paused|stopped, role creator|community|engager, plan, kpi_metric, kpi_target, mcp, notes), channel_tasks(channel_id, position, type, cadence, notes), budget(channel_id, kind planned|spent, period 'monthly'|'YYYY-MM', currency, proxy, account, content, ads, other), metrics(channel_id, date, reach, followers, posts_published, cost, currency), posts(id, channel_id, scheduled_at, status draft|scheduled|published, title, body_or_path, kind video_script|caption|text|telegram_post|carousel|comment_reply, hook, script, caption, hashtags JSON, media_brief, language, generated_by, context_ref, revision, parent_post_id, review_status pending|liked|disliked|needs_changes|approved), post_reviews(id, post_id, verdict like|dislike|needs_changes|approve, comment, actor, at, delivered_at, delivered_to, resolved_at, resolution_post_id, resolution_note), inventory_items(id, type proxy|phone|email|device_profile|api_key|account|domain|card|other, label, provider, geo, cost_month, currency, status active|reserved|broken|retired, capacity NULL=unlimited, secret_ref = vault secret NAME, meta JSON), bindings(id, item_id, subject_type persona|channel|mcp, subject_id, purpose egress|login|2fa|api|device|other, since, until NULL = active), inventory_requirements(platform|'*', type, scope persona|channel, purpose, count, geo_match), audit_log(at, actor, action, entity, entity_id, diff). Use json_extract() for JSON columns. Full schema: ~/AgentArea/channels/SCHEMA.md.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sql": { "type": "string", "description": "e.g. SELECT platform, stage, count(*) FROM channels GROUP BY 1, 2" },
                    "params": { "type": "array", "description": "Values for ?1, ?2 … placeholders" },
                    "limit": { "type": "integer", "description": "Row cap, default 200, max 1000" }
                },
                "required": ["sql"]
            }
        },
        {
            "name": "channel_update",
            "description": "Change one channel (persona × platform). `set` may hold: stage, role, plan, handle_registered (the registered @handle), url, kpi_metric, kpi_target, mcp (MCP server that runs the account), notes, tasks (full list [{type, cadence, notes}]), budget (planned per month {currency, proxy, account, content, ads, other}; omitted keys keep their value), spent (actuals {period 'YYYY-MM' default this month, currency, proxy, …}). Audited. Never put credentials in any field.",
            "inputSchema": { "type": "object", "properties": with(json!({ "set": { "type": "object" } })), "required": ["set"] }
        },
        {
            "name": "persona_update",
            "description": "Change a persona's structured fields. `set` may hold: name, emoji, niche, category, country (DE|PL|IT…), city, languages [..], target_audience, hypothesis, personality {ocean{…}, valence, humor, energy, archetype, likes, dislikes}, references [..], soul_path. The long text stays in ~/AgentArea/channels/<handle>/SOUL.md — edit that file directly. Audited.",
            "inputSchema": {
                "type": "object",
                "properties": { "handle": { "type": "string" }, "set": { "type": "object" } },
                "required": ["handle", "set"]
            }
        },
        {
            "name": "inventory_add",
            "description": "Add an inventory item (a proxy, phone number, email, device/browser profile, API key, platform account, domain, card…). An account is one registered/purchased platform login: meta {platform, username, url, email_item (id of its email item), recovery_email_item, has_2fa}; it binds to a channel of the same platform with purpose login. Credentials never go here: store them in the user's vault (AgentArea → Secrets) and put the secret's NAME in secret_ref. meta holds plain facts (host, port, protocol, ip, address, number, profile_id…); keys like password/token/secret are rejected. capacity = how many personas/channels/MCPs may share it (null = unlimited; a persona's proxy is usually 1). Returns the item with its id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": { "type": "string", "enum": ITEM_TYPES },
                    "label": { "type": "string", "description": "Short name, e.g. 'DE residential #03'" },
                    "provider": { "type": "string" },
                    "geo": { "type": "string", "description": "Country code first: DE, DE-BY, 'Munich, DE'" },
                    "cost_month": { "type": "number" },
                    "currency": { "type": "string", "description": "EUR (default), USD, PLN…" },
                    "status": { "type": "string", "enum": ITEM_STATUSES },
                    "capacity": { "type": ["integer", "null"] },
                    "secret_ref": { "type": ["string", "null"], "description": "Vault secret NAME, e.g. PROXY_DE_03" },
                    "meta": { "type": "object" },
                    "notes": { "type": "string" }
                },
                "required": ["type", "label"]
            }
        },
        {
            "name": "inventory_update",
            "description": "Change an inventory item: `set` with any of type, label, provider, geo, cost_month, currency, status (active|reserved|broken|retired), capacity, secret_ref, meta, notes. Mark a dead proxy broken instead of deleting it; bindings to broken/retired items stop counting as coverage. Audited.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" }, "set": { "type": "object" } },
                "required": ["id", "set"]
            }
        },
        {
            "name": "inventory_bind",
            "description": "Bind an item to a persona (handle), channel (id or persona/platform) or MCP server (name) for a purpose (egress|login|2fa|api|device|other; default from the item type: proxy→egress, email→login, phone→2fa, device_profile→device, api_key→api). A persona's items also apply to all its channels. Refused when the item is broken/retired or its capacity is full. Idempotent.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "item_id": { "type": "integer" },
                    "subject_type": { "type": "string", "enum": SUBJECT_TYPES },
                    "subject_id": { "type": "string" },
                    "purpose": { "type": "string", "enum": PURPOSES },
                    "notes": { "type": "string" }
                },
                "required": ["item_id", "subject_type", "subject_id"]
            }
        },
        {
            "name": "inventory_unbind",
            "description": "End a binding (history is kept): by binding_id, or item_id + subject_type + subject_id (+ purpose).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "binding_id": { "type": "integer" },
                    "item_id": { "type": "integer" },
                    "subject_type": { "type": "string", "enum": SUBJECT_TYPES },
                    "subject_id": { "type": "string" },
                    "purpose": { "type": "string", "enum": PURPOSES }
                }
            }
        },
        {
            "name": "post_upsert",
            "description": "Create a post (give the channel) or update one (give id). Fields: scheduled_at ('YYYY-MM-DD HH:MM' local), status draft|scheduled|published, kind (video_script for TikTok/Reels/Shorts, caption, carousel, text for X/Threads/Facebook, telegram_post, comment_reply), hook (first line / on-screen text), script (spoken + on-screen lines of a video), caption, hashtags [\"#a\", …], media_brief (what to shoot or generate), language (uk|ru|en…), title, body_or_path (the text, or a path relative to ~/AgentArea/channels for long drafts). A new post records generated_by = you and context_ref = your thread, so the user's review feedback comes back to this thread; set context_ref: \"\" to opt out. To change a post the user already reviewed, use post_revise instead.",
            "inputSchema": {
                "type": "object",
                "properties": with(json!({
                    "id": { "type": "integer" },
                    "scheduled_at": { "type": "string" },
                    "status": { "type": "string", "enum": POST_STATUSES },
                    "kind": { "type": "string", "enum": POST_KINDS },
                    "title": { "type": "string" },
                    "hook": { "type": "string" },
                    "script": { "type": "string" },
                    "caption": { "type": "string" },
                    "hashtags": { "type": "array", "items": { "type": "string" } },
                    "media_brief": { "type": "string" },
                    "language": { "type": "string", "description": "uk, ru, en…" },
                    "body_or_path": { "type": "string" },
                    "generated_by": { "type": "string", "description": "Default: agent:<your thread>; or a batch label" },
                    "context_ref": { "type": "string", "description": "Desktop thread id that gets the review feedback (default: yours)" }
                }))
            }
        },
        {
            "name": "review_feedback",
            "description": "The user's review verdicts on generated posts (like, dislike, needs_changes, approve + comment), for the next generation run. Returns posts with their content and open reviews (unresolved_only, default true). Act on them: post_revise for needs_changes / dislike, post_review_resolve {post_id, note} to acknowledge likes and approvals. Filter by persona, platform or context_ref (a thread id).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "persona": { "type": "string" },
                    "platform": { "type": "string", "enum": PLATFORMS },
                    "context_ref": { "type": "string" },
                    "unresolved_only": { "type": "boolean", "description": "Default true" },
                    "limit": { "type": "integer", "description": "Default 100, max 500" }
                }
            }
        },
        {
            "name": "post_revise",
            "description": "Make a new revision of a post after review: give post_id and only the fields that change (hook, script, caption, hashtags, media_brief, language, title, body_or_path, kind, scheduled_at, status). The new post keeps the rest, gets revision + 1 and parent_post_id, starts review_status pending (back in the user's review feed), and resolves the parent's open reviews. Revise the latest revision only.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "post_id": { "type": "integer" },
                    "hook": { "type": "string" },
                    "script": { "type": "string" },
                    "caption": { "type": "string" },
                    "hashtags": { "type": "array", "items": { "type": "string" } },
                    "media_brief": { "type": "string" },
                    "language": { "type": "string" },
                    "title": { "type": "string" },
                    "body_or_path": { "type": "string" },
                    "kind": { "type": "string", "enum": POST_KINDS },
                    "scheduled_at": { "type": "string" },
                    "status": { "type": "string", "enum": POST_STATUSES },
                    "note": { "type": "string", "description": "What changed, stored on the resolved reviews" }
                },
                "required": ["post_id"]
            }
        },
        {
            "name": "post_review_resolve",
            "description": "Mark review feedback as handled without a new revision: {id} one review, or {post_id} all open reviews of a post; optional note and resolution_post_id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "description": "Review id" },
                    "post_id": { "type": "integer" },
                    "resolution_post_id": { "type": "integer" },
                    "note": { "type": "string" }
                }
            }
        },
        {
            "name": "metrics_add",
            "description": "Record channel metrics for a date: reach (by the channel's kpi_metric) and cost in the period ending on `date`, followers at `date` (a level), posts_published in the period. One row per channel per date; the same date again replaces it. Batch with rows: [{persona, platform, date, …}].",
            "inputSchema": {
                "type": "object",
                "properties": with(json!({
                    "date": { "type": "string", "description": "YYYY-MM-DD" },
                    "reach": { "type": "number" },
                    "followers": { "type": "number" },
                    "posts_published": { "type": "number" },
                    "cost": { "type": "number" },
                    "currency": { "type": "string" },
                    "rows": { "type": "array", "items": { "type": "object" } }
                }))
            }
        },
        {
            "name": "inventory_gaps",
            "description": "What inventory is missing per requirement (default rules: every persona needs 1 proxy for egress in its own country, 1 email for login, 1 phone for 2fa, 1 device_profile), plus warnings: geo_mismatch (proxy country ≠ persona country), capacity (item shared beyond its capacity), bad_status (bound item is broken/retired). Filter by type or scope; limit caps listed subjects per requirement.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": { "type": "string", "enum": ITEM_TYPES },
                    "scope": { "type": "string", "enum": ["persona", "channel"] },
                    "limit": { "type": "integer", "description": "Max missing subjects listed per requirement (default 20)" }
                }
            }
        }
    ])
}

/// Run a state tool for an agent; `None` when `name` isn't one of ours.
pub fn mcp_call(name: &str, args: &Value, actor: &str) -> Option<R<String>> {
    let method = match name {
        "state_query" => "query",
        "inventory_gaps" => "gaps",
        "channel_update" | "persona_update" | "inventory_add" | "inventory_update" | "inventory_bind" | "inventory_unbind"
        | "post_upsert" | "metrics_add" | "review_feedback" | "post_revise" | "post_review_resolve" => name,
        _ => return None,
    };
    let mut args = args.clone();
    if method == "gaps" {
        if args.get("limit").is_none() {
            args["limit"] = json!(20);
        }
        args["by_subject"] = json!(false);
    }
    Some(user().and_then(|db| db.call(actor, method, &args)).map(|v| serde_json::to_string_pretty(&v).unwrap_or_default()))
}

/// Appended to a local turn's data hint.
pub const AGENT_HINT: &str = "The Channels network (personas, channels, budgets, metrics, posts, inventory of proxies/phones/emails/device profiles and their bindings) lives in ~/AgentArea/state.db: read it with state_query (read-only SQL) and change it only with the typed agentarea_data tools (channel_update, persona_update, inventory_*, post_upsert, metrics_add); inventory_gaps shows what's missing. Before generating posts, call review_feedback for the user's verdicts on earlier ones and answer them with post_revise / post_review_resolve. ~/AgentArea/channels/export/*.json are read-only snapshots; SOUL.md files stay editable in the persona folders; format in ~/AgentArea/channels/SCHEMA.md. Secrets only as vault names (secret_ref), never values.";

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aa-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        // export files are 0444; the dir itself is writable, so removal works
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A fixture in the real layout: `n` personas × 7 platforms, fields as the
    /// generator wrote them.
    fn write_fixture(dir: &Path, n: usize) {
        let countries = ["DE", "PL", "IT"];
        let cities = ["Leipzig", "Krakow", "Milano"];
        let mut channels = Vec::new();
        for i in 0..n {
            let handle = format!("p{i:03}.test.city");
            let folder = dir.join(&handle);
            std::fs::create_dir_all(&folder).unwrap();
            let k = i % 3;
            std::fs::write(
                folder.join("config.json"),
                serde_json::to_string_pretty(&json!({
                    "name": format!("Persona {i}"), "emoji": "🔌", "niche": "Repair", "category": "Home Services & Garden",
                    "country": countries[k], "city": cities[k], "languages": ["Ukrainian", "English"],
                    "target_audience": "Homeowners", "references": [{"name": "X", "platform": "YouTube", "why": "y"}],
                    "platforms": PLATFORMS, "handle": handle, "hypothesis": "h",
                    "personality": {"ocean": {"openness": 3}, "valence": "edgy", "humor": "dry", "energy": "calm", "archetype": "Rebel", "likes": [], "dislikes": []}
                }))
                .unwrap(),
            )
            .unwrap();
            std::fs::write(folder.join("SOUL.md"), "---\nname: \"@x\"\n---\n# Soul\n").unwrap();
            std::fs::write(folder.join("plan.json"), "{}").unwrap();
            for (j, platform) in PLATFORMS.iter().enumerate() {
                let role = ROLES[j % 3];
                channels.push(json!({
                    "persona": handle, "platform": platform, "handle": "", "stage": "idea",
                    "role": role, "plan": "Posts daily",
                    "tasks": [{"type": "post", "cadence": "daily", "notes": ""}, {"type": "reply", "cadence": "daily", "notes": "n"}],
                    "kpi_metric": "video views, 30d", "kpi_target": null,
                    "budget": {"currency": "EUR", "proxy": 1.14, "account": 0.0, "content": 12.0, "ads": 0.0, "other": 0.0},
                    "spent": {"currency": "EUR", "proxy": 0, "account": 0, "content": if j == 0 { 3 } else { 0 }, "ads": 0, "other": 0},
                    "mcp": "", "network": {"egress": "", "ip": "", "geo": format!("{}, {}", cities[k], countries[k]), "device_profile": "", "url": ""},
                    "notes": "", "created": "2026-09-28", "updated": "2026-09-28"
                }));
            }
        }
        std::fs::write(dir.join("channels.json"), serde_json::to_string(&channels).unwrap()).unwrap();
        std::fs::write(
            dir.join("posts.json"),
            r#"[{"persona":"p000.test.city","platform":"tiktok","datetime":"2026-10-03 18:00","status":"scheduled","title":"First","text_or_file":"p000.test.city/posts/01.md"}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("metrics.csv"),
            "date,persona,platform,reach,cost,currency,followers,posts_published\n2026-10-01,p000.test.city,tiktok,18400,35,EUR,212,1\n",
        )
        .unwrap();
    }

    fn open_with_fixture(name: &str, n: usize) -> (PathBuf, StateDb, Value) {
        let root = temp_root(name);
        write_fixture(&root.join("channels"), n);
        let db = StateDb::open(&root).unwrap();
        let report = db.import_dir("import", &root.join("channels"), true).unwrap();
        (root, db, report)
    }

    #[test]
    fn migrates_from_empty_and_reopens() {
        let root = temp_root("migrate");
        let db = StateDb::open(&root).unwrap();
        let v: i64 = db.read(|c| c.query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        let reqs = db.call("user", "requirements", &json!({})).unwrap();
        assert_eq!(reqs.as_array().unwrap().len(), 4, "seeded requirements");
        let fk: i64 = db.read(|c| c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(fk, 1);
        let mode: String = db.read(|c| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(mode, "wal");
        drop(db);
        let db = StateDb::open(&root).unwrap(); // no re-run, no duplicate seeds
        assert_eq!(db.call("user", "requirements", &json!({})).unwrap().as_array().unwrap().len(), 4);
        cleanup(&root);
    }

    #[test]
    fn migration_2_turns_account_rows_into_accounts() {
        let root = temp_root("v2");
        {
            let mut conn = Connection::open(root.join("state.db")).unwrap();
            conn.execute_batch("CREATE TABLE schema_version (version INTEGER NOT NULL, applied_at TEXT NOT NULL)").unwrap();
            migrate_range(&mut conn, 0, 1).unwrap();
            conn.execute_batch(
                r#"INSERT INTO personas (handle, country, created_at, updated_at) VALUES ('p', 'PL', 'a', 'a');
                   INSERT INTO channels (persona, platform, created_at, updated_at) VALUES ('p', 'tiktok', 'a', 'a'), ('p', 'x', 'a', 'a');
                   INSERT INTO inventory_items (id, type, label, meta, created_at, updated_at) VALUES
                     (1, 'other', 'TikTok · acc', '{"kind":"account","platform":"tiktok","username":"acc","email_item":2}', 'a', 'a'),
                     (2, 'email', 'acc@outlook.com', '{}', 'a', 'a'),
                     (3, 'other', 'misc', 'not json', 'a', 'a');
                   INSERT INTO bindings (item_id, subject_type, subject_id, purpose, since) VALUES (2, 'persona', 'p', 'login', 'a');"#,
            )
            .unwrap();
        }
        let db = StateDb::open(&root).unwrap();
        let types = db.query_readonly("SELECT id, type FROM inventory_items ORDER BY id", &[], 10).unwrap();
        assert_eq!(types["rows"][0]["type"], "account");
        assert_eq!(types["rows"][1]["type"], "email");
        assert_eq!(types["rows"][2]["type"], "other");
        assert_eq!(db.call("user", "bindings", &json!({})).unwrap().as_array().unwrap().len(), 1, "bindings survive the rebuild");
        let fk: i64 = db.read(|c| c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(fk, 1);
        let audit = db.call("user", "audit", &json!({ "actor": "migration" })).unwrap();
        assert_eq!(audit.as_array().unwrap().len(), 1);
        // accounts bind to a channel of their platform only
        assert!(db.call("user", "inventory_bind", &json!({ "item_id": 1, "subject_type": "persona", "subject_id": "p" })).unwrap_err().contains("bind it to a channel"));
        assert!(db.call("user", "inventory_bind", &json!({ "item_id": 1, "subject_type": "channel", "subject_id": "p/x" })).unwrap_err().contains("tiktok account"));
        let r = db.call("user", "inventory_bind", &json!({ "item_id": 1, "subject_type": "channel", "subject_id": "p/tiktok" })).unwrap();
        assert_eq!(r["purpose"], "login");
        db.call("user", "requirement_add", &json!({ "platform": "tiktok", "type": "account", "scope": "channel" })).unwrap();
        let g = db.call("user", "gaps", &json!({ "type": "account" })).unwrap();
        assert_eq!(g["requirements"][0]["missing_count"], 0);
        // autobind matches platforms: a new x account goes to the x channel only
        db.call("user", "inventory_add", &json!({ "type": "account", "label": "X · a", "meta": { "platform": "x", "username": "a" } })).unwrap();
        let req = db.call("user", "requirement_add", &json!({ "platform": "*", "type": "account", "scope": "channel" })).unwrap();
        let r = db.call("user", "inventory_autobind", &json!({ "requirement_id": req["id"] })).unwrap();
        assert_eq!((r["bound"].as_i64(), r["unassigned_count"].as_i64()), (Some(1), Some(0)));
        cleanup(&root);
    }

    #[test]
    fn imports_the_legacy_layout_idempotently() {
        let (root, db, report) = open_with_fixture("import", 104);
        assert_eq!(report["personas"], 104);
        assert_eq!(report["channels"], 728);
        assert_eq!(report["tasks"], 728 * 2);
        assert_eq!(report["posts"], 1);
        assert_eq!(report["metrics"], 1);
        let s = db.call("user", "summary", &json!({})).unwrap();
        assert_eq!((s["personas"].as_i64(), s["channels"].as_i64()), (Some(104), Some(728)));
        // files moved, SOUL stays
        let ch = root.join("channels");
        assert!(!ch.join("channels.json").exists() && ch.join("_legacy/channels.json").is_file());
        assert!(!ch.join("p000.test.city/config.json").exists() && ch.join("_legacy/p000.test.city/config.json").is_file());
        assert!(ch.join("p000.test.city/SOUL.md").is_file());
        let p = db.call("user", "persona", &json!({ "handle": "p000.test.city" })).unwrap();
        assert_eq!(p["soul_path"], "p000.test.city/SOUL.md");
        assert_eq!(p["personality"]["archetype"], "Rebel");
        // spent: only the non-zero row; planned for all
        let budgets: i64 = db.read(|c| c.query_row("SELECT count(*) FROM budget WHERE kind = 'spent'", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(budgets, 104);
        // re-import of the same files (copied back) doesn't duplicate
        std::fs::copy(ch.join("_legacy/channels.json"), ch.join("channels.json")).unwrap();
        std::fs::copy(ch.join("_legacy/posts.json"), ch.join("posts.json")).unwrap();
        let again = db.import_dir("import", &ch, false).unwrap();
        assert_eq!(again["channels"], 728);
        let s = db.call("user", "summary", &json!({})).unwrap();
        assert_eq!((s["channels"].as_i64(), s["posts"].as_i64(), s["tasks"].as_i64()), (Some(728), Some(1), Some(728 * 2)));
        // nothing left to import
        std::fs::remove_file(ch.join("channels.json")).unwrap();
        std::fs::remove_file(ch.join("posts.json")).unwrap();
        let _ = std::fs::remove_file(ch.join("metrics.csv"));
        assert_eq!(db.import_dir("import", &ch, true).unwrap()["imported"], false);
        cleanup(&root);
    }

    /// The real files, copied: counts must be 104 / 728. Skipped when this
    /// machine has no Channels data (CI).
    #[test]
    fn imports_a_copy_of_the_real_files() {
        let Ok(home) = std::env::var("HOME") else { return };
        let base = PathBuf::from(home).join("AgentArea/channels");
        let src = [base.clone(), base.join("_legacy")].into_iter().find(|d| d.join("channels.json").is_file());
        let Some(src) = src else {
            eprintln!("no ~/AgentArea/channels data; skipping");
            return;
        };
        let root = temp_root("real");
        let dst = root.join("channels");
        std::fs::create_dir_all(&dst).unwrap();
        for f in ["channels.json", "posts.json", "metrics.csv"] {
            let _ = std::fs::copy(src.join(f), dst.join(f));
        }
        for entry in std::fs::read_dir(&src).unwrap().flatten() {
            let cfg = entry.path().join("config.json");
            if cfg.is_file() {
                let d = dst.join(entry.file_name());
                std::fs::create_dir_all(&d).unwrap();
                std::fs::copy(&cfg, d.join("config.json")).unwrap();
            }
        }
        let db = StateDb::open(&root).unwrap();
        let report = db.import_dir("import", &dst, false).unwrap();
        assert_eq!(report["personas"], 104, "{report}");
        assert_eq!(report["channels"], 728, "{report}");
        assert_eq!(report["stub_personas"], json!([]));
        cleanup(&root);
    }

    #[test]
    fn state_query_is_read_only() {
        let (root, db, _) = open_with_fixture("ro", 3);
        let ok = db.query_readonly("SELECT platform, count(*) AS n FROM channels GROUP BY platform", &[], 100).unwrap();
        assert_eq!(ok["row_count"], 7);
        let ok = db.query_readonly("-- comment\nWITH x AS (SELECT 1 AS a) SELECT a FROM x", &[], 10).unwrap();
        assert_eq!(ok["rows"][0]["a"], 1);
        let ok = db.query_readonly("SELECT name FROM pragma_table_info('channels')", &[], 100).unwrap();
        assert!(ok["row_count"].as_i64().unwrap() > 5);
        let ok = db.query_readonly("SELECT handle FROM personas WHERE country = ?1", &[json!("DE")], 100).unwrap();
        assert_eq!(ok["row_count"], 1);
        let capped = db.query_readonly("SELECT * FROM channels", &[], 5).unwrap();
        assert_eq!(capped["row_count"], 5);
        assert!(capped["truncated"].is_string());
        for bad in [
            "INSERT INTO personas (handle, created_at, updated_at) VALUES ('x', 'a', 'a')",
            "UPDATE channels SET stage = 'live'",
            "DELETE FROM channels",
            "DROP TABLE channels",
            "ATTACH DATABASE '/tmp/x.db' AS x",
            "PRAGMA journal_mode = DELETE",
            "PRAGMA foreign_keys = OFF",
            "WITH x AS (SELECT 1) INSERT INTO audit_log (at, actor, action, entity) SELECT 'a','b','c','d' FROM x",
            "SELECT 1; DELETE FROM channels",
            "SELECT load_extension('x')",
            "VACUUM",
            "CREATE TABLE t (a)",
            "REPLACE INTO personas (handle) VALUES ('x')",
        ] {
            assert!(db.query_readonly(bad, &[], 10).is_err(), "must reject: {bad}");
        }
        let n = db.call("user", "summary", &json!({})).unwrap();
        assert_eq!(n["channels"], 21, "nothing changed");
        // a runaway recursive query is stopped by the time limit / row cap
        let r = db.query_readonly("WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT x FROM c", &[], 10).unwrap();
        assert_eq!(r["row_count"], 10);
        cleanup(&root);
    }

    fn add(db: &StateDb, v: Value) -> i64 {
        db.call("user", "inventory_add", &v).unwrap()["id"].as_i64().unwrap()
    }

    #[test]
    fn bind_unbind_and_gaps() {
        let (root, db, _) = open_with_fixture("gaps", 3); // DE, PL, IT personas
        let g = db.call("user", "gaps", &json!({})).unwrap();
        let reqs = g["requirements"].as_array().unwrap();
        assert_eq!(reqs.len(), 4);
        assert!(reqs.iter().all(|r| r["missing_count"] == 3), "every persona misses everything");

        let de = add(&db, json!({ "type": "proxy", "label": "DE #1", "geo": "DE-BY", "capacity": 1, "cost_month": 8 }));
        let pl = add(&db, json!({ "type": "proxy", "label": "PL #1", "geo": "Krakow, PL", "capacity": 1 }));
        let r = db.call("user", "inventory_bind", &json!({ "item_id": de, "subject_type": "persona", "subject_id": "p000.test.city" })).unwrap();
        assert_eq!(r["purpose"], "egress", "default purpose from type");
        // idempotent
        let again = db.call("user", "inventory_bind", &json!({ "item_id": de, "subject_type": "persona", "subject_id": "p000.test.city" })).unwrap();
        assert_eq!(again["already"], true);
        // geo mismatch: the PL proxy on the IT persona
        db.call("user", "inventory_bind", &json!({ "item_id": pl, "subject_type": "persona", "subject_id": "p002.test.city" })).unwrap();
        let g = db.call("user", "gaps", &json!({ "type": "proxy" })).unwrap();
        assert_eq!(g["requirements"][0]["missing_count"], 1);
        assert_eq!(g["requirements"][0]["missing"][0]["subject_id"], "p001.test.city");
        let w: Vec<&Value> = g["warnings"].as_array().unwrap().iter().filter(|w| w["kind"] == "geo_mismatch").collect();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0]["persona_country"], "IT");

        // channel subjects accept persona/platform, bad subjects are refused
        let key = add(&db, json!({ "type": "api_key", "label": "TG bot", "secret_ref": "TG_BOT_TOKEN" }));
        let r = db.call("user", "inventory_bind", &json!({ "item_id": key, "subject_type": "channel", "subject_id": "p000.test.city/telegram" })).unwrap();
        assert_eq!(r["purpose"], "api");
        assert!(db.call("user", "inventory_bind", &json!({ "item_id": key, "subject_type": "persona", "subject_id": "nobody" })).is_err());
        assert!(db.call("user", "inventory_bind", &json!({ "item_id": key, "subject_type": "robot", "subject_id": "x" })).is_err());

        // unbind → the gap is back, history kept
        db.call("user", "inventory_unbind", &json!({ "item_id": de, "subject_type": "persona", "subject_id": "p000.test.city" })).unwrap();
        let g = db.call("user", "gaps", &json!({ "type": "proxy" })).unwrap();
        assert_eq!(g["requirements"][0]["missing_count"], 2);
        let all = db.call("user", "bindings", &json!({ "include_ended": true })).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 3);
        assert!(db.call("user", "inventory_unbind", &json!({ "binding_id": r["binding_id"] })).is_ok());
        assert!(db.call("user", "inventory_unbind", &json!({ "binding_id": r["binding_id"] })).is_err(), "already ended");

        // a broken item doesn't cover and can't be bound
        db.call("user", "inventory_update", &json!({ "id": pl, "set": { "status": "broken" } })).unwrap();
        let g = db.call("user", "gaps", &json!({ "type": "proxy" })).unwrap();
        assert_eq!(g["requirements"][0]["missing_count"], 3);
        assert!(g["warnings"].as_array().unwrap().iter().any(|w| w["kind"] == "bad_status"));
        assert!(db.call("user", "inventory_bind", &json!({ "item_id": pl, "subject_type": "persona", "subject_id": "p001.test.city" })).is_err());

        // autobind honours geo: only the DE persona gets the DE proxy
        let r = db.call("user", "inventory_autobind", &json!({ "requirement_id": g["requirements"][0]["id"] })).unwrap();
        assert_eq!(r["bound"], 1);
        assert_eq!(r["unassigned_count"], 2);
        let b = db.call("user", "bindings", &json!({ "item_id": de })).unwrap();
        assert_eq!(b[0]["subject_id"], "p000.test.city");

        // the snapshot shows it on the persona's channels
        let snap = db.call("user", "snapshot", &json!({})).unwrap();
        let tiktok = snap["channels"].as_array().unwrap().iter().find(|c| c["persona"] == "p000.test.city" && c["platform"] == "tiktok").unwrap().clone();
        assert_eq!(tiktok["network"]["geo"], "DE-BY");
        assert_eq!(tiktok["inventory"][0]["via"], "persona");
        assert_eq!(snap["inventory"].as_array().unwrap().iter().find(|i| i["id"] == key).unwrap()["secret"], "missing");
        cleanup(&root);
    }

    #[test]
    fn capacity_is_enforced_and_overflow_warned() {
        let (root, db, _) = open_with_fixture("cap", 3);
        let phone = add(&db, json!({ "type": "phone", "label": "+49 …01", "capacity": 2 }));
        for h in ["p000.test.city", "p001.test.city"] {
            db.call("user", "inventory_bind", &json!({ "item_id": phone, "subject_type": "persona", "subject_id": h })).unwrap();
        }
        let err = db.call("user", "inventory_bind", &json!({ "item_id": phone, "subject_type": "persona", "subject_id": "p002.test.city" })).unwrap_err();
        assert!(err.contains("capacity full"), "{err}");
        // the same subject for another purpose still fits
        db.call("user", "inventory_bind", &json!({ "item_id": phone, "subject_type": "persona", "subject_id": "p000.test.city", "purpose": "login" })).unwrap();
        // lowering capacity below use → warning, and gaps lists it
        let r = db.call("user", "inventory_update", &json!({ "id": phone, "set": { "capacity": 1 } })).unwrap();
        assert!(r["warning"].as_str().unwrap().contains("over its capacity"));
        let g = db.call("user", "gaps", &json!({})).unwrap();
        assert!(g["warnings"].as_array().unwrap().iter().any(|w| w["kind"] == "capacity" && w["used"] == 2));
        let sum = db.call("user", "inventory_summary", &json!({})).unwrap();
        assert_eq!(sum[0]["over_capacity"], 1);
        assert!(db.call("user", "inventory_update", &json!({ "id": phone, "set": { "capacity": 0 } })).is_err());
        cleanup(&root);
    }

    #[test]
    fn secrets_stay_out_of_the_database() {
        let (root, db, _) = open_with_fixture("secrets", 1);
        for bad in [
            json!({ "type": "proxy", "label": "x", "secret_ref": "hunter2 password" }),
            json!({ "type": "proxy", "label": "x", "meta": { "password": "hunter2" } }),
            json!({ "type": "proxy", "label": "x", "meta": { "proxy_token": "abc" } }),
            json!({ "type": "proxy", "label": "x", "meta": { "url": "http://user:pass@1.2.3.4:8080" } }),
        ] {
            assert!(db.call("user", "inventory_add", &bad).is_err(), "{bad}");
        }
        let ok = db.call("user", "inventory_add", &json!({ "type": "proxy", "label": "x", "secret_ref": "PROXY_DE_01", "meta": { "host": "1.2.3.4", "port": 8080, "ip": "5.6.7.8" } })).unwrap();
        assert_eq!(ok["secret_ref"], "PROXY_DE_01");
        // secret status comes from the vault's metadata file
        std::fs::write(root.join("secrets.json"), r#"[{"id":"a","name":"PROXY_DE_01","allowedHosts":[],"createdAt":1}]"#).unwrap();
        let inv = db.call("user", "inventory", &json!({})).unwrap();
        assert_eq!(inv[0]["secret"], "ok");
        cleanup(&root);
    }

    #[test]
    fn mutations_write_audit_rows() {
        let (root, db, _) = open_with_fixture("audit", 1);
        let before = db.call("user", "audit", &json!({})).unwrap().as_array().unwrap().len();
        assert_eq!(before, 1, "the import");
        db.call("agent:t1", "channel_update", &json!({ "persona": "p000.test.city", "platform": "tiktok",
            "set": { "stage": "setup", "handle_registered": "@p000", "tasks": [{"type": "post", "cadence": "3/week"}],
                     "budget": { "ads": 5 }, "spent": { "period": "2026-10", "content": 4 } } })).unwrap();
        // no-op update writes nothing
        db.call("user", "channel_update", &json!({ "persona": "p000.test.city", "platform": "tiktok", "set": { "stage": "setup" } })).unwrap();
        db.call("user", "persona_update", &json!({ "handle": "p000.test.city", "set": { "city": "Dresden", "references": [] } })).unwrap();
        db.call("user", "post_upsert", &json!({ "persona": "p000.test.city", "platform": "x", "title": "Hi", "status": "draft" })).unwrap();
        db.call("user", "metrics_add", &json!({ "rows": [{ "channel": "p000.test.city/x", "date": "2026-10-02", "reach": 10 }] })).unwrap();
        assert!(db.call("user", "channel_update", &json!({ "persona": "p000.test.city", "platform": "x", "set": { "stage": "flying" } })).is_err());
        assert!(db.call("user", "channel_update", &json!({ "persona": "p000.test.city", "platform": "x", "set": { "bogus": 1 } })).is_err());
        let log = db.call("user", "audit", &json!({})).unwrap();
        let log = log.as_array().unwrap();
        assert_eq!(log.len(), 5);
        let ch = log.iter().find(|r| r["entity"] == "channel").unwrap();
        assert_eq!(ch["actor"], "agent:t1");
        assert_eq!(ch["diff"]["stage"]["to"], "setup");
        assert!(ch["diff"]["spent:2026-10"].is_object());
        let c = db.call("user", "channel", &json!({ "persona": "p000.test.city", "platform": "tiktok" })).unwrap();
        assert_eq!(c["handle_registered"], "@p000");
        assert_eq!(c["tasks"].as_array().unwrap().len(), 1);
        let planned = c["budget"].as_array().unwrap().iter().find(|b| b["kind"] == "planned").unwrap().clone();
        assert_eq!((planned["ads"].as_f64(), planned["content"].as_f64()), (Some(5.0), Some(12.0)), "omitted keys kept");
        cleanup(&root);
    }

    #[test]
    fn exports_snapshots_after_changes() {
        let root = temp_root("export");
        write_fixture(&root.join("channels"), 2);
        let db = Arc::new(StateDb::open(&root).unwrap());
        db.start_exporter(Duration::from_millis(100));
        db.import_dir("import", &root.join("channels"), true).unwrap();
        let dir = db.export_dir();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !EXPORT_FILES.iter().all(|f| dir.join(f).is_file()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        for f in EXPORT_FILES {
            let body = std::fs::read_to_string(dir.join(f)).unwrap_or_else(|_| panic!("{f} missing"));
            serde_json::from_str::<Value>(&body).unwrap();
        }
        let channels: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("channels.json")).unwrap()).unwrap();
        assert_eq!(channels.as_array().unwrap().len(), 14);
        assert_eq!(channels[0]["budget"]["content"], 12.0, "legacy row shape");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("channels.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o222, 0, "read-only");
        }
        // a change re-exports (read-only files are replaced, not written into)
        db.call("user", "inventory_add", &json!({ "type": "email", "label": "a@b.c" })).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ok = false;
        while Instant::now() < deadline {
            let inv: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("inventory.json")).unwrap_or_default()).unwrap_or(json!([]));
            if inv.as_array().is_some_and(|a| a.len() == 1) {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(ok, "inventory.json re-exported");
        drop(db);
        cleanup(&root);
    }

    /// A consistent copy of the real ~/AgentArea/state.db at `root/state.db`,
    /// rolled back to schema v2 when the app already migrated it. `false` when
    /// this machine has no state.db (CI).
    fn copy_real_db_as_v2(root: &Path) -> bool {
        let Ok(home) = std::env::var("HOME") else { return false };
        let real = PathBuf::from(home).join("AgentArea/state.db");
        if !real.is_file() {
            eprintln!("no ~/AgentArea/state.db; skipping");
            return false;
        }
        let dst = root.join("state.db");
        let src = Connection::open_with_flags(&real, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        src.execute("VACUUM INTO ?1", [dst.to_string_lossy()]).unwrap();
        drop(src);
        let conn = Connection::open(&dst).unwrap();
        let v: i64 = conn.query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0)).unwrap();
        assert!(v >= 2, "real db at v{v}");
        if v >= 3 {
            conn.execute_batch(
                "PRAGMA foreign_keys = OFF;
                 BEGIN;
                 DROP TABLE post_reviews;
                 CREATE TABLE posts_v2 (
                   id           INTEGER PRIMARY KEY,
                   channel_id   INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
                   scheduled_at TEXT NOT NULL DEFAULT '',
                   status       TEXT NOT NULL DEFAULT 'draft' CHECK (status IN ('draft','scheduled','published')),
                   title        TEXT NOT NULL DEFAULT '',
                   body_or_path TEXT NOT NULL DEFAULT '',
                   created_at   TEXT NOT NULL,
                   updated_at   TEXT NOT NULL
                 );
                 INSERT INTO posts_v2 SELECT id, channel_id, scheduled_at, status, title, body_or_path, created_at, updated_at FROM posts;
                 DROP TABLE posts;
                 ALTER TABLE posts_v2 RENAME TO posts;
                 CREATE INDEX posts_channel ON posts(channel_id);
                 DELETE FROM schema_version WHERE version >= 3;
                 COMMIT;",
            )
            .unwrap();
        }
        true
    }

    /// Six sample drafts (plus one earlier revision) for the Review tab —
    /// only ever in a test copy, never in the real database.
    fn seed_review_samples(db: &StateDb) -> Vec<i64> {
        let samples = [
            json!({ "persona": "nadia_putzfee", "platform": "tiktok", "kind": "video_script", "language": "uk", "scheduled_at": "2026-10-02 18:00",
                "hook": "3 засоби для прибирання, які я більше ніколи не куплю",
                "script": "[в кадрі: полиця з побутовою хімією]\n1. «Універсальний» спрей — розводи на склі, плачу двічі.\n2. Дорогий засіб для духовки — сода + оцет роблять те саме.\n3. Ароматизовані серветки — залишають липкий шар.\n[в кадрі: мої 3 замінники]\nЗберігай, щоб не переплачувати в DM.",
                "caption": "Прибираю в Франкфурті 4 роки — ось на чому я перестала економити навпаки 🧽",
                "hashtags": ["#прибирання", "#Німеччина", "#putzen", "#лайфхак"], "media_brief": "Вертикальне відео 25 с, полиця з хімією в Rossmann, крупні плани рук, текст на екрані жовтим.",
                "context_ref": "thread-demo-1", "generated_by": "agent:thread-demo-1" }),
            json!({ "persona": "lesia_ksiegowa", "platform": "instagram", "kind": "carousel", "language": "uk", "scheduled_at": "2026-10-03 12:30",
                "hook": "JDG у Польщі: 5 витрат, які можна списати вже цього місяця",
                "caption": "Більшість моїх клієнтів з JDG недоотримують 300–600 zł на місяць, бо не знають, що можна вносити у витрати. Гортай ➡️ і зберігай на кінець місяця.\n\nПитання — в коментарі, відповідаю всім.",
                "hashtags": ["#JDG", "#księgowość", "#бухгалтер", "#Польща", "#ZUS"], "media_brief": "Карусель 6 слайдів 4:5: обкладинка з хуком, 5 слайдів по одній витраті (телефон, авто, ноутбук, курси, оренда кута), фінальний — CTA «запис на консультацію».",
                "generated_by": "batch:pl-oct-w1", "context_ref": "" }),
            json!({ "persona": "tania_qa_wro", "platform": "x", "kind": "text", "language": "en", "scheduled_at": "2026-10-03 09:00",
                "body_or_path": "Switched from teaching to QA at 34, in a new country, in my third language.\n\nThe thing nobody tells you: your first bug report matters more than your CV. Mine was 11 lines, had a video and steps anyone could repeat. That's what got me the offer.",
                "hashtags": ["#QA", "#careerswitch"], "generated_by": "batch:pl-oct-w1", "context_ref": "" }),
            json!({ "persona": "iryna_amt_help", "platform": "telegram", "kind": "telegram_post", "language": "ru", "scheduled_at": "2026-10-04 08:00",
                "title": "Письмо из Jobcenter: Einladung",
                "body_or_path": "📄 Пришло «Einladung» из Jobcenter — не паникуйте.\n\nЭто приглашение на встречу, не штраф. Что сделать:\n• проверьте дату и время вверху справа;\n• если не можете прийти — позвоните ДО встречи и попросите новый Termin;\n• возьмите паспорт, Aufenthaltstitel и само письмо.\n\nНе пришли без причины — могут урезать выплаты на 10%. Вопросы — в комментариях 👇",
                "context_ref": "thread-demo-2", "generated_by": "agent:thread-demo-2" }),
            json!({ "persona": "mykola_remont", "platform": "facebook", "kind": "text", "language": "uk", "scheduled_at": "2026-10-05 19:00",
                "body_or_path": "Шукаю у Вроцлаві 2 плиточників у бригаду 🧱\n\nОб'єкти: квартири в новобудовах, ванні під ключ. Оплата за м², розрахунок щотижня, житло допомагаємо знайти. Досвід від 2 років і свій інструмент.\n\nПишіть у приват або дзвоніть — покажу наші роботи.",
                "media_brief": "Фото готової ванної: великоформатна плитка, рівні шви.", "generated_by": "batch:pl-oct-w1", "context_ref": "" }),
        ];
        let mut ids = Vec::new();
        let mut used: Vec<String> = Vec::new();
        for mut s in samples {
            // the named persona, else any unused one with a channel on that platform
            let (want, platform) = (s["persona"].as_str().unwrap().to_string(), s["platform"].as_str().unwrap().to_string());
            let pick: Option<String> = db
                .read(|c| {
                    c.query_row(
                        "SELECT persona FROM channels WHERE platform = ?1 AND (persona = ?2 OR NOT EXISTS (SELECT 1 FROM channels WHERE persona = ?2 AND platform = ?1))
                         ORDER BY persona = ?2 DESC, persona",
                        params![platform, want],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(e)
                })
                .unwrap()
                .filter(|h: &String| !used.contains(h));
            let Some(h) = pick else { continue };
            used.push(h.clone());
            s["persona"] = json!(h);
            s["status"] = json!("draft");
            ids.push(db.call("agent:seed", "post_upsert", &s).unwrap()["id"].as_i64().unwrap());
        }
        // a short with a revision history: v1 got "needs changes", v2 answers it
        if db.read(|c| persona_exists(c, "vitalik_pizza")).unwrap() {
            let v1 = db.call("agent:thread-demo-1", "post_upsert", &json!({
                "persona": "vitalik_pizza", "platform": "youtube", "kind": "video_script", "language": "uk", "status": "draft", "scheduled_at": "2026-10-06 17:00",
                "hook": "Чому в Неаполі тісто для піци стоїть 24 години",
                "script": "Тісто — це борошно, вода, сіль і дріжджі.\nМи залишаємо його на 24 години в холодильнику.\nТак воно смачніше.",
                "caption": "Секрет неаполітанської піци 🍕", "hashtags": ["#піца", "#Неаполь"],
                "media_brief": "Кухня піцерії, тісто в контейнерах, руки піцайоло." })).unwrap()["id"].as_i64().unwrap();
            db.call("user", "post_review_add", &json!({ "post_id": v1, "verdict": "needs_changes", "comment": "Хук слабкий — додай конкретику і цифру. І покажи тісто на 2-й і 24-й годині." })).unwrap();
            let v2 = db.call("agent:thread-demo-1", "post_revise", &json!({ "post_id": v1,
                "hook": "2 години vs 24 години: ось що відбувається з тістом для піци",
                "script": "[в кадрі: два шматки тіста поруч]\nЛіворуч — тісто, яке стояло 2 години. Праворуч — 24 години в холодильнику.\nБачите бульбашки? Це ферментація: тісто легше, скоринка хрусткіша.\nУ нашій піцерії в Неаполі інакше не роблять.\nЗберігай — на вихідних спробуєш сам.",
                "note": "stronger hook with the 2 h vs 24 h comparison" })).unwrap()["post"]["id"].as_i64().unwrap();
            ids.push(v2);
        }
        ids
    }

    #[test]
    fn migration_3_on_a_copy_of_the_real_db() {
        let root = temp_root("real-v3");
        if !copy_real_db_as_v2(&root) {
            return cleanup(&root);
        }
        let counts = |c: &Connection| -> R<(i64, i64, i64)> {
            let n = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0)).map_err(e);
            Ok((n("SELECT count(*) FROM personas")?, n("SELECT count(*) FROM channels")?, n("SELECT count(*) FROM posts")?))
        };
        let before = {
            let c = Connection::open(root.join("state.db")).unwrap();
            counts(&c).unwrap()
        };
        let db = StateDb::open(&root).unwrap();
        let v: i64 = db.read(|c| c.query_row("SELECT max(version) FROM schema_version", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(v, 3);
        assert_eq!(db.read(counts).unwrap(), before, "no rows lost");
        let fk: i64 = db.read(|c| c.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0)).map_err(e)).unwrap();
        assert_eq!(fk, 0);
        // old rows got the defaults
        let bad = db.query_readonly("SELECT count(*) AS n FROM posts WHERE review_status <> 'pending' OR revision <> 1 OR kind <> 'text'", &[], 1).unwrap();
        assert_eq!(bad["rows"][0]["n"], 0);

        let ids = seed_review_samples(&db);
        assert!(ids.len() >= 6 || before.0 < 60, "seeded {ids:?}");
        let feed = db.call("user", "review_feed", &json!({ "review_status": "pending" })).unwrap();
        assert_eq!(feed["counts"]["pending"].as_i64().unwrap() as usize, before.2 as usize + ids.len());
        if let Some(first) = ids.first() {
            db.call("user", "post_review_add", &json!({ "post_id": first, "verdict": "like", "comment": "Сильний хук" })).unwrap();
        }
        db.export_now().unwrap();
        let posts: Value = serde_json::from_str(&std::fs::read_to_string(db.export_dir().join("posts.json")).unwrap()).unwrap();
        assert!(posts.as_array().unwrap().iter().any(|p| !p["reviews"].as_array().unwrap().is_empty()));
        // Screenshot fixture for the Review tab: AA_REVIEW_FIXTURE_DIR=<dir> keeps the export.
        if let Ok(out) = std::env::var("AA_REVIEW_FIXTURE_DIR") {
            std::fs::create_dir_all(&out).unwrap();
            for f in EXPORT_FILES {
                let _ = std::fs::remove_file(Path::new(&out).join(f));
                std::fs::copy(db.export_dir().join(f), Path::new(&out).join(f)).unwrap();
            }
        }
        drop(db);
        cleanup(&root);
    }

    #[test]
    fn reviews_add_resolve_and_revise() {
        let (root, db, _) = open_with_fixture("review", 2);
        // an agent's post routes feedback to its thread
        let p = db.call("agent:th-9", "post_upsert", &json!({ "persona": "p000.test.city", "platform": "tiktok", "kind": "video_script",
            "hook": "Hook", "script": "line 1\nline 2", "hashtags": "#a #b", "language": "UK" })).unwrap();
        let id = p["id"].as_i64().unwrap();
        assert_eq!((p["generated_by"].as_str(), p["context_ref"].as_str()), (Some("agent:th-9"), Some("th-9")));
        assert_eq!(p["hashtags"], json!(["#a", "#b"]));
        assert_eq!((p["language"].as_str(), p["revision"].as_i64(), p["review_status"].as_str()), (Some("uk"), Some(1), Some("pending")));
        assert!(db.call("user", "post_upsert", &json!({ "id": id, "kind": "essay" })).is_err());
        assert!(db.call("user", "post_upsert", &json!({ "id": id, "language": "ukrainian" })).is_err());
        assert!(db.call("user", "post_upsert", &json!({ "id": id, "review_status": "approved" })).is_err(), "status moves through reviews only");
        let user_post = db.call("user", "post_upsert", &json!({ "persona": "p001.test.city", "platform": "x", "body_or_path": "hi" })).unwrap();
        assert_eq!((user_post["generated_by"].as_str(), user_post["context_ref"].as_str()), (Some("user"), Some("")));

        // verdicts set the status; the latest wins; undo falls back
        assert!(db.call("user", "post_review_add", &json!({ "post_id": id, "verdict": "meh" })).is_err());
        let r1 = db.call("user", "post_review_add", &json!({ "post_id": id, "verdict": "like" })).unwrap();
        assert_eq!(r1["post"]["review_status"], "liked");
        let r2 = db.call("plugin:channels", "post_review_add", &json!({ "post_id": id, "verdict": "needs_changes", "comment": "Shorter hook" })).unwrap();
        assert_eq!(r2["post"]["review_status"], "needs_changes");
        assert_eq!(r2["review"]["actor"], "plugin:channels");
        let u = db.call("user", "post_review_undo", &json!({ "id": r2["review"]["id"] })).unwrap();
        assert_eq!(u["post"]["review_status"], "liked");
        let r2 = db.call("user", "post_review_add", &json!({ "post_id": id, "verdict": "needs_changes", "comment": "Shorter hook" })).unwrap();
        let d = db.call("user", "post_review_delivered", &json!({ "id": r2["review"]["id"], "to": "th-9" })).unwrap();
        assert!(d["delivered_at"].is_string());
        assert_eq!(d["delivered_to"], "th-9");

        // the agent sees both open reviews
        let fb = db.call("agent:th-9", "review_feedback", &json!({ "context_ref": "th-9" })).unwrap();
        assert_eq!(fb["count"], 1);
        assert_eq!(fb["posts"][0]["reviews"].as_array().unwrap().len(), 2);

        // revise: linked copy, parent's reviews resolved, back to pending
        let rv = db.call("agent:th-9", "post_revise", &json!({ "post_id": id, "hook": "Short hook" })).unwrap();
        let v2 = rv["post"]["id"].as_i64().unwrap();
        assert_eq!(rv["resolved"], 2);
        assert_eq!((rv["post"]["revision"].as_i64(), rv["post"]["parent_post_id"].as_i64()), (Some(2), Some(id)));
        assert_eq!((rv["post"]["hook"].as_str(), rv["post"]["script"].as_str()), (Some("Short hook"), Some("line 1\nline 2")));
        assert_eq!((rv["post"]["review_status"].as_str(), rv["post"]["context_ref"].as_str()), (Some("pending"), Some("th-9")));
        assert!(db.call("user", "post_revise", &json!({ "post_id": id })).unwrap_err().contains("newer revision"));
        assert!(db.call("user", "post_review_undo", &json!({ "id": r2["review"]["id"] })).is_err(), "resolved reviews stay");
        let fb = db.call("agent:th-9", "review_feedback", &json!({})).unwrap();
        assert_eq!(fb["count"], 0, "nothing open");
        let all = db.call("agent:th-9", "review_feedback", &json!({ "unresolved_only": false })).unwrap();
        assert_eq!(all["posts"][0]["reviews"][1]["resolution_post_id"], v2);

        // history: both revisions, oldest first, with reviews
        let h = db.call("user", "post_history", &json!({ "id": v2 })).unwrap();
        assert_eq!(h.as_array().unwrap().iter().map(|p| p["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![id, v2]);
        assert_eq!(h[0]["reviews"].as_array().unwrap().len(), 2);
        assert_eq!(h[0]["child_post_id"], v2);

        // resolve by id / by post
        let r3 = db.call("user", "post_review_add", &json!({ "post_id": v2, "verdict": "approve" })).unwrap();
        db.call("agent:th-9", "post_review_resolve", &json!({ "id": r3["review"]["id"], "note": "noted" })).unwrap();
        assert!(db.call("agent:th-9", "post_review_resolve", &json!({ "id": r3["review"]["id"] })).is_err());
        assert_eq!(db.call("user", "post_review_resolve", &json!({ "post_id": v2 })).unwrap()["resolved"], 0);

        // the feed: head revisions only, counts, filters, include_ids
        let f = db.call("user", "review_feed", &json!({})).unwrap();
        let ids: Vec<i64> = f["posts"].as_array().unwrap().iter().map(|p| p["id"].as_i64().unwrap()).collect();
        assert!(ids.contains(&v2) && !ids.contains(&id), "{ids:?}");
        assert_eq!((f["counts"]["approved"].as_i64(), f["counts"]["pending"].as_i64(), f["counts"]["total"].as_i64()), (Some(1), Some(2), Some(3)));
        let f = db.call("user", "review_feed", &json!({ "review_status": "pending" })).unwrap();
        assert_eq!(f["posts"].as_array().unwrap().len(), 2);
        let f = db.call("user", "review_feed", &json!({ "review_status": "pending", "include_ids": [v2] })).unwrap();
        assert_eq!(f["posts"].as_array().unwrap().len(), 3);
        let f = db.call("user", "review_feed", &json!({ "platform": "tiktok", "country": "de" })).unwrap();
        assert_eq!(f["posts"].as_array().unwrap().len(), 2, "p000 is DE: its tiktok fixture post and v2");
        assert_eq!(f["posts"][1]["reviews"][0]["verdict"], "approve");
        let f = db.call("user", "review_feed", &json!({ "from": "2026-10-01", "to": "2026-10-31" })).unwrap();
        assert_eq!(f["posts"].as_array().unwrap().len(), 1, "only the dated fixture post");
        assert!(db.call("user", "review_feed", &json!({ "review_status": "great" })).is_err());

        // audited; deleting a post drops its reviews and unlinks its revision
        let log = db.call("user", "audit", &json!({ "entity": "post_review" })).unwrap();
        assert!(log.as_array().unwrap().len() >= 6);
        db.call("user", "post_delete", &json!({ "id": id })).unwrap();
        let left = db.query_readonly("SELECT count(*) AS n FROM post_reviews WHERE post_id = ?1", &[json!(id)], 1).unwrap();
        assert_eq!(left["rows"][0]["n"], 0);
        assert!(db.call("user", "post_history", &json!({ "id": v2 })).unwrap()[0]["parent_post_id"].is_null());
        cleanup(&root);
    }

    #[test]
    fn review_tables_stay_read_only_for_queries() {
        let (root, db, _) = open_with_fixture("review-ro", 1);
        let p = db.call("user", "post_upsert", &json!({ "persona": "p000.test.city", "platform": "x", "body_or_path": "hi" })).unwrap();
        db.call("user", "post_review_add", &json!({ "post_id": p["id"], "verdict": "dislike", "comment": "no" })).unwrap();
        let ok = db.query_readonly("SELECT verdict, comment FROM post_reviews", &[], 10).unwrap();
        assert_eq!(ok["rows"][0]["verdict"], "dislike");
        let ok = db.query_readonly("SELECT json_array_length(hashtags) AS n, review_status FROM posts", &[], 10).unwrap();
        assert_eq!(ok["rows"][ok["row_count"].as_i64().unwrap() as usize - 1]["review_status"], "disliked");
        for bad in [
            "INSERT INTO post_reviews (post_id, verdict, at) VALUES (1, 'like', 'a')",
            "UPDATE post_reviews SET resolved_at = 'x'",
            "DELETE FROM post_reviews",
            "UPDATE posts SET review_status = 'approved'",
            "WITH x AS (SELECT 1) UPDATE posts SET revision = 9",
        ] {
            assert!(db.query_readonly(bad, &[], 10).is_err(), "must reject: {bad}");
        }
        assert_eq!(db.query_readonly("SELECT count(*) AS n FROM post_reviews", &[], 1).unwrap()["rows"][0]["n"], 1);
        // agents reach the review tools through mcp_call's names
        for t in ["review_feedback", "post_revise", "post_review_resolve"] {
            assert!(mcp_tools().as_array().unwrap().iter().any(|d| d["name"] == t), "{t} listed");
        }
        cleanup(&root);
    }

    #[test]
    fn parses_geo_countries_and_csv() {
        assert_eq!(geo_country("DE").as_deref(), Some("DE"));
        assert_eq!(geo_country("de-by").as_deref(), Some("DE"));
        assert_eq!(geo_country("Munich, DE").as_deref(), Some("DE"));
        assert_eq!(geo_country("Munich"), None);
        assert_eq!(geo_country(""), None);
        let rows = parse_csv("a,b\n\"x, y\",\"q\"\"q\"\n\n1,2");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["a"], "x, y");
        assert_eq!(rows[0]["b"], "q\"q");
        assert!(check_select("  /* hi */ select 1").is_ok());
        assert!(check_select("select 1;").is_ok());
        assert!(check_select("select 1; select 2").is_err());
    }
}
