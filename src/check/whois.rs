//! WHOIS client (port 43) with IANA server discovery and tolerant text parsing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::{clean_host, normalize_status, CheckError, Lookup, Registration};
use crate::ratelimit::{retry, HostLimiter};

const IANA_SERVER: &str = "whois.iana.org";
const WHOIS_PORT: u16 = 43;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

const STATUS_KEYS: &[&str] = &["domain status", "status", "state"];
const EXPIRY_KEYS: &[&str] = &[
    "registry expiry date",
    "registrar registration expiration date",
    "expiration date",
    "expiry date",
    "expires on",
    "expires",
    "paid-till",
    "expire date",
];
const CREATED_KEYS: &[&str] = &[
    "creation date",
    "created",
    "created on",
    "registered",
    "registration time",
];
const UPDATED_KEYS: &[&str] = &["updated date", "last modified", "changed", "last updated"];
const REGISTRAR_KEYS: &[&str] = &["registrar", "sponsoring registrar", "registrar name"];
const NAMESERVER_KEYS: &[&str] = &["name server", "nserver", "nameserver"];
/// Headers whose following lines list name servers without a `key:` prefix.
const NAMESERVER_SECTIONS: &[&str] = &["domain servers", "name servers", "nameservers"];

const DATE_TIME_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M:%S",
    "%Y.%m.%d %H:%M:%S",
    "%Y/%m/%d %H:%M:%S",
];
const DATE_FORMATS: &[&str] = &[
    "%Y-%m-%d", "%Y.%m.%d", "%Y/%m/%d", "%d-%b-%Y", "%Y-%b-%d", "%d.%m.%Y",
];

pub struct Whois {
    limiter: Arc<HostLimiter>,
    timeout: Duration,
    not_found_phrases: Vec<String>,
    /// TLD -> WHOIS server, filled from IANA referrals for the process lifetime.
    servers: Mutex<HashMap<String, String>>,
}

impl Whois {
    pub fn new(
        limiter: Arc<HostLimiter>,
        timeout: Duration,
        not_found_phrases: Vec<String>,
    ) -> Self {
        Self {
            limiter,
            timeout,
            not_found_phrases,
            servers: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the WHOIS server used together with the parsed answer.
    pub async fn lookup(&self, domain: &str) -> Result<(String, Lookup), CheckError> {
        let tld = domain
            .trim_end_matches('.')
            .rsplit('.')
            .next()
            .unwrap_or(domain);
        let server = self.server_for(tld).await?;
        let text = self.query(&server, domain).await?;
        Ok((server, parse_whois(&text, &self.not_found_phrases)?))
    }

    async fn server_for(&self, tld: &str) -> Result<String, CheckError> {
        if let Some(server) = self.servers.lock().expect("whois cache poisoned").get(tld) {
            return Ok(server.clone());
        }
        let text = self.query(IANA_SERVER, tld).await?;
        let server = parse_iana_refer(&text).ok_or_else(|| {
            CheckError::Permanent(format!(
                "`.{tld}` uzantısı için ne RDAP ne de WHOIS sunucusu bulundu"
            ))
        })?;
        self.servers
            .lock()
            .expect("whois cache poisoned")
            .insert(tld.to_string(), server.clone());
        Ok(server)
    }

    async fn query(&self, server: &str, query: &str) -> Result<String, CheckError> {
        retry(|| async {
            self.limiter.until_ready(server).await;
            let bytes = tokio::time::timeout(self.timeout, exchange(server, query))
                .await
                .map_err(|_| CheckError::Transient(format!("WHOIS {server}: zaman aşımı")))??;
            if bytes.is_empty() {
                return Err(CheckError::Transient(format!("WHOIS {server}: boş yanıt")).into());
            }
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await
    }
}

/// One WHOIS exchange: send the query, read until EOF (capped).
async fn exchange(server: &str, query: &str) -> Result<Vec<u8>, CheckError> {
    let io_err = |e: std::io::Error| CheckError::Transient(format!("WHOIS {server}: {e}"));
    let mut stream = TcpStream::connect((server, WHOIS_PORT))
        .await
        .map_err(io_err)?;
    stream
        .write_all(format!("{query}\r\n").as_bytes())
        .await
        .map_err(io_err)?;
    let mut buf = Vec::new();
    stream
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut buf)
        .await
        .map_err(io_err)?;
    Ok(buf)
}

/// Splits a line into a lowercase key and trimmed value; handles `Key......: value`.
fn split_line(line: &str) -> Option<(String, &str)> {
    let (key, value) = line.split_once(':')?;
    let key = key
        .trim()
        .trim_start_matches('*')
        .trim()
        .trim_end_matches('.')
        .trim();
    Some((key.to_lowercase(), value.trim()))
}

/// Extracts the `refer:` / `whois:` server from an IANA TLD answer.
pub fn parse_iana_refer(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = split_line(line)?;
        let host = clean_host(value.split_whitespace().next()?);
        (matches!(key.as_str(), "refer" | "whois") && !host.is_empty()).then_some(host)
    })
}

/// Parses a date in any of the formats seen in WHOIS output; `None` if unrecognised.
pub fn parse_date(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim().trim_end_matches('.').trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    DATE_TIME_FORMATS
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(raw, f).ok())
        .or_else(|| {
            DATE_FORMATS
                .iter()
                .find_map(|f| NaiveDate::parse_from_str(raw, f).ok())
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })
        .map(|naive| naive.and_utc())
}

