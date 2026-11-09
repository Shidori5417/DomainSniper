//! TOML configuration loading and validation.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::NaiveTime;
use chrono_tz::Tz;
use serde::Deserialize;

use crate::domain;

const DISCORD_WEBHOOK_ENV: &str = "DOMAIN_GOZCU_DISCORD_WEBHOOK";
const TELEGRAM_TOKEN_ENV: &str = "DOMAIN_GOZCU_TELEGRAM_TOKEN";

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(rename = "genel")]
    pub general: General,
    #[serde(rename = "zamanlama")]
    pub schedule: Schedule,
    #[serde(rename = "uyarilar")]
    pub alerts: Alerts,
    #[serde(rename = "bildirim")]
    pub notify: Notify,
    #[serde(rename = "tespit")]
    pub detection: Detection,
    #[serde(rename = "domain")]
    pub domains: Vec<DomainEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct General {
    #[serde(rename = "veritabani")]
    pub database: String,
    #[serde(rename = "log_seviyesi")]
    pub log_level: String,
    #[serde(rename = "eszamanli_kontrol")]
    pub concurrency: usize,
    /// Per RDAP/WHOIS server request rate limit (requests per second).
    #[serde(rename = "sunucu_istek_per_sn")]
    pub server_requests_per_sec: u32,
    #[serde(rename = "http_timeout_sn")]
    pub http_timeout_secs: u64,
    #[serde(rename = "kullanici_ajani")]
    pub user_agent: String,
}

impl Default for General {
    fn default() -> Self {
        Self {
            database: "/var/lib/domain-gozcu/gozcu.db".into(),
            log_level: "info".into(),
            concurrency: 8,
            server_requests_per_sec: 1,
            http_timeout_secs: 10,
            user_agent: "DomainGozcu/1.0 (+kişisel izleme aracı)".into(),
        }
    }
}

