//! RDAP client: IANA bootstrap (disk-cached) plus domain lookups and response parsing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use reqwest::header::{ACCEPT, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::{clean_host, normalize_status, CheckError, Lookup, Registration};
use crate::ratelimit::{retry, Failure, HostLimiter};

const BOOTSTRAP_URL: &str = "https://data.iana.org/rdap/dns.json";
const BOOTSTRAP_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const RDAP_ACCEPT: &str = "application/rdap+json";

/// TLD (possibly multi-label) -> RDAP base URL, always ending in `/`.
#[derive(Debug, Default)]
struct Bootstrap {
    servers: HashMap<String, String>,
}

#[derive(Deserialize)]
struct BootstrapFile {
    services: Vec<(Vec<String>, Vec<String>)>,
}

impl Bootstrap {
    fn parse(json: &str) -> Result<Self, CheckError> {
        let file: BootstrapFile = serde_json::from_str(json)
            .map_err(|e| CheckError::Permanent(format!("RDAP bootstrap geçersiz: {e}")))?;
        let mut servers = HashMap::new();
        for (tlds, urls) in file.services {
            let Some(url) = urls.first() else { continue };
            let base = if url.ends_with('/') {
                url.clone()
            } else {
                format!("{url}/")
            };
            for tld in tlds {
                servers.insert(tld.to_lowercase(), base.clone());
            }
        }
        Ok(Self { servers })
    }

    /// Longest-suffix match over the domain's labels (the domain itself is never a TLD).
    fn base_url(&self, domain: &str) -> Option<&str> {
        let labels: Vec<&str> = domain.trim_end_matches('.').split('.').collect();
        (1..labels.len())
            .find_map(|start| self.servers.get(&labels[start..].join(".")))
            .map(String::as_str)
    }
}

struct Loaded {
    bootstrap: Bootstrap,
    fetched: SystemTime,
}

impl Loaded {
    fn is_fresh(&self) -> bool {
        SystemTime::now()
            .duration_since(self.fetched)
            .is_ok_and(|age| age < BOOTSTRAP_MAX_AGE)
    }
}

pub struct Rdap {
    http: reqwest::Client,
    limiter: Arc<HostLimiter>,
    cache_path: PathBuf,
    loaded: Mutex<Option<Loaded>>,
}

impl Rdap {
    pub fn new(http: reqwest::Client, limiter: Arc<HostLimiter>, cache_path: PathBuf) -> Self {
        Self {
            http,
            limiter,
            cache_path,
            loaded: Mutex::new(None),
        }
    }

    /// `Ok(None)` when the TLD has no RDAP server (caller falls back to WHOIS).
    /// On success returns the queried URL together with the result.
    pub async fn lookup(&self, domain: &str) -> Result<Option<(String, Lookup)>, CheckError> {
        let Some(base) = self.base_url(domain).await? else {
            return Ok(None);
        };
        let url = format!("{base}domain/{domain}");
        let lookup = retry(|| self.query(&url)).await?;
        Ok(Some((url, lookup)))
    }

    async fn query(&self, url: &str) -> Result<Lookup, Failure> {
        self.limiter.until_ready(&host_of(url)).await;
        let response = self
            .http
            .get(url)
            .header(ACCEPT, RDAP_ACCEPT)
            .send()
            .await
            .map_err(transient)?;
        let status = response.status();
        match status {
            StatusCode::OK => {
                let body = response.text().await.map_err(transient)?;
                Ok(Lookup::Registered(parse_rdap(&body)?))
            }
            StatusCode::NOT_FOUND => Ok(Lookup::NotFound),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => Err(Failure {
                error: CheckError::Transient(format!("RDAP HTTP {s}")),
                retry_after: retry_after(&response),
            }),
            s => Err(CheckError::Permanent(format!("RDAP beklenmeyen HTTP durumu: {s}")).into()),
        }
    }

    async fn base_url(&self, domain: &str) -> Result<Option<String>, CheckError> {
        let mut guard = self.loaded.lock().await;
        if !guard.as_ref().is_some_and(Loaded::is_fresh) {
            if let Some(loaded) = self.refresh(guard.take()).await? {
                *guard = Some(loaded);
            }
        }
        Ok(guard
            .as_ref()
            .and_then(|l| l.bootstrap.base_url(domain))
            .map(str::to_owned))
    }

