//! Notification payloads, channel-neutral message formatting and the sender facade.
//!
//! A queued payload is turned into a [`Message`] by [`format_payload`]; each channel module then
//! renders that message in its own wire format. The persistent queue and its worker live in
//! [`queue`].

pub mod discord;
#[cfg(test)]
mod mock;
pub mod queue;
pub mod telegram;

use std::fmt;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::state::{Event, State};

pub const DISCORD: &str = "discord";
pub const TELEGRAM: &str = "telegram";

/// Used when a 429 response carries no usable `retry_after`.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(5);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);
/// Longest list shown in a summary section; the rest is collapsed into "... N more".
const SUMMARY_LIST_LIMIT: usize = 25;
const ERROR_BODY_CHARS: usize = 200;
const DAY_SECS: i64 = 86_400;

/// Names of the channels enabled in the config, in delivery order.
pub fn enabled_channels(config: &Config) -> Vec<&'static str> {
    let mut channels = Vec::new();
    if config.notify.discord.enabled {
        channels.push(DISCORD);
    }
    if config.notify.telegram.enabled {
        channels.push(TELEGRAM);
    }
    channels
}

/// The `notify_queue.payload` format: a JSON object tagged with `kind`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Payload {
    Event(QueuedEvent),
    Summary(Summary),
}

/// A state machine event plus the registry facts the message text shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedEvent {
    /// Human-readable domain name (not punycode).
    pub domain: String,
    pub event: Event,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registrar: Option<String>,
}

/// Snapshot for the daily summary message, taken when it is enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// The local (config time zone) day this summary belongs to.
    pub date: NaiveDate,
    pub total: usize,
    /// Domains per state, states with no domain left out.
    pub counts: Vec<(State, usize)>,
    /// Domains whose state is not `Aktif`.
    pub attention: Vec<(String, State)>,
    /// Domains expiring within `expiry_window_days`, soonest first.
    pub expiring: Vec<(String, DateTime<Utc>)>,
    pub expiry_window_days: u32,
}

/// Importance class of a message; channels map it to a color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warning,
    Orange,
    Critical,
    Success,
    Purple,
    Muted,
}

/// A channel-neutral notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// First line, starts with an emoji.
    pub title: String,
    pub body: String,
    pub level: Level,
    /// Domain became free: Discord adds the configured mention.
    pub high_priority: bool,
}

fn state_emoji(state: State) -> &'static str {
    match state {
        State::Aktif => "🔵",
        State::ParkSatilik => "🟣",
        State::BitisYakin => "🟡",
        State::SuresiDoldu | State::Redemption => "🟠",
        State::PendingDelete => "🔴",
        State::Bosta => "🟢",
        State::Askida => "⚫",
        State::Bilinmiyor => "⚪",
    }
}

fn state_level(state: State) -> Level {
    match state {
        State::Aktif => Level::Info,
        State::ParkSatilik => Level::Purple,
        State::BitisYakin => Level::Warning,
        State::SuresiDoldu | State::Redemption => Level::Orange,
        State::PendingDelete => Level::Critical,
        State::Bosta => Level::Success,
        State::Askida | State::Bilinmiyor => Level::Muted,
    }
}

fn local_date(time: DateTime<Utc>, tz: Tz) -> String {
    time.with_timezone(&tz).format("%Y-%m-%d").to_string()
}

/// Whole days from `now` to `time`, rounded up and never negative.
fn days_until(time: DateTime<Utc>, now: DateTime<Utc>) -> i64 {
    ((time - now).num_seconds() + DAY_SECS - 1)
        .div_euclid(DAY_SECS)
        .max(0)
}

/// Builds the channel-neutral message for a queued payload. Dates are shown in `tz`; relative
/// day counts are measured from `now`.
pub fn format_payload(payload: &Payload, tz: Tz, now: DateTime<Utc>) -> Message {
    match payload {
        Payload::Event(queued) => format_event(queued, tz, now),
        Payload::Summary(summary) => format_summary(summary, tz, now),
    }
}