/// Per-state check intervals in minutes.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Schedule {
    #[serde(rename = "aktif")]
    pub active: u32,
    #[serde(rename = "park_satilik")]
    pub parked_for_sale: u32,
    #[serde(rename = "bitis_yakin")]
    pub expiring_soon: u32,
    #[serde(rename = "suresi_doldu")]
    pub expired: u32,
    pub redemption: u32,
    pub pending_delete: u32,
    #[serde(rename = "bosta")]
    pub available: u32,
    #[serde(rename = "askida")]
    pub suspended: u32,
    #[serde(rename = "bilinmiyor")]
    pub unknown: u32,
    #[serde(rename = "jitter_yuzde")]
    pub jitter_percent: u32,
    #[serde(rename = "bosta_dogrulama_dk")]
    pub available_confirm_minutes: u32,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            active: 1440,
            parked_for_sale: 1440,
            expiring_soon: 720,
            expired: 360,
            redemption: 60,
            pending_delete: 10,
            available: 360,
            suspended: 360,
            unknown: 30,
            jitter_percent: 10,
            available_confirm_minutes: 2,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Alerts {
    #[serde(rename = "bitis_esikleri_gun")]
    pub expiry_thresholds_days: Vec<u32>,
    #[serde(rename = "pending_delete_gun")]
    pub pending_delete_days: u32,
    #[serde(rename = "ardisik_hata_uyarisi")]
    pub consecutive_error_alert: u32,
    #[serde(rename = "gunluk_ozet")]
    pub daily_summary: bool,
    /// Daily summary time as `HH:MM` in `timezone`.
    #[serde(rename = "gunluk_ozet_saat")]
    pub daily_summary_time: String,
    #[serde(rename = "saat_dilimi")]
    pub timezone: String,
    /// Per-extension override of `pending_delete_days`.
    #[serde(rename = "uzanti_pending_delete_gun")]
    pub extension_pending_delete_days: HashMap<String, u32>,
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            expiry_thresholds_days: vec![30, 7, 1],
            pending_delete_days: 5,
            consecutive_error_alert: 5,
            daily_summary: true,
            daily_summary_time: "09:00".into(),
            timezone: "Europe/Istanbul".into(),
            extension_pending_delete_days: HashMap::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Notify {
    pub discord: Discord,
    pub telegram: Telegram,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Discord {
    #[serde(rename = "aktif")]
    pub enabled: bool,
    pub webhook_url: Option<String>,
    #[serde(rename = "bosta_mention")]
    pub available_mention: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Telegram {
    #[serde(rename = "aktif")]
    pub enabled: bool,
    pub bot_token: Option<String>,
    pub chat_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Detection {
    pub park_ns: Vec<String>,
    #[serde(rename = "market_alanlari")]
    pub market_domains: Vec<String>,
    #[serde(rename = "satilik_ifadeleri")]
    pub for_sale_phrases: Vec<String>,
    #[serde(rename = "whois_bulunamadi_ifadeleri")]
    pub whois_not_found_phrases: Vec<String>,
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

impl Default for Detection {
    fn default() -> Self {
        Self {
            park_ns: strings(&[
                "sedoparking.com",
                "dan.com",
                "afternic.com",
                "bodis.com",
                "parkingcrew.net",
                "above.com",
                "parklogic.com",
                "domainmarket.com",
            ]),
            market_domains: strings(&[
                "dan.com",
                "sedo.com",
                "afternic.com",
                "atom.com",
                "hugedomains.com",
            ]),
            for_sale_phrases: strings(&[
                "this domain is for sale",
                "domain is for sale",
                "buy this domain",
                "make an offer",
                "bu alan adı satılıktır",
                "bu domain satılık",
                "alan adı satılık",
            ]),
            whois_not_found_phrases: strings(&[
                "no match",
                "not found",
                "no data found",
                "is available",
                "no entries found",
            ]),
        }
    }
}

/// A `[[domain]]` entry from the config file.
#[derive(Debug, Deserialize)]
pub struct DomainEntry {
    #[serde(rename = "ad")]
    pub name: String,
    #[serde(rename = "not")]
    pub note: Option<String>,
    #[serde(rename = "etiket", default)]
    pub tags: Vec<String>,
}

impl Config {
    /// Reads, parses, applies secret env overrides and validates the config file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("yapılandırma dosyası okunamadı: {}", path.display()))?;
        let mut config: Config = toml::from_str(&text)
            .with_context(|| format!("yapılandırma dosyası geçersiz: {}", path.display()))?;
        config.apply_secrets(
            env_secret(DISCORD_WEBHOOK_ENV),
            env_secret(TELEGRAM_TOKEN_ENV),
        );
        config
            .validate()
            .with_context(|| format!("yapılandırma doğrulanamadı: {}", path.display()))?;
        Ok(config)
    }

    fn apply_secrets(&mut self, discord_webhook: Option<String>, telegram_token: Option<String>) {
        if discord_webhook.is_some() {
            self.notify.discord.webhook_url = discord_webhook;
        }
        if telegram_token.is_some() {
            self.notify.telegram.bot_token = telegram_token;
        }
    }

    fn validate(&self) -> Result<()> {
        let g = &self.general;
        if g.concurrency == 0 {
            bail!("[genel].eszamanli_kontrol 0'dan büyük olmalı");
        }
        if g.server_requests_per_sec == 0 {
            bail!("[genel].sunucu_istek_per_sn 0'dan büyük olmalı");
        }
        if g.http_timeout_secs == 0 {
            bail!("[genel].http_timeout_sn 0'dan büyük olmalı");
        }

        let s = &self.schedule;
        let intervals = [
            ("aktif", s.active),
            ("park_satilik", s.parked_for_sale),
            ("bitis_yakin", s.expiring_soon),
            ("suresi_doldu", s.expired),
            ("redemption", s.redemption),
            ("pending_delete", s.pending_delete),
            ("bosta", s.available),
            ("askida", s.suspended),
            ("bilinmiyor", s.unknown),
            ("bosta_dogrulama_dk", s.available_confirm_minutes),
        ];
        for (key, minutes) in intervals {
            if minutes == 0 {
                bail!("[zamanlama].{key} 0'dan büyük olmalı");
            }
        }
        if s.jitter_percent > 100 {
            bail!("[zamanlama].jitter_yuzde 100'den büyük olamaz");
        }

        let a = &self.alerts;
        NaiveTime::parse_from_str(&a.daily_summary_time, "%H:%M").map_err(|_| {
            anyhow::anyhow!(
                "[uyarilar].gunluk_ozet_saat SS:DD biçiminde olmalı, bulunan: {:?}",
                a.daily_summary_time
            )
        })?;
        a.timezone
            .parse::<Tz>()
            .map_err(|_| anyhow::anyhow!("[uyarilar].saat_dilimi geçersiz: {:?}", a.timezone))?;

        for entry in &self.domains {
            domain::normalize(&entry.name)
                .with_context(|| format!("[[domain]] ad geçersiz: {:?}", entry.name))?;
        }
        Ok(())
    }

    /// Checks that every enabled notification channel has its secrets.
    /// Only `run` and `test-notify` need this; list/add/... work without secrets.
    pub fn validate_channels(&self) -> Result<()> {
        let d = &self.notify.discord;
        if d.enabled && is_blank(&d.webhook_url) {
            bail!(
                "[bildirim.discord] aktif ama webhook_url boş; {DISCORD_WEBHOOK_ENV} ortam değişkenini ayarlayın"
            );
        }
        let t = &self.notify.telegram;
        if t.enabled && is_blank(&t.bot_token) {
            bail!(
                "[bildirim.telegram] aktif ama bot_token boş; {TELEGRAM_TOKEN_ENV} ortam değişkenini ayarlayın"
            );
        }
        if t.enabled && is_blank(&t.chat_id) {
            bail!("[bildirim.telegram] aktif ama chat_id boş");
        }
        Ok(())
    }
}

fn env_secret(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn is_blank(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(|v| v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn empty_config_uses_defaults_and_validates() {
        let config = parse("");
        config.validate().unwrap();
        assert_eq!(config.general.concurrency, 8);
        assert_eq!(config.general.server_requests_per_sec, 1);
        assert_eq!(config.schedule.pending_delete, 10);
        assert_eq!(config.alerts.expiry_thresholds_days, [30, 7, 1]);
        assert!(!config.notify.discord.enabled);
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../config.example.toml");
        let mut config = parse(text);
        config.apply_secrets(
            Some("https://example.invalid/hook".into()),
            Some("tok".into()),
        );
        config.validate().unwrap();
        assert_eq!(config.domains.len(), 1);
        assert_eq!(config.domains[0].name, "ornek.com");
        assert_eq!(config.domains[0].tags, ["hedef"]);
        assert_eq!(config.detection.park_ns.len(), 8);
    }

    #[test]
    fn extension_override_map_parses() {
        let config = parse("[uyarilar.uzanti_pending_delete_gun]\norg = 7\n");
        assert_eq!(config.alerts.extension_pending_delete_days["org"], 7);
    }

    #[test]
    fn rejects_bad_values() {
        for text in [
            "[genel]\neszamanli_kontrol = 0",
            "[genel]\nsunucu_istek_per_sn = 0",
            "[zamanlama]\njitter_yuzde = 101",
            "[zamanlama]\naktif = 0",
            "[uyarilar]\ngunluk_ozet_saat = \"25:00\"",
            "[uyarilar]\nsaat_dilimi = \"Mars/Olympus\"",
            "[[domain]]\nad = \"localhost\"",
        ] {
            assert!(parse(text).validate().is_err(), "{text}");
        }
    }

    #[test]
    fn enabled_channels_need_secrets() {
        let discord = "[bildirim.discord]\naktif = true\nwebhook_url = \"\"";
        let mut config = parse(discord);
        let err = config.validate_channels().unwrap_err().to_string();
        assert!(err.contains("webhook_url"), "{err}");
        config.apply_secrets(Some("https://example.invalid/hook".into()), None);
        config.validate_channels().unwrap();

        let telegram = "[bildirim.telegram]\naktif = true\nchat_id = \"1\"";
        let mut config = parse(telegram);
        assert!(config.validate_channels().is_err());
        config.apply_secrets(None, Some("tok".into()));
        config.validate_channels().unwrap();

        let no_chat = parse("[bildirim.telegram]\naktif = true\nbot_token = \"tok\"");
        assert!(no_chat.validate_channels().is_err());
    }

    #[test]
    fn empty_env_secret_does_not_override() {
        let mut config = parse("[bildirim.discord]\nwebhook_url = \"keep\"");
        config.apply_secrets(None, None);
        assert_eq!(config.notify.discord.webhook_url.as_deref(), Some("keep"));
    }
}
