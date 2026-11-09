//! The persistent notification queue worker, retry/backoff policy and the daily summary trigger.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration as StdDuration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, Utc};
use chrono_tz::Tz;
use tokio::sync::watch;
use tokio::time::{interval, MissedTickBehavior};

use super::{format_payload, Notifier, Payload, SendError};
use crate::config::Config;
use crate::db::{Db, QueueRow};

const POLL_INTERVAL: StdDuration = StdDuration::from_secs(5);
/// Delivery attempts per pass.
const BATCH: usize = 20;
const BACKOFF_BASE_SECS: i64 = 30;
const BACKOFF_CAP_SECS: i64 = 3600;
const BACKOFF_JITTER: f64 = 0.2;
/// Exponent limit that keeps `2^attempts` far from overflow; the cap is reached long before.
const MAX_BACKOFF_EXPONENT: u32 = 16;

/// What to do with a queue row after a delivery attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Delete,
    Retry {
        at: DateTime<Utc>,
        count_attempt: bool,
    },
}

/// `30 s * 2^attempts`, capped at one hour, with +-20% jitter (`r` in [-1, 1]).
pub fn backoff(attempts: u32, r: f64) -> Duration {
    let base = BACKOFF_BASE_SECS
        .saturating_mul(1 << attempts.min(MAX_BACKOFF_EXPONENT))
        .min(BACKOFF_CAP_SECS);
    let factor = 1.0 + r.clamp(-1.0, 1.0) * BACKOFF_JITTER;
    Duration::milliseconds((base as f64 * 1000.0 * factor).round() as i64)
}

/// Maps a send result to the row's fate: delivered rows go away, 429 waits `retry_after`
/// without counting an attempt, every other failure backs off exponentially.
pub fn plan(result: &Result<u16, SendError>, attempts: u32, now: DateTime<Utc>, r: f64) -> Action {
    match result {
        Ok(_) => Action::Delete,
        Err(SendError::RateLimited { retry_after }) => Action::Retry {
            at: now + Duration::seconds(retry_after.as_secs_f64().ceil() as i64),
            count_attempt: false,
        },
        Err(SendError::Failed { .. }) => Action::Retry {
            at: now + backoff(attempts, r),
            count_attempt: true,
        },
    }
}

/// The local day whose summary is due, if any: today, once `at` has passed in `tz` and no summary
/// was recorded for today yet. A service started after `at` therefore still sends it.
pub fn summary_due(
    now: DateTime<Utc>,
    tz: Tz,
    at: NaiveTime,
    last_sent: Option<NaiveDate>,
) -> Option<NaiveDate> {
    let local = now.with_timezone(&tz);
    let today = local.date_naive();
    (local.time() >= at && last_sent.is_none_or(|sent| sent < today)).then_some(today)
}

struct SummarySchedule {
    tz: Tz,
    at: NaiveTime,
    window_days: u32,
}

pub struct Worker {
    db: Mutex<Db>,
    notifier: Notifier,
    channels: Vec<&'static str>,
    summary: Option<SummarySchedule>,
    /// Channels that are rate limited or down; their rows wait until the given time.
    blocked_until: HashMap<String, DateTime<Utc>>,
}

impl Worker {
    pub fn new(config: &Config, db: Db, notifier: Notifier) -> Result<Worker> {
        let alerts = &config.alerts;
        let summary = if alerts.daily_summary {
            Some(SummarySchedule {
                tz: notifier.tz,
                at: NaiveTime::parse_from_str(&alerts.daily_summary_time, "%H:%M").map_err(
                    |_| anyhow!("gunluk_ozet_saat geçersiz: {:?}", alerts.daily_summary_time),
                )?,
                window_days: alerts
                    .expiry_thresholds_days
                    .iter()
                    .copied()
                    .max()
                    .unwrap_or(0),
            })
        } else {
            None
        };
        Ok(Worker {
            db: Mutex::new(db),
            channels: super::enabled_channels(config),
            notifier,
            summary,
            blocked_until: HashMap::new(),
        })
    }

