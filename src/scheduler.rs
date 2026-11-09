//! Scans `next_check_at`, dispatches checks under a semaphore, applies jitter.
//!
//! The database is shared as `Arc<std::sync::Mutex<Db>>`: every query is short and synchronous,
//! the lock is never held across an `.await`, and SQLite is a single-writer store anyway.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration as StdDuration;

use anyhow::{anyhow, Result};
use chrono::{Duration, Utc};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::{watch, Semaphore};
use tokio::task::{Id, JoinSet};
use tokio::time::{interval, timeout, MissedTickBehavior};

use crate::check::Checker;
use crate::config::Config;
use crate::db::Db;
use crate::notify::{self, queue::Worker, Notifier};
use crate::state;

const TICK: StdDuration = StdDuration::from_secs(15);
const SHUTDOWN_GRACE: StdDuration = StdDuration::from_secs(30);
const MAX_START_DELAY_MS: u64 = 2_000;
const MIN_NEXT_CHECK_SECS: i64 = 30;
/// Due domains fetched per tick beyond those already in flight, per concurrency slot.
const QUEUE_DEPTH_PER_SLOT: usize = 2;

struct Shared {
    config: Arc<Config>,
    checker: Checker,
    db: Mutex<Db>,
    permits: Semaphore,
    channels: Vec<&'static str>,
}

impl Shared {
    fn db(&self) -> Result<MutexGuard<'_, Db>> {
        self.db
            .lock()
            .map_err(|_| anyhow!("veritabanı kilidi bozuldu"))
    }
}

/// Applies `percent`% jitter to `base`; `r` in [-1, 1] picks the position inside the band.
/// The result never drops below 30 seconds.
pub fn jittered(base: Duration, percent: u32, r: f64) -> Duration {
    let factor = 1.0 + r.clamp(-1.0, 1.0) * f64::from(percent) / 100.0;
    let millis = (base.num_milliseconds() as f64 * factor).round() as i64;
    Duration::milliseconds(millis).max(Duration::seconds(MIN_NEXT_CHECK_SECS))
}

/// Runs the monitoring loop until SIGTERM / SIGINT, then drains in-flight checks.
pub async fn run(config: Config, db_path: &Path) -> Result<()> {
    let db = Db::open(db_path)?;
    db.sync_config_domains(&config.domains)?;
    let domain_count = db.list(None)?.len();
    let channels = notify::enabled_channels(&config);
    let concurrency = config.general.concurrency;
    tracing::info!(
        "başlatıldı: {domain_count} domain, kanallar: {}, eşzamanlı kontrol: {concurrency}",
        if channels.is_empty() {
            "yok".to_string()
        } else {
            channels.join(", ")
        }
    );

    let config = Arc::new(config);
    let worker = Worker::new(&config, Db::open(db_path)?, Notifier::new(&config)?)?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let worker_task = tokio::spawn(worker.run(stop_rx));

    let shared = Arc::new(Shared {
        checker: Checker::new(&config)?,
        config,
        db: Mutex::new(db),
        permits: Semaphore::new(concurrency),
        channels,
    });
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    let mut in_flight: HashMap<Id, String> = HashMap::new();
    let mut ticker = interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticker.tick() => dispatch(&shared, &mut tasks, &mut in_flight),
            Some(done) = tasks.join_next_with_id(), if !tasks.is_empty() => {
                reap(done, &mut in_flight);
            }
            _ = sigterm.recv() => { tracing::info!("SIGTERM alındı, kapatılıyor"); break; }
            _ = sigint.recv() => { tracing::info!("SIGINT alındı, kapatılıyor"); break; }
        }
    }

    // The notification worker finishes the send it is in the middle of, then stops.
    let _ = stop_tx.send(true);
    // Checks still waiting for a permit give up; the ones already running finish and write.
    shared.permits.close();
    let pending = tasks.len();
    tracing::info!(
        "{pending} devam eden kontrol bekleniyor (en çok {} sn)",
        SHUTDOWN_GRACE.as_secs()
    );
    let drained = timeout(SHUTDOWN_GRACE, async {
        while let Some(done) = tasks.join_next_with_id().await {
            reap(done, &mut in_flight);
        }
    })
    .await;
    if drained.is_err() {
        tracing::warn!("{} kontrol zamanında bitmedi, iptal ediliyor", tasks.len());
        tasks.abort_all();
    }
    if timeout(SHUTDOWN_GRACE, worker_task).await.is_err() {
        tracing::warn!("bildirim işçisi zamanında durmadı");
    }
    tracing::info!("kapandı");
    Ok(())
}