fn format_event(queued: &QueuedEvent, tz: Tz, now: DateTime<Utc>) -> Message {
    let domain = queued.domain.as_str();
    let message = |emoji: &str, headline: String, body: String, level: Level| Message {
        title: format!("{emoji} {domain} {headline}"),
        body,
        level,
        high_priority: false,
    };
    match &queued.event {
        Event::StateChanged { from, to, detail } => match to {
            State::Redemption => {
                let mut facts = Vec::new();
                if let Some(expires) = queued.expires {
                    facts.push(format!("Bitiş: {}", local_date(expires, tz)));
                }
                if let Some(registrar) = &queued.registrar {
                    facts.push(format!("Registrar: {registrar}"));
                }
                let body = if facts.is_empty() {
                    detail.clone()
                } else {
                    facts.join(". ") + "."
                };
                message(
                    "🟠",
                    "redemption dönemine girdi.".into(),
                    body,
                    Level::Orange,
                )
            }
            State::ParkSatilik => message(
                "🟣",
                "satılık görünüyor.".into(),
                detail.clone(),
                Level::Purple,
            ),
            _ => message(
                state_emoji(*to),
                format!("durumu değişti: {from} → {to}."),
                detail.clone(),
                state_level(*to),
            ),
        },
        Event::Retaken { registrar } => {
            let body = match registrar {
                Some(registrar) => format!("Yeni registrar: {registrar}"),
                None => "Yeni registrar bilinmiyor.".into(),
            };
            message("⚪", "tekrar alındı.".into(), body, Level::Muted)
        }
        Event::PendingDelete { estimated_drop } => {
            let days =
                (((*estimated_drop - now).num_seconds() as f64) / DAY_SECS as f64).round() as i64;
            message(
                "🔴",
                "pendingDelete!".into(),
                format!(
                    "Tahmini düşme: {} (~{} gün)",
                    local_date(*estimated_drop, tz),
                    days.max(0)
                ),
                Level::Critical,
            )
        }
        Event::BecameFree => Message {
            high_priority: true,
            ..message(
                "🟢",
                "BOŞTA görünüyor.".into(),
                "Kaydedilebilir (2 kez doğrulandı)".into(),
                Level::Success,
            )
        },
        Event::ExpiryWarning { days_left } => {
            let body = queued
                .expires
                .map(|expires| format!("Bitiş: {}", local_date(expires, tz)))
                .unwrap_or_default();
            message(
                "⏳",
                format!("bitişine {days_left} gün kaldı."),
                body,
                Level::Warning,
            )
        }
        Event::Renewed { new_expires } => message(
            "🔄",
            "yenilendi.".into(),
            format!("Yeni bitiş: {}", local_date(*new_expires, tz)),
            Level::Info,
        ),
        Event::Unreachable {
            error_count,
            reason,
        } => message(
            "⚠️",
            "kontrol edilemiyor.".into(),
            format!("{error_count} ardışık hata. Son neden: {reason}"),
            Level::Muted,
        ),
    }
}

fn format_summary(summary: &Summary, tz: Tz, now: DateTime<Utc>) -> Message {
    let mut lines = vec![format!("Toplam: {} domain", summary.total)];
    for (state, count) in &summary.counts {
        lines.push(format!("{} {state}: {count}", state_emoji(*state)));
    }
    if !summary.attention.is_empty() {
        lines.push(String::new());
        lines.push("Dikkat gerektirenler:".into());
        let shown = summary.attention.iter().take(SUMMARY_LIST_LIMIT);
        lines.extend(shown.map(|(domain, state)| format!("• {domain} — {state}")));
        push_remainder(&mut lines, summary.attention.len());
    }
    if !summary.expiring.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Önümüzdeki {} gün içinde bitenler:",
            summary.expiry_window_days
        ));
        let shown = summary.expiring.iter().take(SUMMARY_LIST_LIMIT);
        lines.extend(shown.map(|(domain, expires)| {
            format!(
                "• {domain} — {} ({} gün)",
                local_date(*expires, tz),
                days_until(*expires, now)
            )
        }));
        push_remainder(&mut lines, summary.expiring.len());
    }
    Message {
        title: format!("📋 Günlük özet — {}", summary.date.format("%Y-%m-%d")),
        body: lines.join("\n"),
        level: Level::Info,
        high_priority: false,
    }
}