    fn db(&self) -> Result<MutexGuard<'_, Db>> {
        self.db
            .lock()
            .map_err(|_| anyhow!("veritabanı kilidi bozuldu"))
    }

    /// Polls the queue until `stop` flips (or its sender is dropped). A send in progress always
    /// finishes; the loop only checks for shutdown between rows.
    pub async fn run(mut self, mut stop: watch::Receiver<bool>) {
        let mut ticker = interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = stop.changed() => {}
            }
            if *stop.borrow() || stop.has_changed().is_err() {
                break;
            }
            let now = Utc::now();
            if let Err(err) = self.enqueue_summary_if_due(now) {
                tracing::warn!("günlük özet kuyruğa alınamadı: {err:#}");
            }
            if let Err(err) = self.deliver_due(now, &stop).await {
                tracing::warn!("bildirim kuyruğu işlenemedi: {err:#}");
            }
        }
        tracing::info!("bildirim işçisi durdu");
    }

    /// Enqueues today's summary for every enabled channel when it is due.
    pub fn enqueue_summary_if_due(&self, now: DateTime<Utc>) -> Result<()> {
        let Some(schedule) = self.summary.as_ref().filter(|_| !self.channels.is_empty()) else {
            return Ok(());
        };
        let db = self.db()?;
        let last_sent = db.last_summary_date()?;
        let Some(date) = summary_due(now, schedule.tz, schedule.at, last_sent) else {
            return Ok(());
        };
        let summary = db.summary(date, now, schedule.window_days)?;
        db.enqueue_summary(&summary, &self.channels, now)?;
        tracing::info!("günlük özet kuyruğa alındı: {date}");
        Ok(())
    }

    /// One pass over the due rows, oldest first, one send at a time.
    pub async fn deliver_due(
        &mut self,
        now: DateTime<Utc>,
        stop: &watch::Receiver<bool>,
    ) -> Result<()> {
        let mut attempted = 0;
        let mut cursor = 0;
        let mut disabled_logged: HashSet<String> = HashSet::new();
        while attempted < BATCH {
            let rows = self.db()?.due_notifications(now, cursor, BATCH)?;
            let Some(last) = rows.last() else { break };
            cursor = last.id;
            for row in rows {
                if *stop.borrow() {
                    return Ok(());
                }
                if !self.notifier.is_enabled(&row.channel) {
                    if disabled_logged.insert(row.channel.clone()) {
                        tracing::debug!(
                            "{} kanalı kapalı, bekleyen satırlar atlanıyor",
                            row.channel
                        );
                    }
                    continue;
                }
                if self
                    .blocked_until
                    .get(&row.channel)
                    .is_some_and(|until| now < *until)
                {
                    continue;
                }
                self.deliver(&row, now).await?;
                attempted += 1;
            }
        }
        Ok(())
    }

    async fn deliver(&mut self, row: &QueueRow, now: DateTime<Utc>) -> Result<()> {
        let r = rand::random_range(-1.0..=1.0);
        let result = match serde_json::from_str::<Payload>(&row.payload) {
            Ok(payload) => {
                let message = format_payload(&payload, self.notifier.tz, Utc::now());
                self.notifier.send(&row.channel, &message).await
            }
            Err(err) => {
                tracing::warn!(
                    "bildirim #{} okunamadı, sonra tekrar denenecek: {err}",
                    row.id
                );
                self.apply(
                    row,
                    Action::Retry {
                        at: now + backoff(row.attempts, r),
                        count_attempt: true,
                    },
                )?;
                return Ok(());
            }
        };
        let action = plan(&result, row.attempts, now, r);
        match (&result, action) {
            (Ok(status), _) => {
                tracing::info!(
                    "bildirim gönderildi: {} #{} (HTTP {status})",
                    row.channel,
                    row.id
                );
            }
            (Err(err), Action::Retry { at, .. }) => {
                tracing::warn!(
                    "bildirim gönderilemedi: {} #{} deneme {}: {err}; sonraki deneme {}",
                    row.channel,
                    row.id,
                    row.attempts + 1,
                    at.format("%H:%M:%S UTC")
                );
                if err.is_channel_wide() {
                    self.blocked_until.insert(row.channel.clone(), at);
                }
            }
            (Err(_), Action::Delete) => {}
        }
        self.apply(row, action)
    }

    fn apply(&self, row: &QueueRow, action: Action) -> Result<()> {
        let db = self.db()?;
        match action {
            Action::Delete => db.delete_notification(row.id),
            Action::Retry { at, count_attempt } => {
                db.reschedule_notification(row.id, at, count_attempt)
            }
        }
        .with_context(|| format!("bildirim #{} güncellenemedi", row.id))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::TimeZone;

    use super::*;
    use crate::notify::mock::{Canned, MockServer};
    use crate::notify::Summary;

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TempDb(std::path::PathBuf);

    impl TempDb {
        fn new() -> TempDb {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("domain-gozcu-queue-test-{}-{n}.db", std::process::id());
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

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn istanbul() -> Tz {
        "Europe/Istanbul".parse().unwrap()
    }

    fn nine() -> NaiveTime {
        NaiveTime::from_hms_opt(9, 0, 0).unwrap()
    }

    #[test]
    fn backoff_doubles_and_caps_at_one_hour() {
        let secs = |attempts| backoff(attempts, 0.0).num_seconds();
        assert_eq!([secs(0), secs(1), secs(2), secs(3)], [30, 60, 120, 240]);
        assert_eq!(secs(7), 3600);
        assert_eq!(secs(u32::MAX), 3600);
    }

    #[test]
    fn backoff_jitter_stays_within_20_percent() {
        assert_eq!(backoff(1, 1.0).num_seconds(), 72);
        assert_eq!(backoff(1, -1.0).num_seconds(), 48);
        assert_eq!(backoff(1, 9.0).num_seconds(), 72);
    }

    #[test]
    fn plan_covers_success_rate_limit_and_failure() {
        let now = at(2026, 6, 1, 12, 0);
        assert_eq!(plan(&Ok(204), 3, now, 0.0), Action::Delete);

        let limited = Err(SendError::RateLimited {
            retry_after: StdDuration::from_millis(2500),
        });
        assert_eq!(
            plan(&limited, 3, now, 0.0),
            Action::Retry {
                at: now + Duration::seconds(3),
                count_attempt: false
            }
        );

        let failed = Err(SendError::Failed {
            status: Some(500),
            reason: String::new(),
        });
        assert_eq!(
            plan(&failed, 2, now, 0.0),
            Action::Retry {
                at: now + Duration::seconds(120),
                count_attempt: true
            }
        );
    }

    #[test]
    fn summary_due_waits_for_the_configured_time() {
        let last = NaiveDate::from_ymd_opt(2026, 6, 1);
        // 05:59 UTC is 08:59 in Istanbul: not yet.
        assert_eq!(
            summary_due(at(2026, 6, 2, 5, 59), istanbul(), nine(), last),
            None
        );
        // 06:00 UTC is 09:00 in Istanbul: due, for the local day.
        assert_eq!(
            summary_due(at(2026, 6, 2, 6, 0), istanbul(), nine(), last),
            NaiveDate::from_ymd_opt(2026, 6, 2)
        );
    }

    #[test]
    fn summary_due_is_once_per_local_day() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 2);
        assert_eq!(
            summary_due(at(2026, 6, 2, 20, 0), istanbul(), nine(), today),
            None
        );
        assert_eq!(
            summary_due(at(2026, 6, 2, 20, 0), istanbul(), nine(), None),
            today
        );
        // A service started after the configured time still sends today's summary.
        assert_eq!(
            summary_due(
                at(2026, 6, 2, 17, 30),
                istanbul(),
                nine(),
                NaiveDate::from_ymd_opt(2026, 5, 1)
            ),
            today
        );
    }

    #[test]
    fn summary_due_uses_local_midnight_not_utc_midnight() {
        let utc: Tz = "UTC".parse().unwrap();
        let midnight = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        let last = NaiveDate::from_ymd_opt(2026, 6, 1);
        // 21:30 UTC on Jun 1 is already Jun 2 00:30 in Istanbul, but still Jun 1 in UTC.
        let now = at(2026, 6, 1, 21, 30);
        assert_eq!(
            summary_due(now, istanbul(), midnight, last),
            NaiveDate::from_ymd_opt(2026, 6, 2)
        );
        assert_eq!(summary_due(now, utc, midnight, last), None);
        // 23:59 local on Jun 1 (20:59 UTC) belongs to Jun 1 and was already sent.
        assert_eq!(
            summary_due(at(2026, 6, 1, 20, 59), istanbul(), midnight, last),
            None
        );
    }

    fn config_for(discord_url: Option<String>, telegram: bool) -> Config {
        let mut config = Config::default();
        config.notify.discord.enabled = discord_url.is_some();
        config.notify.discord.webhook_url = discord_url;
        config.notify.telegram.enabled = telegram;
        if telegram {
            config.notify.telegram.bot_token = Some("t".into());
            config.notify.telegram.chat_id = Some("1".into());
        }
        config
    }

    fn worker(tmp: &TempDb, config: &Config) -> Worker {
        let db = Db::open(&tmp.0).unwrap();
        Worker::new(config, db, Notifier::new(config).unwrap()).unwrap()
    }

    fn queue_summaries(worker: &Worker, channel: &str, count: usize, now: DateTime<Utc>) {
        let db = worker.db().unwrap();
        for _ in 0..count {
            let date = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
            let summary: Summary = db.summary(date, now, 30).unwrap();
            db.enqueue_summary(&summary, &[channel], now).unwrap();
        }
    }

    /// `(id, attempts)` of the rows that are due at `time`.
    fn due_at(worker: &Worker, time: DateTime<Utc>) -> Vec<(i64, u32)> {
        let rows = worker
            .db()
            .unwrap()
            .due_notifications(time, 0, 100)
            .unwrap();
        rows.into_iter().map(|r| (r.id, r.attempts)).collect()
    }

    fn running() -> (watch::Sender<bool>, watch::Receiver<bool>) {
        watch::channel(false)
    }

    #[tokio::test]
    async fn success_deletes_the_row_and_sends_the_formatted_message() {
        let server = MockServer::start(vec![Canned::status(204)]).await;
        let tmp = TempDb::new();
        let mut w = worker(&tmp, &config_for(Some(server.url("/hook")), false));
        let now = at(2026, 6, 1, 12, 0);
        queue_summaries(&w, "discord", 1, now);
        let (_tx, rx) = running();

        w.deliver_due(now, &rx).await.unwrap();
        assert!(w
            .db()
            .unwrap()
            .due_notifications(now + Duration::days(1), 0, 10)
            .unwrap()
            .is_empty());
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].body.contains("Günlük özet"),
            "{}",
            requests[0].body
        );
    }

    #[tokio::test]
    async fn rate_limit_reschedules_without_counting_an_attempt() {
        let server = MockServer::start(vec![Canned::json(429, r#"{"retry_after": 2}"#)]).await;
        let tmp = TempDb::new();
        let mut w = worker(&tmp, &config_for(Some(server.url("/hook")), false));
        let now = at(2026, 6, 1, 12, 0);
        queue_summaries(&w, "discord", 2, now);
        let (_tx, rx) = running();

        w.deliver_due(now, &rx).await.unwrap();
        // The channel is blocked after the 429, so the second row was not even tried.
        assert_eq!(server.requests().len(), 1);
        // Only the untouched second row is due right away; the first one waits the 2 s, with
        // its attempt counter unchanged.
        assert_eq!(due_at(&w, now + Duration::seconds(1)), [(2, 0)]);
        assert_eq!(due_at(&w, now + Duration::seconds(2)), [(1, 0), (2, 0)]);
    }

    #[tokio::test]
    async fn failure_backs_off_and_counts_the_attempt() {
        let server = MockServer::start(vec![Canned::json(500, "{}")]).await;
        let tmp = TempDb::new();
        let mut w = worker(&tmp, &config_for(Some(server.url("/hook")), false));
        let now = at(2026, 6, 1, 12, 0);
        queue_summaries(&w, "discord", 1, now);
        let (_tx, rx) = running();

        w.deliver_due(now, &rx).await.unwrap();
        // 30 s backoff with +-20% jitter.
        assert!(due_at(&w, now + Duration::seconds(23)).is_empty());
        assert_eq!(due_at(&w, now + Duration::seconds(37)), [(1, 1)]);

        // Not due yet, so a second pass does not send again.
        w.deliver_due(now + Duration::seconds(10), &rx)
            .await
            .unwrap();
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn rows_of_a_disabled_channel_are_left_alone() {
        let server = MockServer::start(vec![Canned::status(204)]).await;
        let tmp = TempDb::new();
        let mut w = worker(&tmp, &config_for(Some(server.url("/hook")), false));
        let now = at(2026, 6, 1, 12, 0);
        queue_summaries(&w, "telegram", 25, now);
        queue_summaries(&w, "discord", 1, now);
        let (_tx, rx) = running();

        w.deliver_due(now, &rx).await.unwrap();
        assert_eq!(server.requests().len(), 1);
        let left = due_at(&w, now);
        assert_eq!(left.len(), 25);
        assert!(left.iter().all(|(_, attempts)| *attempts == 0));
    }

    #[tokio::test]
    async fn stop_signal_halts_delivery() {
        let server = MockServer::start(vec![Canned::status(204)]).await;
        let tmp = TempDb::new();
        let mut w = worker(&tmp, &config_for(Some(server.url("/hook")), false));
        let now = at(2026, 6, 1, 12, 0);
        queue_summaries(&w, "discord", 2, now);
        let (tx, rx) = running();
        tx.send(true).unwrap();

        w.deliver_due(now, &rx).await.unwrap();
        assert!(server.requests().is_empty());
        assert_eq!(due_at(&w, now).len(), 2);
    }

    #[test]
    fn summary_is_enqueued_once_per_day_for_every_enabled_channel() {
        let tmp = TempDb::new();
        let w = worker(
            &tmp,
            &config_for(Some("http://127.0.0.1:1/hook".into()), true),
        );
        // Default time is 09:00 Europe/Istanbul = 06:00 UTC.
        w.enqueue_summary_if_due(at(2026, 6, 2, 5, 59)).unwrap();
        assert!(due_at(&w, at(2026, 6, 3, 0, 0)).is_empty());

        let due = at(2026, 6, 2, 6, 0);
        w.enqueue_summary_if_due(due).unwrap();
        w.enqueue_summary_if_due(due + Duration::hours(3)).unwrap();
        let rows = w.db().unwrap().due_notifications(due, 0, 10).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.channel.as_str()).collect::<Vec<_>>(),
            ["discord", "telegram"]
        );
        assert_eq!(
            w.db().unwrap().last_summary_date().unwrap(),
            NaiveDate::from_ymd_opt(2026, 6, 2)
        );

        w.enqueue_summary_if_due(due + Duration::days(1)).unwrap();
        assert_eq!(
            w.db()
                .unwrap()
                .due_notifications(due + Duration::days(1), 0, 10)
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn no_summary_when_disabled_or_without_channels() {
        let tmp = TempDb::new();
        let mut config = config_for(Some("http://127.0.0.1:1/hook".into()), false);
        config.alerts.daily_summary = false;
        let w = worker(&tmp, &config);
        w.enqueue_summary_if_due(at(2026, 6, 2, 12, 0)).unwrap();
        assert_eq!(w.db().unwrap().last_summary_date().unwrap(), None);

        let tmp2 = TempDb::new();
        let w = worker(&tmp2, &config_for(None, false));
        w.enqueue_summary_if_due(at(2026, 6, 2, 12, 0)).unwrap();
        assert_eq!(w.db().unwrap().last_summary_date().unwrap(), None);
    }
}
