//! SQLite access: migrations (WAL mode), domain/history/notify_queue queries.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, NaiveDate, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension};

use crate::config::DomainEntry;
use crate::domain;
use crate::notify::{Payload, QueuedEvent, Summary};
use crate::state::{Decision, Previous, State};

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Each entry upgrades the schema by one version (`PRAGMA user_version`).
const MIGRATIONS: &[&str] = &[
    r#"
CREATE TABLE domains (
  id                    INTEGER PRIMARY KEY,
  name                  TEXT NOT NULL UNIQUE,
  display_name          TEXT NOT NULL,
  note                  TEXT,
  tags                  TEXT,
  state                 TEXT NOT NULL DEFAULT 'Bilinmiyor',
  registrar             TEXT,
  created_at_reg        TEXT,
  expires_at            TEXT,
  pending_delete_since  TEXT,
  last_status_codes     TEXT,
  last_nameservers      TEXT,
  last_checked_at       TEXT,
  next_check_at         TEXT NOT NULL,
  error_count           INTEGER NOT NULL DEFAULT 0,
  notified_thresholds   TEXT,
  pending_free_confirm  INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE history (
  id         INTEGER PRIMARY KEY,
  domain_id  INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  at         TEXT NOT NULL,
  from_state TEXT,
  to_state   TEXT NOT NULL,
  detail     TEXT
);

CREATE TABLE notify_queue (
  id         INTEGER PRIMARY KEY,
  channel    TEXT NOT NULL,
  payload    TEXT NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 0,
  next_try   TEXT NOT NULL
);

CREATE INDEX idx_domains_next_check_at ON domains(next_check_at);
"#,
    r#"
CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT
);
"#,
];

/// `meta` key holding the local date (`YYYY-MM-DD`) of the last enqueued daily summary.
const META_LAST_SUMMARY: &str = "last_summary_date";

pub struct Db {
    conn: Connection,
}

/// A domain to insert: `name` is punycode, `display_name` the readable form.
pub struct NewDomain<'a> {
    pub name: &'a str,
    pub display_name: &'a str,
    pub note: Option<&'a str>,
    pub tags: &'a [String],
}

pub struct DomainRow {
    pub id: i64,
    pub display_name: String,
    pub state: String,
    pub expires_at: Option<String>,
    pub next_check_at: String,
}

/// One `notify_queue` row that is due for delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRow {
    pub id: i64,
    pub channel: String,
    pub payload: String,
    pub attempts: u32,
}

pub struct HistoryRow {
    pub at: String,
    pub from_state: Option<String>,
    pub to_state: String,
    pub detail: Option<String>,
}

