//! Shared types for the DNS / RDAP / WHOIS checks.

pub mod dns;
pub mod parking;
pub mod rdap;
pub mod whois;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::ratelimit::HostLimiter;
use crate::state::{self, Observation};

const MAX_REDIRECTS: usize = 5;
const BOOTSTRAP_CACHE_FILE: &str = "rdap-dns.json";

#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// Timeouts, 429, 5xx, connection or DNS server failures. Never means "domain free".
    #[error("geçici hata: {0}")]
    Transient(String),
    /// Retrying will not help (unsupported TLD, malformed response, unexpected HTTP status).
    #[error("{0}")]
    Permanent(String),
}

/// Registry data for a registered domain; statuses are in RFC 8056 form (`client hold`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Registration {
    pub statuses: Vec<String>,
    pub registered: Option<DateTime<Utc>>,
    pub expires: Option<DateTime<Utc>>,
    pub last_changed: Option<DateTime<Utc>>,
    pub registrar: Option<String>,
    pub nameservers: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
    Registered(Registration),
    NotFound,
}

/// A registry answer together with where it came from (e.g. the RDAP URL or WHOIS server).
#[derive(Debug)]
pub struct Outcome {
    pub source: String,
    pub lookup: Lookup,
}

/// Turns EPP / WHOIS style statuses into RFC 8056 form:
/// `clientTransferProhibited` -> `client transfer prohibited`.
pub fn normalize_status(raw: &str) -> String {
    let mut out = String::new();
    let mut prev: Option<char> = None;
    for c in raw.trim().chars() {
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            if c.is_uppercase() && prev.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
                out.push(' ');
            }
            out.extend(c.to_lowercase());
        }
        prev = Some(c);
    }
    out
}

/// Lowercases a host name and strips the trailing root dot.
pub fn clean_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_lowercase()
}

pub struct Checker {
    dns: dns::Dns,
    rdap: rdap::Rdap,
    whois: whois::Whois,
    parking: parking::Parking,
}

impl Checker {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        let general = &config.general;
        let timeout = Duration::from_secs(general.http_timeout_secs);
        let http = reqwest::Client::builder()
            .user_agent(&general.user_agent)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .build()
            .context("HTTP istemcisi oluşturulamadı")?;
        let limiter = Arc::new(HostLimiter::new(general.server_requests_per_sec));
        let cache_dir = Path::new(&general.database)
            .parent()
            .unwrap_or_else(|| Path::new("."));
        Ok(Self {
            dns: dns::Dns::new(timeout)?,
            rdap: rdap::Rdap::new(http, limiter.clone(), cache_dir.join(BOOTSTRAP_CACHE_FILE)),
            whois: whois::Whois::new(
                limiter,
                timeout,
                config.detection.whois_not_found_phrases.clone(),
            ),
            parking: parking::Parking::new(general, &config.detection)?,
        })
    }

    pub async fn nameservers(&self, domain: &str) -> Result<Vec<String>, CheckError> {
        self.dns.nameservers(domain).await
    }

    /// Asks RDAP, falling back to WHOIS when the TLD has no RDAP server.
    pub async fn registration(&self, domain: &str) -> Result<Outcome, CheckError> {
        if let Some((source, lookup)) = self.rdap.lookup(domain).await? {
            return Ok(Outcome {
                source: format!("RDAP {source}"),
                lookup,
            });
        }
        let (server, lookup) = self.whois.lookup(domain).await?;
        Ok(Outcome {
            source: format!("WHOIS {server}"),
            lookup,
        })
    }

    /// One full observation: DNS + RDAP/WHOIS, plus parking detection for plain registered domains.
    pub async fn observe(&self, domain: &str, now: DateTime<Utc>) -> Observation {
        let (ns, registration) = tokio::join!(self.nameservers(domain), self.registration(domain));
        let outcome = match registration {
            Ok(outcome) => outcome,
            Err(err) => return Observation::Err(err),
        };
        let dns_ns = match (ns, &outcome.lookup) {
            (Ok(list), _) => list,
            // A failed DNS query must never look like "no NS", or a free verdict could be a false alarm.
            (Err(err), Lookup::NotFound) => return Observation::Err(err),
            (Err(err), Lookup::Registered(_)) => {
                tracing::warn!("{domain}: NS sorgusu başarısız: {err}");
                Vec::new()
            }
        };
        let parking = match &outcome.lookup {
            Lookup::Registered(reg) if state::needs_parking_check(reg, now) => {
                let mut hosts = dns_ns.clone();
                hosts.extend(reg.nameservers.iter().cloned());
                self.parking.detect(domain, &hosts).await
            }
            _ => None,
        };
        Observation::Ok {
            source: outcome.source,
            lookup: outcome.lookup,
            dns_ns,
            parking,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_epp_statuses() {
        for (raw, want) in [
            ("clientTransferProhibited", "client transfer prohibited"),
            ("pendingDelete", "pending delete"),
            ("client delete prohibited", "client delete prohibited"),
            ("OK", "ok"),
            ("Active", "active"),
            ("  redemptionPeriod ", "redemption period"),
        ] {
            assert_eq!(normalize_status(raw), want, "{raw}");
        }
    }

    #[test]
    fn cleans_hosts() {
        assert_eq!(clean_host(" NS1.Example.COM. "), "ns1.example.com");
    }
}