fn push_remainder(lines: &mut Vec<String>, total: usize) {
    if total > SUMMARY_LIST_LIMIT {
        lines.push(format!("… ve {} tane daha", total - SUMMARY_LIST_LIMIT));
    }
}

/// Why a send did not deliver. Never contains webhook URLs or tokens.
#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    /// HTTP 429: the channel asks to wait.
    RateLimited { retry_after: Duration },
    /// Network error or any non-2xx, non-429 response (`status` is `None` without a response).
    Failed { status: Option<u16>, reason: String },
}

impl SendError {
    /// Whether the whole channel is unavailable (as opposed to this one message being refused).
    pub fn is_channel_wide(&self) -> bool {
        match self {
            SendError::RateLimited { .. } => true,
            SendError::Failed { status, .. } => status.is_none_or(|code| code >= 500),
        }
    }
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::RateLimited { retry_after } => {
                write!(
                    f,
                    "HTTP 429, {:.1} sn sonra tekrar denenecek",
                    retry_after.as_secs_f64()
                )
            }
            SendError::Failed {
                status: Some(code),
                reason,
            } => write!(f, "HTTP {code}: {reason}"),
            SendError::Failed {
                status: None,
                reason,
            } => f.write_str(reason),
        }
    }
}

impl std::error::Error for SendError {}

/// Maps a transport error to a [`SendError`] without leaking the request URL.
fn transport_error(err: reqwest::Error) -> SendError {
    let reason = if err.is_timeout() {
        "zaman aşımı".to_string()
    } else if err.is_connect() {
        "bağlantı kurulamadı".to_string()
    } else {
        err.without_url().to_string()
    };
    SendError::Failed {
        status: None,
        reason,
    }
}

/// Everything the channels need from an HTTP response.
struct Reply {
    status: u16,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    async fn read(response: reqwest::Response) -> Reply {
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        Reply {
            status,
            headers,
            body,
        }
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }

    /// Classifies the reply; `body_retry_secs` extracts the channel's own `retry_after` value.
    fn into_result(
        self,
        body_retry_secs: impl FnOnce(&Value) -> Option<f64>,
    ) -> Result<u16, SendError> {
        match self.status {
            200..=299 => Ok(self.status),
            429 => {
                let from_header = self
                    .headers
                    .get(RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<f64>().ok());
                let secs = body_retry_secs(&self.json()).or(from_header);
                Err(SendError::RateLimited {
                    retry_after: clamp_retry_after(secs),
                })
            }
            status => {
                let snippet: String = self.body.trim().chars().take(ERROR_BODY_CHARS).collect();
                let reason = if snippet.is_empty() {
                    "yanıt gövdesi boş".into()
                } else {
                    snippet
                };
                Err(SendError::Failed {
                    status: Some(status),
                    reason,
                })
            }
        }
    }
}

fn clamp_retry_after(secs: Option<f64>) -> Duration {
    match secs.filter(|s| s.is_finite() && *s >= 0.0) {
        Some(secs) => Duration::try_from_secs_f64(secs)
            .unwrap_or(MAX_RETRY_AFTER)
            .clamp(Duration::from_secs(1), MAX_RETRY_AFTER),
        None => DEFAULT_RETRY_AFTER,
    }
}

/// The configured channels, ready to send.
pub struct Notifier {
    discord: Option<discord::Discord>,
    telegram: Option<telegram::Telegram>,
    pub tz: Tz,
}