/// Interprets WHOIS text. A response that is neither a not-found answer nor contains any
/// registration data (e.g. a rate-limit notice) is reported as transient, never as free.
pub fn parse_whois(text: &str, not_found_phrases: &[String]) -> Result<Lookup, CheckError> {
    let mut reg = Registration::default();
    let mut in_ns_section = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            in_ns_section = false;
            continue;
        }
        if line.starts_with(['%', '#', '>']) {
            continue;
        }
        if in_ns_section {
            let first = line.split_whitespace().next().unwrap_or_default();
            if !first.contains(':') {
                reg.nameservers.push(clean_host(first));
                continue;
            }
            in_ns_section = false;
        }
        let Some((key, value)) = split_line(line) else {
            continue;
        };
        let key = key.as_str();
        if value.is_empty() {
            in_ns_section = NAMESERVER_SECTIONS.contains(&key);
            continue;
        }
        let first_token = value.split_whitespace().next().unwrap_or_default();
        if STATUS_KEYS.contains(&key) {
            reg.statuses.push(normalize_status(first_token));
        } else if EXPIRY_KEYS.contains(&key) {
            reg.expires = reg.expires.or_else(|| parse_date(value));
        } else if CREATED_KEYS.contains(&key) {
            reg.registered = reg.registered.or_else(|| parse_date(value));
        } else if UPDATED_KEYS.contains(&key) {
            reg.last_changed = reg.last_changed.or_else(|| parse_date(value));
        } else if REGISTRAR_KEYS.contains(&key) {
            reg.registrar.get_or_insert_with(|| value.to_string());
        } else if NAMESERVER_KEYS.contains(&key) {
            reg.nameservers.push(clean_host(first_token));
        }
    }

    reg.statuses.sort();
    reg.statuses.dedup();
    reg.nameservers.retain(|ns| !ns.is_empty());
    reg.nameservers.sort();
    reg.nameservers.dedup();

    let has_data = !reg.statuses.is_empty()
        || reg.expires.is_some()
        || reg.registered.is_some()
        || !reg.nameservers.is_empty();
    if has_data {
        return Ok(Lookup::Registered(reg));
    }
    let lower = text.to_lowercase();
    if not_found_phrases
        .iter()
        .any(|p| !p.is_empty() && lower.contains(&p.to_lowercase()))
    {
        return Ok(Lookup::NotFound);
    }
    Err(CheckError::Transient(
        "WHOIS yanıtı yorumlanamadı (hız sınırı olabilir)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    const COM_FOUND: &str = include_str!("../../tests/fixtures/whois_com_google.txt");
    const COM_NOT_FOUND: &str = include_str!("../../tests/fixtures/whois_com_notfound.txt");
    const IO_FOUND: &str = include_str!("../../tests/fixtures/whois_io_found.txt");
    const IO_NOT_FOUND: &str = include_str!("../../tests/fixtures/whois_io_notfound.txt");
    const TR_FOUND: &str = include_str!("../../tests/fixtures/whois_tr_found.txt");
    const TR_NOT_FOUND: &str = include_str!("../../tests/fixtures/whois_tr_notfound.txt");
    const IANA_TR: &str = include_str!("../../tests/fixtures/iana_tr.txt");
    const IANA_COM: &str = include_str!("../../tests/fixtures/iana_com.txt");

    fn phrases() -> Vec<String> {
        crate::config::Config::default()
            .detection
            .whois_not_found_phrases
    }

    fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    fn registered(text: &str) -> Registration {
        match parse_whois(text, &phrases()).unwrap() {
            Lookup::Registered(reg) => reg,
            Lookup::NotFound => panic!("expected registered"),
        }
    }

    #[test]
    fn parses_verisign_found() {
        let reg = registered(COM_FOUND);
        assert_eq!(reg.statuses.len(), 6);
        assert!(reg
            .statuses
            .contains(&"client transfer prohibited".to_string()));
        assert_eq!(
            reg.expires,
            Some(Utc.with_ymd_and_hms(2028, 9, 14, 4, 0, 0).unwrap())
        );
        assert_eq!(
            reg.registered,
            Some(Utc.with_ymd_and_hms(1997, 9, 15, 4, 0, 0).unwrap())
        );
        assert_eq!(
            reg.last_changed,
            Some(Utc.with_ymd_and_hms(2019, 9, 9, 15, 39, 4).unwrap())
        );
        assert_eq!(reg.registrar.as_deref(), Some("MarkMonitor Inc."));
        assert_eq!(reg.nameservers.len(), 4);
        assert_eq!(reg.nameservers[0], "ns1.google.com");
    }

    #[test]
    fn parses_io_found() {
        let reg = registered(IO_FOUND);
        assert_eq!(reg.statuses.len(), 3);
        assert_eq!(reg.nameservers.len(), 5);
        assert!(reg.expires.is_some());
    }

    #[test]
    fn not_found_outputs() {
        for text in [COM_NOT_FOUND, IO_NOT_FOUND, TR_NOT_FOUND] {
            assert_eq!(parse_whois(text, &phrases()).unwrap(), Lookup::NotFound);
        }
    }

    #[test]
    fn not_found_phrase_is_case_insensitive_and_configurable() {
        let custom = vec!["No Such Domain".to_string()];
        assert_eq!(
            parse_whois("domain NO SUCH DOMAIN.", &custom).unwrap(),
            Lookup::NotFound
        );
    }

    #[test]
    fn unrecognised_text_is_transient_not_free() {
        let err = parse_whois("Query rate limit exceeded", &phrases()).unwrap_err();
        assert!(matches!(err, CheckError::Transient(_)));
    }

    #[test]
    fn parses_tr_found() {
        let reg = registered(TR_FOUND);
        assert_eq!(reg.statuses, ["active"]);
        assert_eq!(reg.registered, Some(utc(2003, 3, 11)));
        assert_eq!(reg.expires, Some(utc(2027, 3, 10)));
        assert_eq!(reg.nameservers, ["ns1.metu.edu.tr", "ns2.metu.edu.tr"]);
    }

    #[test]
    fn tolerates_dotted_keys_and_leading_spaces() {
        let reg =
            registered("  Expires on..........: 2030-01-02\n  Name Server: NS.EXAMPLE.ORG.\n");
        assert_eq!(reg.expires, Some(utc(2030, 1, 2)));
        assert_eq!(reg.nameservers, ["ns.example.org"]);
    }

    #[test]
    fn parses_date_variants() {
        let want = Utc.with_ymd_and_hms(2026, 3, 8, 19, 12, 48).unwrap();
        for raw in [
            "2026-03-08T19:12:48Z",
            "2026-03-08T19:12:48+00:00",
            "2026-03-08 19:12:48",
            "2026.03.08 19:12:48",
        ] {
            assert_eq!(parse_date(raw), Some(want), "{raw}");
        }
        let midnight = Some(utc(2026, 3, 8));
        for raw in [
            "2026-03-08",
            "2026.03.08",
            "2026/03/08",
            "08-Mar-2026",
            "2026-Mar-08.",
            "08.03.2026",
        ] {
            assert_eq!(parse_date(raw), midnight, "{raw}");
        }
        for raw in ["", "n/a", "not a date", "REDACTED", "99-99-9999"] {
            assert_eq!(parse_date(raw), None, "{raw}");
        }
    }

    #[test]
    fn extracts_iana_referral() {
        assert_eq!(
            parse_iana_refer(IANA_TR).as_deref(),
            Some("whois.trabis.gov.tr")
        );
        assert_eq!(
            parse_iana_refer(IANA_COM).as_deref(),
            Some("whois.verisign-grs.com")
        );
        assert_eq!(parse_iana_refer("domain: XX\norganisation: none\n"), None);
    }
}