fn format_time(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn now_rfc3339() -> String {
    format_time(Utc::now())
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("veritabanı dizini oluşturulamadı: {}", dir.display()))?;
        }
        let mut conn = Connection::open(path)
            .with_context(|| format!("veritabanı açılamadı: {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .context("WAL modu etkinleştirilemedi")?;
        migrate(&mut conn).context("veritabanı şeması güncellenemedi")?;
        Ok(Db { conn })
    }

    /// Inserts the config's `[[domain]]` entries that are not yet stored; never deletes.
    pub fn sync_config_domains(&self, entries: &[DomainEntry]) -> Result<()> {
        for entry in entries {
            let normalized = domain::normalize(&entry.name)?;
            self.add(&NewDomain {
                name: &normalized.ascii,
                display_name: &normalized.display,
                note: entry.note.as_deref(),
                tags: &entry.tags,
            })?;
        }
        Ok(())
    }

    /// Returns `true` if the domain was newly inserted, `false` if it already existed.
    pub fn add(&self, domain: &NewDomain) -> Result<bool> {
        let tags = serde_json::to_string(domain.tags)?;
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO domains (name, display_name, note, tags, next_check_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                domain.name,
                domain.display_name,
                domain.note,
                tags,
                now_rfc3339()
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Returns `true` if a domain was removed.
    pub fn remove(&self, name: &str) -> Result<bool> {
        let removed = self
            .conn
            .execute("DELETE FROM domains WHERE name = ?1", params![name])?;
        Ok(removed == 1)
    }

    /// Lists domains ordered by name, optionally restricted to one state.
    pub fn list(&self, state: Option<&str>) -> Result<Vec<DomainRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, display_name, state, expires_at, next_check_at FROM domains
             WHERE ?1 IS NULL OR state = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map(params![state], domain_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn get(&self, name: &str) -> Result<Option<DomainRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, display_name, state, expires_at, next_check_at FROM domains
                 WHERE name = ?1",
                params![name],
                domain_row,
            )
            .optional()?)
    }

    /// Loads what the state machine needs from a domain row; `None` if the domain is not listed.
    pub fn previous(&self, name: &str) -> Result<Option<Previous>> {
        type Raw = (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
            i64,
        );
        let raw: Option<Raw> = self
            .conn
            .query_row(
                "SELECT state, expires_at, pending_delete_since, notified_thresholds,
                        error_count, pending_free_confirm FROM domains WHERE name = ?1",
                params![name],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((state, expires_at, since, thresholds, error_count, confirm)) = raw else {
            return Ok(None);
        };
        Ok(Some(Previous {
            state: state
                .parse()
                .with_context(|| format!("{name}: veritabanında bozuk durum"))?,
            expires_at: parse_time(expires_at.as_deref())?,
            pending_delete_since: parse_time(since.as_deref())?,
            notified_thresholds: match thresholds.as_deref().filter(|t| !t.trim().is_empty()) {
                Some(json) => serde_json::from_str(json)
                    .with_context(|| format!("{name}: notified_thresholds okunamadı"))?,
                None => Vec::new(),
            },
            error_count: u32::try_from(error_count).unwrap_or(0),
            pending_free_confirm: confirm != 0,
        }))
    }

    /// Names of domains whose `next_check_at` has passed, earliest first.
    pub fn due_domains(&self, now: DateTime<Utc>, limit: usize) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM domains WHERE next_check_at <= ?1
             ORDER BY next_check_at, id LIMIT ?2",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![format_time(now), limit], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Persists one check result atomically: the domain row, the optional history entry and one
    /// queue row per event and channel. Returns `false` (writing nothing) if the domain was
    /// removed in the meantime.
    pub fn apply_decision(
        &self,
        name: &str,
        decision: &Decision,
        now: DateTime<Utc>,
        next_check_at: DateTime<Utc>,
        channels: &[&str],
    ) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let found: Option<(i64, String)> = tx
            .query_row(
                "SELECT id, display_name FROM domains WHERE name = ?1",
                params![name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((id, display_name)) = found else {
            return Ok(false);
        };
        let now_text = format_time(now);

        tx.execute(
            "UPDATE domains SET state = ?1, pending_delete_since = ?2, notified_thresholds = ?3,
                    error_count = ?4, pending_free_confirm = ?5, last_checked_at = ?6,
                    next_check_at = ?7 WHERE id = ?8",
            params![
                decision.state.as_str(),
                decision.pending_delete_since.map(format_time),
                serde_json::to_string(&decision.notified_thresholds)?,
                decision.error_count,
                decision.pending_free_confirm,
                now_text,
                format_time(next_check_at),
                id,
            ],
        )?;
        if let Some(reg) = &decision.registration {
            tx.execute(
                "UPDATE domains SET registrar = ?1, created_at_reg = ?2, expires_at = ?3,
                        last_status_codes = ?4, last_nameservers = ?5 WHERE id = ?6",
                params![
                    reg.registrar,
                    reg.registered.map(format_time),
                    reg.expires.map(format_time),
                    serde_json::to_string(&reg.statuses)?,
                    serde_json::to_string(&reg.nameservers)?,
                    id,
                ],
            )?;
        }
        if let Some(entry) = &decision.history {
            tx.execute(
                "INSERT INTO history (domain_id, at, from_state, to_state, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    now_text,
                    entry.from.as_str(),
                    entry.to.as_str(),
                    entry.detail
                ],
            )?;
        }
        for event in &decision.events {
            let payload = serde_json::to_string(&Payload::Event(QueuedEvent {
                domain: display_name.clone(),
                event: event.clone(),
                expires: decision.registration.as_ref().and_then(|r| r.expires),
                registrar: decision
                    .registration
                    .as_ref()
                    .and_then(|r| r.registrar.clone()),
            }))?;
            for channel in channels {
                tx.execute(
                    "INSERT INTO notify_queue (channel, payload, attempts, next_try)
                     VALUES (?1, ?2, 0, ?3)",
                    params![channel, payload, now_text],
                )?;
            }
        }
        tx.commit()?;
        Ok(true)
    }

    /// Queue rows with `id > after_id` whose `next_try` has passed, oldest first.
    pub fn due_notifications(
        &self,
        now: DateTime<Utc>,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<QueueRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, channel, payload, attempts FROM notify_queue
             WHERE next_try <= ?1 AND id > ?2 ORDER BY id LIMIT ?3",
        )?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = stmt.query_map(params![format_time(now), after_id, limit], |row| {
            Ok(QueueRow {
                id: row.get(0)?,
                channel: row.get(1)?,
                payload: row.get(2)?,
                attempts: u32::try_from(row.get::<_, i64>(3)?).unwrap_or(u32::MAX),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Removes a delivered row.
    pub fn delete_notification(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM notify_queue WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Postpones a row to `next_try`; `count_attempt` bumps `attempts` (rate limits do not).
    pub fn reschedule_notification(
        &self,
        id: i64,
        next_try: DateTime<Utc>,
        count_attempt: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE notify_queue SET next_try = ?1, attempts = attempts + ?2 WHERE id = ?3",
            params![format_time(next_try), i64::from(count_attempt), id],
        )?;
        Ok(())
    }

    /// Local date of the last enqueued daily summary, if any.
    pub fn last_summary_date(&self) -> Result<Option<NaiveDate>> {
        let value: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![META_LAST_SUMMARY],
                |r| r.get(0),
            )
            .optional()?;
        value
            .flatten()
            .map(|text| {
                text.parse::<NaiveDate>()
                    .with_context(|| format!("meta.{META_LAST_SUMMARY} bozuk: {text:?}"))
            })
            .transpose()
    }

    /// Snapshot of the watch list for the summary of local day `date`; `expiry_window_days` is
    /// how far ahead "expiring soon" looks.
    pub fn summary(
        &self,
        date: NaiveDate,
        now: DateTime<Utc>,
        expiry_window_days: u32,
    ) -> Result<Summary> {
        let mut stmt = self
            .conn
            .prepare("SELECT display_name, state, expires_at FROM domains ORDER BY name")?;
        let rows: Vec<(String, String, Option<String>)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let horizon = now + ChronoDuration::days(i64::from(expiry_window_days));
        let mut per_state = [0usize; State::ALL.len()];
        let mut attention = Vec::new();
        let mut expiring = Vec::new();
        for (name, state_text, expires) in &rows {
            let state: State = state_text
                .parse()
                .with_context(|| format!("{name}: veritabanında bozuk durum"))?;
            if let Some(slot) = State::ALL.iter().position(|s| *s == state) {
                per_state[slot] += 1;
            }
            if state != State::Aktif {
                attention.push((name.clone(), state));
            }
            if let Some(expires) =
                parse_time(expires.as_deref())?.filter(|e| *e > now && *e <= horizon)
            {
                expiring.push((name.clone(), expires));
            }
        }
        expiring.sort_by_key(|(_, expires)| *expires);
        let counts = State::ALL
            .into_iter()
            .zip(per_state)
            .filter(|(_, count)| *count > 0)
            .collect();
        Ok(Summary {
            date,
            total: rows.len(),
            counts,
            attention,
            expiring,
            expiry_window_days,
        })
    }

    /// Enqueues `summary` for every channel and records its date, atomically.
    pub fn enqueue_summary(
        &self,
        summary: &Summary,
        channels: &[&str],
        now: DateTime<Utc>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let payload = serde_json::to_string(&Payload::Summary(summary.clone()))?;
        let now_text = format_time(now);
        for channel in channels {
            tx.execute(
                "INSERT INTO notify_queue (channel, payload, attempts, next_try)
                 VALUES (?1, ?2, 0, ?3)",
                params![channel, payload, now_text],
            )?;
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![META_LAST_SUMMARY, summary.date.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// State change history of a domain, newest first.
    pub fn history(&self, domain_id: i64) -> Result<Vec<HistoryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT at, from_state, to_state, detail FROM history
             WHERE domain_id = ?1 ORDER BY at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![domain_id], |row| {
            Ok(HistoryRow {
                at: row.get(0)?,
                from_state: row.get(1)?,
                to_state: row.get(2)?,
                detail: row.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn parse_time(text: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    text.map(|t| {
        DateTime::parse_from_rfc3339(t)
            .map(|parsed| parsed.with_timezone(&Utc))
            .with_context(|| format!("geçersiz zaman damgası: {t:?}"))
    })
    .transpose()
}

fn domain_row(row: &rusqlite::Row) -> rusqlite::Result<DomainRow> {
    Ok(DomainRow {
        id: row.get(0)?,
        display_name: row.get(1)?,
        state: row.get(2)?,
        expires_at: row.get(3)?,
        next_check_at: row.get(4)?,
    })
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (version, sql) in (1..).zip(MIGRATIONS).skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::state::Event;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    /// Unique temp DB path that removes the DB and its WAL files on drop.
    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new() -> TempDb {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("domain-gozcu-test-{}-{n}.db", std::process::id());
            TempDb(std::env::temp_dir().join(name))
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = self.0.clone().into_os_string();
                path.push(suffix);
                let _ = std::fs::remove_file(path);
            }
        }
    }

    fn new_domain<'a>(name: &'a str, tags: &'a [String]) -> NewDomain<'a> {
        NewDomain {
            name,
            display_name: name,
            note: None,
            tags,
        }
    }

    #[test]
    fn open_twice_is_idempotent() {
        let tmp = TempDb::new();
        drop(Db::open(&tmp.0).unwrap());
        let db = Db::open(&tmp.0).unwrap();
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn open_creates_missing_parent_dir() {
        let tmp = TempDb::new();
        let dir = tmp.0.with_extension("d");
        let path = dir.join("nested.db");
        Db::open(&path).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_list_get_remove() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        let tags = vec!["a".to_string()];
        assert!(db.add(&new_domain("b.com", &tags)).unwrap());
        assert!(!db.add(&new_domain("b.com", &tags)).unwrap());
        assert!(db.add(&new_domain("a.com", &[])).unwrap());

        let names: Vec<_> = db
            .list(None)
            .unwrap()
            .into_iter()
            .map(|r| r.display_name)
            .collect();
        assert_eq!(names, ["a.com", "b.com"]);
        assert_eq!(db.list(Some("Bilinmiyor")).unwrap().len(), 2);
        assert!(db.list(Some("Aktif")).unwrap().is_empty());

        let row = db.get("b.com").unwrap().unwrap();
        assert_eq!(row.state, "Bilinmiyor");
        assert!(row.expires_at.is_none());
        assert!(chrono::DateTime::parse_from_rfc3339(&row.next_check_at).is_ok());
        let stored_tags: String = db
            .conn
            .query_row("SELECT tags FROM domains WHERE name='b.com'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored_tags, r#"["a"]"#);

        assert!(db.get("zzz.com").unwrap().is_none());
        assert!(db.remove("b.com").unwrap());
        assert!(!db.remove("b.com").unwrap());
    }

    #[test]
    fn previous_parses_stored_columns() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        assert!(db.previous("a.com").unwrap().is_none());
        db.add(&new_domain("a.com", &[])).unwrap();
        let fresh = db.previous("a.com").unwrap().unwrap();
        assert_eq!(fresh, Previous::default());

        db.conn
            .execute(
                "UPDATE domains SET state='PendingDelete', expires_at='2026-07-01T00:00:00Z',
                 pending_delete_since='2026-06-01T10:00:00Z', notified_thresholds='[30,7]',
                 error_count=3, pending_free_confirm=1 WHERE name='a.com'",
                [],
            )
            .unwrap();
        let prev = db.previous("a.com").unwrap().unwrap();
        assert_eq!(prev.state, State::PendingDelete);
        assert_eq!(
            prev.expires_at.unwrap().to_rfc3339(),
            "2026-07-01T00:00:00+00:00"
        );
        assert!(prev.pending_delete_since.is_some());
        assert_eq!(prev.notified_thresholds, [30, 7]);
        assert_eq!(prev.error_count, 3);
        assert!(prev.pending_free_confirm);

        db.conn
            .execute("UPDATE domains SET state='Bogus'", [])
            .unwrap();
        assert!(db.previous("a.com").is_err());
    }

    #[test]
    fn history_is_newest_first_and_cascades() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        db.add(&new_domain("a.com", &[])).unwrap();
        let id = db.get("a.com").unwrap().unwrap().id;
        for (at, from, to) in [
            ("2026-01-01T00:00:00Z", None, "Aktif"),
            ("2026-02-01T00:00:00Z", Some("Aktif"), "Redemption"),
        ] {
            db.conn
                .execute(
                    "INSERT INTO history (domain_id, at, from_state, to_state) VALUES (?1,?2,?3,?4)",
                    params![id, at, from, to],
                )
                .unwrap();
        }
        let rows = db.history(id).unwrap();
        assert_eq!(rows[0].to_state, "Redemption");
        assert_eq!(rows[0].from_state.as_deref(), Some("Aktif"));
        assert_eq!(rows[1].to_state, "Aktif");
        assert!(rows[1].detail.is_none());
        assert!(!rows[0].at.is_empty());

        db.remove("a.com").unwrap();
        assert!(db.history(id).unwrap().is_empty());
    }

    #[test]
    fn sync_config_domains_inserts_without_deleting() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        db.add(&new_domain("manual.com", &[])).unwrap();
        let entries = vec![DomainEntry {
            name: "https://www.Örnek.com.tr/".into(),
            note: Some("n".into()),
            tags: vec![],
        }];
        db.sync_config_domains(&entries).unwrap();
        db.sync_config_domains(&entries).unwrap();
        let names: Vec<_> = db
            .list(None)
            .unwrap()
            .into_iter()
            .map(|r| r.display_name)
            .collect();
        assert_eq!(names, ["manual.com", "örnek.com.tr"]);
    }

    fn decision(state: State) -> Decision {
        Decision {
            state,
            next_check_in: chrono::Duration::hours(1),
            pending_free_confirm: false,
            pending_delete_since: None,
            notified_thresholds: vec![30],
            error_count: 2,
            registration: None,
            events: Vec::new(),
            history: None,
        }
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn count(db: &Db, table: &str) -> i64 {
        db.conn
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn apply_decision_writes_row_history_and_queue() {
        use crate::check::Registration;
        use crate::state::HistoryEntry;

        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        db.add(&NewDomain {
            name: "xn--rnek-5qa.com",
            display_name: "örnek.com",
            note: None,
            tags: &[],
        })
        .unwrap();
        let now = at("2026-06-01T12:00:00Z");
        let next = at("2026-06-02T12:00:00Z");

        let mut d = decision(State::Bosta);
        d.pending_delete_since = Some(at("2026-05-30T00:00:00Z"));
        d.registration = Some(Registration {
            statuses: vec!["ok".into()],
            registered: Some(at("2020-01-01T00:00:00Z")),
            expires: Some(at("2027-01-01T00:00:00Z")),
            registrar: Some("Reg Inc".into()),
            nameservers: vec!["ns1.example.net".into()],
            ..Registration::default()
        });
        d.history = Some(HistoryEntry {
            from: State::Aktif,
            to: State::Bosta,
            detail: "x".into(),
        });
        d.events = vec![Event::BecameFree, Event::ExpiryWarning { days_left: 7 }];

        assert!(db
            .apply_decision("xn--rnek-5qa.com", &d, now, next, &["discord", "telegram"])
            .unwrap());

        type Row = (
            String,
            String,
            i64,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
        );
        let row: Row = db
            .conn
            .query_row(
                "SELECT state, notified_thresholds, error_count, last_checked_at, next_check_at,
                        registrar, expires_at, last_status_codes, last_nameservers, pending_delete_since
                 FROM domains",
                [],
                |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?))
                },
            )
            .unwrap();
        assert_eq!(row.0, "Bosta");
        assert_eq!(row.1, "[30]");
        assert_eq!(row.2, 2);
        assert_eq!(row.3, "2026-06-01T12:00:00Z");
        assert_eq!(row.4, "2026-06-02T12:00:00Z");
        assert_eq!(row.5, "Reg Inc");
        assert_eq!(row.6, "2027-01-01T00:00:00Z");
        assert_eq!(row.7, r#"["ok"]"#);
        assert_eq!(row.8, r#"["ns1.example.net"]"#);
        assert_eq!(row.9, "2026-05-30T00:00:00Z");

        let id = db.get("xn--rnek-5qa.com").unwrap().unwrap().id;
        let history = db.history(id).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].from_state.as_deref(), Some("Aktif"));
        assert_eq!(history[0].to_state, "Bosta");

        assert_eq!(count(&db, "notify_queue"), 4);
        let (channel, payload, attempts, next_try): (String, String, i64, String) = db
            .conn
            .query_row(
                "SELECT channel, payload, attempts, next_try FROM notify_queue ORDER BY id LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (channel.as_str(), attempts, next_try.as_str()),
            ("discord", 0, "2026-06-01T12:00:00Z")
        );
        let queued: Payload = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            queued,
            Payload::Event(QueuedEvent {
                domain: "örnek.com".into(),
                event: Event::BecameFree,
                expires: Some(at("2027-01-01T00:00:00Z")),
                registrar: Some("Reg Inc".into()),
            })
        );
    }

    #[test]
    fn apply_decision_without_registration_keeps_registry_fields() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        db.add(&new_domain("a.com", &[])).unwrap();
        db.conn
            .execute("UPDATE domains SET registrar='Keep'", [])
            .unwrap();
        let now = at("2026-06-01T12:00:00Z");
        assert!(db
            .apply_decision("a.com", &decision(State::Aktif), now, now, &[])
            .unwrap());
        let registrar: String = db
            .conn
            .query_row("SELECT registrar FROM domains", [], |r| r.get(0))
            .unwrap();
        assert_eq!(registrar, "Keep");
        assert_eq!(count(&db, "history"), 0);
        assert_eq!(count(&db, "notify_queue"), 0);
    }

    #[test]
    fn apply_decision_on_removed_domain_is_a_noop() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        let now = at("2026-06-01T12:00:00Z");
        let mut d = decision(State::Aktif);
        d.events = vec![Event::BecameFree];
        assert!(!db
            .apply_decision("gone.com", &d, now, now, &["discord"])
            .unwrap());
        assert_eq!(count(&db, "notify_queue"), 0);
        assert_eq!(count(&db, "history"), 0);
    }

    #[test]
    fn queued_event_round_trips_every_event() {
        let events = [
            Event::StateChanged {
                from: State::Aktif,
                to: State::Redemption,
                detail: "d".into(),
            },
            Event::Retaken {
                registrar: Some("R".into()),
            },
            Event::Retaken { registrar: None },
            Event::PendingDelete {
                estimated_drop: at("2026-06-06T12:00:00Z"),
            },
            Event::BecameFree,
            Event::ExpiryWarning { days_left: 7 },
            Event::Renewed {
                new_expires: at("2028-01-01T00:00:00Z"),
            },
            Event::Unreachable {
                error_count: 5,
                reason: "zaman aşımı".into(),
            },
        ];
        for event in events {
            let queued = Payload::Event(QueuedEvent {
                domain: "örnek.com".into(),
                event,
                expires: None,
                registrar: None,
            });
            let json = serde_json::to_string(&queued).unwrap();
            assert_eq!(
                serde_json::from_str::<Payload>(&json).unwrap(),
                queued,
                "{json}"
            );
        }
        let json = serde_json::to_string(&Event::StateChanged {
            from: State::Aktif,
            to: State::Bosta,
            detail: String::new(),
        })
        .unwrap();
        assert!(
            json.contains(r#""from":"Aktif""#) && json.contains(r#""to":"Bosta""#),
            "{json}"
        );
    }

    #[test]
    fn due_domains_filters_and_orders_by_next_check() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        for (name, next) in [
            ("late.com", "2026-06-01T11:00:00Z"),
            ("early.com", "2026-06-01T09:00:00Z"),
            ("exact.com", "2026-06-01T12:00:00Z"),
            ("future.com", "2026-06-01T12:00:01Z"),
        ] {
            db.add(&new_domain(name, &[])).unwrap();
            db.conn
                .execute(
                    "UPDATE domains SET next_check_at=?1 WHERE name=?2",
                    params![next, name],
                )
                .unwrap();
        }
        let now = at("2026-06-01T12:00:00Z");
        assert_eq!(
            db.due_domains(now, 10).unwrap(),
            ["early.com", "late.com", "exact.com"]
        );
        assert_eq!(db.due_domains(now, 2).unwrap(), ["early.com", "late.com"]);
        assert!(db
            .due_domains(at("2026-06-01T08:00:00Z"), 10)
            .unwrap()
            .is_empty());
    }

    fn queue_one(db: &Db, channel: &str, next_try: &str) -> i64 {
        db.conn
            .execute(
                "INSERT INTO notify_queue (channel, payload, attempts, next_try) VALUES (?1, '{}', 0, ?2)",
                params![channel, next_try],
            )
            .unwrap();
        db.conn.last_insert_rowid()
    }

    #[test]
    fn due_notifications_filters_by_time_and_orders_by_id() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        let late = queue_one(&db, "discord", "2026-06-01T11:00:00Z");
        let future = queue_one(&db, "discord", "2026-06-01T12:00:01Z");
        let exact = queue_one(&db, "telegram", "2026-06-01T12:00:00Z");
        let now = at("2026-06-01T12:00:00Z");
        let due = db.due_notifications(now, 0, 10).unwrap();
        assert_eq!(due.iter().map(|r| r.id).collect::<Vec<_>>(), [late, exact]);
        assert_eq!(due[1].channel, "telegram");
        assert_eq!(due[0].attempts, 0);
        assert_eq!(db.due_notifications(now, 0, 1).unwrap().len(), 1);
        let paged = db.due_notifications(now, late, 10).unwrap();
        assert_eq!(paged.iter().map(|r| r.id).collect::<Vec<_>>(), [exact]);
        assert!(!due.iter().any(|r| r.id == future));
    }

    #[test]
    fn delete_and_reschedule_notifications() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        let a = queue_one(&db, "discord", "2026-06-01T11:00:00Z");
        let b = queue_one(&db, "discord", "2026-06-01T11:00:00Z");
        let now = at("2026-06-01T12:00:00Z");

        db.reschedule_notification(a, at("2026-06-01T12:05:00Z"), false)
            .unwrap();
        db.reschedule_notification(b, at("2026-06-01T12:10:00Z"), true)
            .unwrap();
        db.reschedule_notification(b, at("2026-06-01T12:20:00Z"), true)
            .unwrap();
        assert!(db.due_notifications(now, 0, 10).unwrap().is_empty());

        let later = db
            .due_notifications(at("2026-06-01T13:00:00Z"), 0, 10)
            .unwrap();
        assert_eq!(
            later.iter().map(|r| (r.id, r.attempts)).collect::<Vec<_>>(),
            [(a, 0), (b, 2)]
        );

        db.delete_notification(a).unwrap();
        assert_eq!(count(&db, "notify_queue"), 1);
    }

    #[test]
    fn migration_v2_creates_meta_and_keeps_v1() {
        let tmp = TempDb::new();
        {
            let mut conn = Connection::open(&tmp.0).unwrap();
            let tx = conn.transaction().unwrap();
            tx.execute_batch(MIGRATIONS[0]).unwrap();
            tx.pragma_update(None, "user_version", 1).unwrap();
            tx.commit().unwrap();
        }
        let db = Db::open(&tmp.0).unwrap();
        assert_eq!(count(&db, "meta"), 0);
        let version: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
    }

    #[test]
    fn summary_counts_attention_and_expiring() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        let now = at("2026-06-01T12:00:00Z");
        for (name, state, expires) in [
            ("a.com", "Aktif", Some("2026-06-10T00:00:00Z")),
            ("b.com", "Aktif", Some("2027-06-10T00:00:00Z")),
            ("c.com", "Redemption", Some("2026-05-01T00:00:00Z")),
            ("d.com", "BitisYakin", Some("2026-06-05T00:00:00Z")),
            ("e.com", "Bosta", None),
        ] {
            db.add(&new_domain(name, &[])).unwrap();
            db.conn
                .execute(
                    "UPDATE domains SET state = ?1, expires_at = ?2 WHERE name = ?3",
                    params![state, expires, name],
                )
                .unwrap();
        }
        let date = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let summary = db.summary(date, now, 30).unwrap();
        assert_eq!(summary.total, 5);
        assert_eq!(
            summary.counts,
            [
                (State::Aktif, 2),
                (State::BitisYakin, 1),
                (State::Redemption, 1),
                (State::Bosta, 1)
            ]
        );
        assert_eq!(
            summary.attention,
            [
                ("c.com".to_string(), State::Redemption),
                ("d.com".to_string(), State::BitisYakin),
                ("e.com".to_string(), State::Bosta),
            ]
        );
        let expiring: Vec<_> = summary.expiring.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(expiring, ["d.com", "a.com"]);
        assert_eq!(summary.expiry_window_days, 30);
    }

    #[test]
    fn enqueue_summary_queues_per_channel_and_records_date() {
        let tmp = TempDb::new();
        let db = Db::open(&tmp.0).unwrap();
        assert_eq!(db.last_summary_date().unwrap(), None);
        let now = at("2026-06-01T12:00:00Z");
        let date = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let summary = db.summary(date, now, 30).unwrap();
        db.enqueue_summary(&summary, &["discord", "telegram"], now)
            .unwrap();
        assert_eq!(count(&db, "notify_queue"), 2);
        assert_eq!(db.last_summary_date().unwrap(), Some(date));

        let next = NaiveDate::from_ymd_opt(2026, 6, 2).unwrap();
        db.enqueue_summary(&db.summary(next, now, 30).unwrap(), &["discord"], now)
            .unwrap();
        assert_eq!(db.last_summary_date().unwrap(), Some(next));
        assert_eq!(count(&db, "meta"), 1);

        let payload: String = db
            .conn
            .query_row(
                "SELECT payload FROM notify_queue ORDER BY id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Payload>(&payload).unwrap(),
            Payload::Summary(summary)
        );
    }
}