impl Notifier {
    pub fn new(config: &Config) -> Result<Notifier> {
        let http = reqwest::Client::builder()
            .user_agent(&config.general.user_agent)
            .timeout(Duration::from_secs(config.general.http_timeout_secs))
            .build()
            .context("bildirim HTTP istemcisi oluşturulamadı")?;
        let tz = config.alerts.timezone.parse::<Tz>().map_err(|_| {
            anyhow!(
                "[uyarilar].saat_dilimi geçersiz: {:?}",
                config.alerts.timezone
            )
        })?;

        let cfg = &config.notify;
        let discord = cfg
            .discord
            .enabled
            .then(|| {
                let url = cfg
                    .discord
                    .webhook_url
                    .clone()
                    .filter(|u| !u.trim().is_empty());
                let url = url.ok_or_else(|| anyhow!("discord etkin ama webhook_url boş"))?;
                Ok::<_, anyhow::Error>(discord::Discord::new(
                    http.clone(),
                    url,
                    cfg.discord.available_mention.clone(),
                ))
            })
            .transpose()?;
        let telegram = cfg
            .telegram
            .enabled
            .then(|| {
                let token = cfg
                    .telegram
                    .bot_token
                    .clone()
                    .filter(|t| !t.trim().is_empty());
                let chat = cfg
                    .telegram
                    .chat_id
                    .clone()
                    .filter(|c| !c.trim().is_empty());
                match (token, chat) {
                    (Some(token), Some(chat)) => {
                        Ok(telegram::Telegram::new(http.clone(), token, chat))
                    }
                    _ => Err(anyhow!("telegram etkin ama bot_token veya chat_id boş")),
                }
            })
            .transpose()?;
        Ok(Notifier {
            discord,
            telegram,
            tz,
        })
    }

    pub fn is_enabled(&self, channel: &str) -> bool {
        match channel {
            DISCORD => self.discord.is_some(),
            TELEGRAM => self.telegram.is_some(),
            _ => false,
        }
    }

    /// Sends one message on one channel; returns the HTTP status on success.
    pub async fn send(&self, channel: &str, message: &Message) -> Result<u16, SendError> {
        let disabled = || SendError::Failed {
            status: None,
            reason: format!("{channel} kanalı etkin değil"),
        };
        match channel {
            DISCORD => {
                self.discord
                    .as_ref()
                    .ok_or_else(disabled)?
                    .send(message)
                    .await
            }
            TELEGRAM => {
                self.telegram
                    .as_ref()
                    .ok_or_else(disabled)?
                    .send(message)
                    .await
            }
            _ => Err(disabled()),
        }
    }
}