fn reap(
    done: Result<(Id, Result<()>), tokio::task::JoinError>,
    in_flight: &mut HashMap<Id, String>,
) {
    let (id, result) = match done {
        Ok((id, result)) => (id, result),
        Err(err) => (err.id(), Err(anyhow!("görev çöktü: {err}"))),
    };
    let name = in_flight.remove(&id).unwrap_or_default();
    if let Err(err) = result {
        tracing::warn!("{name}: kontrol başarısız: {err:#}");
    }
}

fn dispatch(
    shared: &Arc<Shared>,
    tasks: &mut JoinSet<Result<()>>,
    in_flight: &mut HashMap<Id, String>,
) {
    let limit = in_flight.len() + shared.config.general.concurrency * QUEUE_DEPTH_PER_SLOT;
    let due = match shared.db().and_then(|db| db.due_domains(Utc::now(), limit)) {
        Ok(due) => due,
        Err(err) => {
            tracing::warn!("vadesi gelen domainler okunamadı: {err:#}");
            return;
        }
    };
    let busy: HashSet<&str> = in_flight.values().map(String::as_str).collect();
    let fresh: Vec<String> = due
        .into_iter()
        .filter(|name| !busy.contains(name.as_str()))
        .collect();
    for name in fresh {
        let shared = Arc::clone(shared);
        let task_name = name.clone();
        let handle = tasks.spawn(async move { check_one(&shared, &task_name).await });
        in_flight.insert(handle.id(), name);
    }
}

async fn check_one(shared: &Shared, name: &str) -> Result<()> {
    let Ok(_permit) = shared.permits.acquire().await else {
        return Ok(());
    };
    let delay = rand::random_range(0..=MAX_START_DELAY_MS);
    tokio::time::sleep(StdDuration::from_millis(delay)).await;

    let Some(previous) = shared.db()?.previous(name)? else {
        return Ok(());
    };
    let now = Utc::now();
    let observation = shared.checker.observe(name, now).await;
    let decision = state::decide(name, &previous, &observation, now, &shared.config);
    let next_check_at = now
        + jittered(
            decision.next_check_in,
            shared.config.schedule.jitter_percent,
            rand::random_range(-1.0..=1.0),
        );
    let applied =
        shared
            .db()?
            .apply_decision(name, &decision, now, next_check_at, &shared.channels)?;
    if !applied {
        tracing::info!("{name}: kontrol sırasında listeden silindi");
    } else if decision.state != previous.state {
        tracing::info!("{name}: {} -> {}", previous.state, decision.state);
    } else {
        tracing::debug!("{name}: durum değişmedi ({})", decision.state);
    }
    for event in &decision.events {
        tracing::info!("{name}: olay kuyruğa alındı: {event:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_band() {
        let base = Duration::minutes(100);
        assert_eq!(jittered(base, 10, 1.0), Duration::minutes(110));
        assert_eq!(jittered(base, 10, -1.0), Duration::minutes(90));
        assert_eq!(jittered(base, 10, 0.0), base);
        assert_eq!(jittered(base, 10, 0.5), Duration::minutes(105));
    }

    #[test]
    fn out_of_range_random_is_clamped() {
        let base = Duration::minutes(100);
        assert_eq!(jittered(base, 10, 5.0), Duration::minutes(110));
        assert_eq!(jittered(base, 10, -5.0), Duration::minutes(90));
    }

    #[test]
    fn zero_percent_is_exact() {
        let base = Duration::hours(6);
        for r in [-1.0, -0.3, 0.0, 0.9, 1.0] {
            assert_eq!(jittered(base, 0, r), base);
        }
    }

    #[test]
    fn never_below_minimum() {
        assert_eq!(
            jittered(Duration::seconds(10), 10, 0.0),
            Duration::seconds(30)
        );
        assert_eq!(
            jittered(Duration::seconds(31), 50, -1.0),
            Duration::seconds(30)
        );
        assert_eq!(
            jittered(Duration::minutes(1), 10, -1.0),
            Duration::seconds(54)
        );
    }
}