    /// Returns the freshest bootstrap available: disk cache, a download, or stale data.
    async fn refresh(&self, stale_memory: Option<Loaded>) -> Result<Option<Loaded>, CheckError> {
        if let Some(cached) = self.read_cache().filter(Loaded::is_fresh) {
            return Ok(Some(cached));
        }
        match self.download().await {
            Ok(loaded) => Ok(Some(loaded)),
            Err(err) => match stale_memory.or_else(|| self.read_cache()) {
                Some(stale) => {
                    tracing::warn!(
                        "RDAP bootstrap yenilenemedi ({err}); eski önbellek kullanılıyor"
                    );
                    Ok(Some(stale))
                }
                None => Err(err),
            },
        }
    }

    fn read_cache(&self) -> Option<Loaded> {
        let fetched = std::fs::metadata(&self.cache_path).ok()?.modified().ok()?;
        let text = std::fs::read_to_string(&self.cache_path).ok()?;
        let bootstrap = Bootstrap::parse(&text).ok()?;
        Some(Loaded { bootstrap, fetched })
    }

    async fn download(&self) -> Result<Loaded, CheckError> {
        let body = retry(|| async {
            self.limiter.until_ready(&host_of(BOOTSTRAP_URL)).await;
            let response = self
                .http
                .get(BOOTSTRAP_URL)
                .send()
                .await
                .map_err(transient)?;
            let status = response.status();
            if !status.is_success() {
                let error = CheckError::Transient(format!("bootstrap HTTP {status}"));
                return Err(Failure {
                    error,
                    retry_after: retry_after(&response),
                });
            }
            response.text().await.map_err(transient)
        })
        .await?;
        let bootstrap = Bootstrap::parse(&body)?;
        self.write_cache(&body);
        Ok(Loaded {
            bootstrap,
            fetched: SystemTime::now(),
        })
    }

    /// Best effort: a cache that cannot be written only costs a re-download later.
    fn write_cache(&self, body: &str) {
        let tmp = self.cache_path.with_extension("json.tmp");
        let result =
            std::fs::write(&tmp, body).and_then(|()| std::fs::rename(&tmp, &self.cache_path));
        if let Err(err) = result {
            tracing::warn!(
                "RDAP bootstrap önbelleği yazılamadı ({}): {err}",
                self.cache_path.display()
            );
        }
    }
}

fn host_of(url: &str) -> String {
    url.split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn transient(err: reqwest::Error) -> Failure {
    CheckError::Transient(format!("HTTP: {err}")).into()
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let secs = response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs))
}

fn event_date(events: &[Value], action: &str) -> Option<DateTime<Utc>> {
    events
        .iter()
        .find(|e| {
            e["eventAction"]
                .as_str()
                .is_some_and(|a| a.eq_ignore_ascii_case(action))
        })
        .and_then(|e| e["eventDate"].as_str())
        .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
        .map(|d| d.with_timezone(&Utc))
}

/// `fn` property of the entity's jCard, if non-empty.
fn vcard_name(entity: &Value) -> Option<&str> {
    entity["vcardArray"][1]
        .as_array()?
        .iter()
        .find(|p| p[0].as_str() == Some("fn"))
        .and_then(|p| p[3].as_str())
        .map(str::trim)
        .filter(|n| !n.is_empty())
}

fn registrar(entities: &[Value]) -> Option<String> {
    let entity = entities.iter().find(|e| {
        e["roles"]
            .as_array()
            .is_some_and(|r| r.iter().any(|r| r.as_str() == Some("registrar")))
    })?;
    vcard_name(entity)
        .or_else(|| entity["handle"].as_str().filter(|h| !h.is_empty()))
        .map(str::to_owned)
}