/// The message `test-notify` sends.
pub fn test_message() -> Message {
    Message {
        title: "🧪 DomainGözcü test mesajı".into(),
        body: "Bildirim kanalı çalışıyor.".into(),
        level: Level::Info,
        high_priority: false,
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn istanbul() -> Tz {
        "Europe/Istanbul".parse().unwrap()
    }

    fn queued(event: Event) -> Payload {
        Payload::Event(QueuedEvent {
            domain: "ornek.com".into(),
            event,
            expires: Some(at(2026, 9, 30, 22)),
            registrar: Some("Reg <A&B>".into()),
        })
    }

    fn fmt(event: Event) -> Message {
        format_payload(&queued(event), istanbul(), at(2026, 11, 9, 12))
    }

    #[test]
    fn redemption_shows_expiry_in_local_tz_and_registrar() {
        let m = fmt(Event::StateChanged {
            from: State::SuresiDoldu,
            to: State::Redemption,
            detail: "RDAP durumu: redemption period".into(),
        });
        assert_eq!(m.title, "🟠 ornek.com redemption dönemine girdi.");
        // 22:00 UTC on Sep 30 is already Oct 1 in Istanbul (UTC+3).
        assert_eq!(m.body, "Bitiş: 2026-10-01. Registrar: Reg <A&B>.");
        assert_eq!(m.level, Level::Orange);
        assert!(!m.high_priority);
    }

    #[test]
    fn redemption_without_facts_falls_back_to_detail() {
        let payload = Payload::Event(QueuedEvent {
            domain: "ornek.com".into(),
            event: Event::StateChanged {
                from: State::Aktif,
                to: State::Redemption,
                detail: "RDAP durumu: redemption period".into(),
            },
            expires: None,
            registrar: None,
        });
        let m = format_payload(&payload, istanbul(), at(2026, 11, 9, 12));
        assert_eq!(m.body, "RDAP durumu: redemption period");
    }

    #[test]
    fn pending_delete_shows_drop_date_and_days() {
        let m = fmt(Event::PendingDelete {
            estimated_drop: at(2026, 11, 14, 12),
        });
        assert_eq!(m.title, "🔴 ornek.com pendingDelete!");
        assert_eq!(m.body, "Tahmini düşme: 2026-11-14 (~5 gün)");
        assert_eq!(m.level, Level::Critical);
    }

    #[test]
    fn became_free_is_high_priority() {
        let m = fmt(Event::BecameFree);
        assert_eq!(m.title, "🟢 ornek.com BOŞTA görünüyor.");
        assert_eq!(m.body, "Kaydedilebilir (2 kez doğrulandı)");
        assert_eq!(m.level, Level::Success);
        assert!(m.high_priority);
    }

    #[test]
    fn for_sale_shows_evidence() {
        let m = fmt(Event::StateChanged {
            from: State::Aktif,
            to: State::ParkSatilik,
            detail: "NS: ns1.sedoparking.com".into(),
        });
        assert_eq!(m.title, "🟣 ornek.com satılık görünüyor.");
        assert_eq!(m.body, "NS: ns1.sedoparking.com");
        assert_eq!(m.level, Level::Purple);
    }

    #[test]
    fn retaken_shows_registrar() {
        let m = fmt(Event::Retaken {
            registrar: Some("Y".into()),
        });
        assert_eq!(m.title, "⚪ ornek.com tekrar alındı.");
        assert_eq!(m.body, "Yeni registrar: Y");
        let unknown = fmt(Event::Retaken { registrar: None });
        assert_eq!(unknown.body, "Yeni registrar bilinmiyor.");
    }

    #[test]
    fn expiry_warning_and_renewal() {
        let m = fmt(Event::ExpiryWarning { days_left: 7 });
        assert_eq!(m.title, "⏳ ornek.com bitişine 7 gün kaldı.");
        assert_eq!(m.body, "Bitiş: 2026-10-01");
        assert_eq!(m.level, Level::Warning);

        let m = fmt(Event::Renewed {
            new_expires: at(2028, 1, 1, 0),
        });
        assert_eq!(m.title, "🔄 ornek.com yenilendi.");
        assert_eq!(m.body, "Yeni bitiş: 2028-01-01");
    }

    #[test]
    fn unreachable_shows_count_and_reason() {
        let m = fmt(Event::Unreachable {
            error_count: 5,
            reason: "geçici hata: zaman aşımı".into(),
        });
        assert_eq!(m.title, "⚠️ ornek.com kontrol edilemiyor.");
        assert_eq!(
            m.body,
            "5 ardışık hata. Son neden: geçici hata: zaman aşımı"
        );
    }

    #[test]
    fn generic_state_change_uses_target_state() {
        let m = fmt(Event::StateChanged {
            from: State::Aktif,
            to: State::Askida,
            detail: "RDAP durumu: client hold".into(),
        });
        assert_eq!(m.title, "⚫ ornek.com durumu değişti: Aktif → Askida.");
        assert_eq!(m.body, "RDAP durumu: client hold");
        assert_eq!(m.level, Level::Muted);
    }

    #[test]
    fn summary_lists_counts_attention_and_expiring() {
        let summary = Summary {
            date: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(),
            total: 3,
            counts: vec![
                (State::Aktif, 1),
                (State::Redemption, 1),
                (State::BitisYakin, 1),
            ],
            attention: vec![
                ("a.com".into(), State::Redemption),
                ("b.com".into(), State::BitisYakin),
            ],
            expiring: vec![("b.com".into(), at(2026, 11, 12, 10))],
            expiry_window_days: 30,
        };
        let m = format_payload(&Payload::Summary(summary), istanbul(), at(2026, 11, 9, 12));
        assert_eq!(m.title, "📋 Günlük özet — 2026-11-09");
        let expected = "Toplam: 3 domain\n🔵 Aktif: 1\n🟠 Redemption: 1\n🟡 BitisYakin: 1\n\n\
                        Dikkat gerektirenler:\n• a.com — Redemption\n• b.com — BitisYakin\n\n\
                        Önümüzdeki 30 gün içinde bitenler:\n• b.com — 2026-11-12 (3 gün)";
        assert_eq!(m.body, expected);
        assert_eq!(m.level, Level::Info);
    }

    #[test]
    fn summary_collapses_long_lists() {
        let summary = Summary {
            date: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(),
            total: 30,
            counts: vec![(State::Bosta, 30)],
            attention: (0..30)
                .map(|i| (format!("d{i}.com"), State::Bosta))
                .collect(),
            expiring: Vec::new(),
            expiry_window_days: 30,
        };
        let m = format_payload(&Payload::Summary(summary), istanbul(), at(2026, 11, 9, 12));
        assert!(m.body.contains("• d24.com") && !m.body.contains("• d25.com"));
        assert!(m.body.ends_with("… ve 5 tane daha"));
    }

    #[test]
    fn payload_round_trips_with_kind_tag() {
        let event = queued(Event::BecameFree);
        let json = serde_json::to_string(&event).unwrap();
        assert!(
            json.starts_with(r#"{"kind":"event","domain":"ornek.com""#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), event);

        let bare = Payload::Event(QueuedEvent {
            domain: "x.com".into(),
            event: Event::BecameFree,
            expires: None,
            registrar: None,
        });
        let json = serde_json::to_string(&bare).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"event","domain":"x.com","event":"BecameFree"}"#
        );
        assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), bare);

        let summary = Payload::Summary(Summary {
            date: NaiveDate::from_ymd_opt(2026, 11, 9).unwrap(),
            total: 1,
            counts: vec![(State::Aktif, 1)],
            attention: vec![("a.com".into(), State::Askida)],
            expiring: vec![("a.com".into(), at(2026, 12, 1, 0))],
            expiry_window_days: 30,
        });
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.starts_with(r#"{"kind":"summary""#), "{json}");
        assert_eq!(serde_json::from_str::<Payload>(&json).unwrap(), summary);
    }

    #[test]
    fn retry_after_is_clamped_with_default() {
        assert_eq!(clamp_retry_after(Some(2.0)), Duration::from_secs(2));
        assert_eq!(clamp_retry_after(Some(0.2)), Duration::from_secs(1));
        assert_eq!(clamp_retry_after(Some(99_999.0)), MAX_RETRY_AFTER);
        assert_eq!(clamp_retry_after(Some(f64::NAN)), DEFAULT_RETRY_AFTER);
        assert_eq!(clamp_retry_after(None), DEFAULT_RETRY_AFTER);
    }

    #[test]
    fn channel_wide_errors() {
        let failed = |status| SendError::Failed {
            status,
            reason: String::new(),
        };
        assert!(SendError::RateLimited {
            retry_after: DEFAULT_RETRY_AFTER
        }
        .is_channel_wide());
        assert!(failed(None).is_channel_wide());
        assert!(failed(Some(503)).is_channel_wide());
        assert!(!failed(Some(400)).is_channel_wide());
    }
}