/// Parses an RDAP domain object.
pub fn parse_rdap(json: &str) -> Result<Registration, CheckError> {
    let doc: Value = serde_json::from_str(json)
        .map_err(|e| CheckError::Permanent(format!("RDAP yanıtı geçersiz JSON: {e}")))?;
    let list = |key: &str| {
        doc[key]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .to_vec()
    };
    let events = list("events");

    let mut statuses: Vec<String> = list("status")
        .iter()
        .filter_map(Value::as_str)
        .map(normalize_status)
        .collect();
    statuses.sort();
    statuses.dedup();

    let mut nameservers: Vec<String> = list("nameservers")
        .iter()
        .filter_map(|ns| ns["ldhName"].as_str())
        .map(clean_host)
        .collect();
    nameservers.sort();
    nameservers.dedup();

    Ok(Registration {
        statuses,
        registered: event_date(&events, "registration"),
        expires: event_date(&events, "expiration"),
        last_changed: event_date(&events, "last changed"),
        registrar: registrar(&list("entities")),
        nameservers,
    })
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    const COM: &str = include_str!("../../tests/fixtures/rdap_com_google.json");
    const ORG: &str = include_str!("../../tests/fixtures/rdap_org_wikipedia.json");
    const BOOTSTRAP: &str = include_str!("../../tests/fixtures/rdap_bootstrap_dns.json");

    fn utc(y: i32, m: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, mi, s).unwrap()
    }

    #[test]
    fn parses_verisign_com_response() {
        let reg = parse_rdap(COM).unwrap();
        assert_eq!(reg.statuses.len(), 6);
        assert!(reg
            .statuses
            .contains(&"client transfer prohibited".to_string()));
        assert_eq!(reg.registered, Some(utc(1997, 9, 15, 4, 0, 0)));
        assert_eq!(reg.expires, Some(utc(2028, 9, 14, 4, 0, 0)));
        assert_eq!(reg.last_changed, Some(utc(2019, 9, 9, 15, 39, 4)));
        assert_eq!(reg.registrar.as_deref(), Some("MarkMonitor Inc."));
        assert_eq!(
            reg.nameservers,
            [
                "ns1.google.com",
                "ns2.google.com",
                "ns3.google.com",
                "ns4.google.com"
            ]
        );
    }

    #[test]
    fn parses_pir_org_response() {
        let reg = parse_rdap(ORG).unwrap();
        assert!(!reg.statuses.is_empty());
        assert!(reg.registered.is_some());
        assert!(reg.expires.is_some());
        assert!(reg.registrar.is_some());
        assert!(!reg.nameservers.is_empty());
    }

    #[test]
    fn registrar_falls_back_to_handle() {
        let json = r#"{"entities":[{"handle":"292","roles":["registrar"],
            "vcardArray":["vcard",[["fn",{},"text",""]]]}]}"#;
        assert_eq!(parse_rdap(json).unwrap().registrar.as_deref(), Some("292"));
    }

    #[test]
    fn minimal_object_and_garbage() {
        assert_eq!(parse_rdap("{}").unwrap(), Registration::default());
        assert!(matches!(
            parse_rdap("<html>"),
            Err(CheckError::Permanent(_))
        ));
    }

    #[test]
    fn bootstrap_matches_tld_and_normalizes_urls() {
        let b = Bootstrap::parse(BOOTSTRAP).unwrap();
        assert_eq!(
            b.base_url("google.com"),
            Some("https://rdap.verisign.com/com/v1/")
        );
        assert_eq!(
            b.base_url("sub.example.com"),
            Some("https://rdap.verisign.com/com/v1/")
        );
        assert!(b.base_url("wikipedia.org").unwrap().ends_with('/'));
        assert_eq!(b.base_url("example.io"), None);
        assert_eq!(b.base_url("example.invalidtld"), None);
    }

    #[test]
    fn bootstrap_prefers_longest_suffix() {
        let json = r#"{"services":[[["uk"],["https://a.example"]],
            [["co.uk"],["https://b.example/"]],[["empty"],[]]]}"#;
        let b = Bootstrap::parse(json).unwrap();
        assert_eq!(b.base_url("foo.co.uk"), Some("https://b.example/"));
        assert_eq!(b.base_url("foo.org.uk"), Some("https://a.example/"));
        assert_eq!(b.base_url("foo.empty"), None);
        assert_eq!(b.base_url("uk"), None);
    }

    #[test]
    fn extracts_host_from_url() {
        assert_eq!(
            host_of("https://rdap.verisign.com/com/v1/"),
            "rdap.verisign.com"
        );
    }
}
