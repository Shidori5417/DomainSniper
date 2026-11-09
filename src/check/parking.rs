//! Parking NS matching and HTTP for-sale detection.

use std::time::Duration;

use anyhow::Context;
use reqwest::Url;

use super::{clean_host, MAX_REDIRECTS};
use crate::config::{Detection, General};

const MAX_BODY_BYTES: usize = 200 * 1024;
const MAX_HTTP_TIMEOUT_SECS: u64 = 8;
/// GoDaddy hosts regular sites too, so only these URL path markers count as "for sale".
const GODADDY_HOST: &str = "godaddy.com";
const GODADDY_SALE_PATHS: [&str; 2] = ["forsale", "domainsearch"];

/// Why a registered domain looks parked or for sale (shown in notifications and history).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkingEvidence {
    pub reason: String,
}

/// Returns the first name server that ends on a label boundary with a `park_ns` entry.
pub fn ns_is_parked(ns: &[String], park_ns: &[String]) -> Option<String> {
    let park: Vec<String> = park_ns.iter().map(|p| clean_host(p)).collect();
    ns.iter().map(|n| clean_host(n)).find(|host| {
        park.iter().any(|p| {
            host == p
                || host
                    .strip_suffix(p.as_str())
                    .is_some_and(|rest| rest.ends_with('.'))
        })
    })
}

fn host_is_or_under(host: &str, parent: &str) -> bool {
    let parent = clean_host(parent);
    host == parent
        || host
            .strip_suffix(parent.as_str())
            .is_some_and(|rest| rest.ends_with('.'))
}

/// True if the (final) URL points at a known marketplace.
pub fn is_market_url(url: &Url, markets: &[String]) -> bool {
    let Some(host) = url.host_str().map(clean_host) else {
        return false;
    };
    if host_is_or_under(&host, GODADDY_HOST) {
        let path = url.path().to_lowercase();
        if GODADDY_SALE_PATHS
            .iter()
            .any(|marker| path.contains(marker))
        {
            return true;
        }
    }
    markets.iter().any(|m| host_is_or_under(&host, m))
}

/// Lowercases and folds Turkish dotted/dotless i so "ALAN ADI SATILIK" matches "alan adı satılık".
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            'İ' | 'ı' => out.push('i'),
            // Combining dot above left behind by lowercasing other dotted capitals.
            '\u{307}' => {}
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// Returns the first phrase (as configured) contained in `body`, ignoring case and Turkish i forms.
pub fn body_has_sale_phrase<'a>(body: &str, phrases: &'a [String]) -> Option<&'a str> {
    let body = fold(body);
    phrases
        .iter()
        .find(|p| !p.trim().is_empty() && body.contains(&fold(p)))
        .map(String::as_str)
}

pub struct Parking {
    http: reqwest::Client,
    park_ns: Vec<String>,
    market_domains: Vec<String>,
    for_sale_phrases: Vec<String>,
}

impl Parking {
    pub fn new(general: &General, detection: &Detection) -> anyhow::Result<Self> {
        let timeout = Duration::from_secs(general.http_timeout_secs.min(MAX_HTTP_TIMEOUT_SECS));
        let http = reqwest::Client::builder()
            .user_agent(&general.user_agent)
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
            .build()
            .context("park tespiti HTTP istemcisi oluşturulamadı")?;
        Ok(Self {
            http,
            park_ns: detection.park_ns.clone(),
            market_domains: detection.market_domains.clone(),
            for_sale_phrases: detection.for_sale_phrases.clone(),
        })
    }

    /// NS match first; the HTTP probe only runs when the name servers say nothing.
    pub async fn detect(&self, domain: &str, ns: &[String]) -> Option<ParkingEvidence> {
        if let Some(host) = ns_is_parked(ns, &self.park_ns) {
            return Some(ParkingEvidence {
                reason: format!("NS: {host}"),
            });
        }
        self.probe_http(domain).await
    }

    /// Any HTTP failure just means "no evidence".
    async fn probe_http(&self, domain: &str) -> Option<ParkingEvidence> {
        let mut response = self
            .http
            .get(format!("http://{domain}/"))
            .send()
            .await
            .ok()?;
        let url = response.url().clone();
        if is_market_url(&url, &self.market_domains) {
            return Some(ParkingEvidence {
                reason: format!("pazar yeri yönlendirmesi: {url}"),
            });
        }
        let mut body = Vec::new();
        while body.len() < MAX_BODY_BYTES {
            match response.chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(_) => return None,
            }
        }
        body.truncate(MAX_BODY_BYTES);
        let text = String::from_utf8_lossy(&body);
        body_has_sale_phrase(&text, &self.for_sale_phrases).map(|phrase| ParkingEvidence {
            reason: format!("sayfada \"{phrase}\" ifadesi"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn ns_matches_on_label_boundary_at_end() {
        let park = list(&["sedoparking.com", "dan.com"]);
        assert_eq!(
            ns_is_parked(&list(&["ns1.example.net", "NS1.SedoParking.com."]), &park).as_deref(),
            Some("ns1.sedoparking.com")
        );
        assert_eq!(
            ns_is_parked(&list(&["sedoparking.com"]), &park).as_deref(),
            Some("sedoparking.com")
        );
        assert_eq!(
            ns_is_parked(&list(&["ns1.notsedoparking.com"]), &park),
            None
        );
        assert_eq!(ns_is_parked(&list(&["ns1.dan.com.evil.org"]), &park), None);
        assert_eq!(ns_is_parked(&list(&["ns1.domaincontrol.com"]), &park), None);
        assert_eq!(ns_is_parked(&[], &park), None);
    }

    #[test]
    fn market_hosts_and_subdomains_match() {
        let markets = list(&["dan.com", "sedo.com"]);
        assert!(is_market_url(
            &url("https://dan.com/buy-domain/x.com"),
            &markets
        ));
        assert!(is_market_url(
            &url("https://www.sedo.com/search/?x"),
            &markets
        ));
        assert!(!is_market_url(&url("https://notdan.com/"), &markets));
        assert!(!is_market_url(
            &url("https://example.com/dan.com"),
            &markets
        ));
    }

    #[test]
    fn godaddy_requires_sale_path() {
        let markets = list(&[]);
        assert!(is_market_url(
            &url("https://www.godaddy.com/forsale/x.com"),
            &markets
        ));
        assert!(is_market_url(
            &url("https://godaddy.com/DomainSearch/find?d=x"),
            &markets
        ));
        assert!(!is_market_url(
            &url("https://www.godaddy.com/hosting"),
            &markets
        ));
        assert!(!is_market_url(
            &url("https://example.com/forsale"),
            &markets
        ));
    }

    #[test]
    fn sale_phrases_ignore_case_and_turkish_i() {
        let phrases = list(&[
            "this domain is for sale",
            "bu alan adı satılıktır",
            "alan adı satılık",
        ]);
        assert_eq!(
            body_has_sale_phrase("<h1>THIS DOMAIN IS FOR SALE!</h1>", &phrases),
            Some("this domain is for sale")
        );
        assert_eq!(
            body_has_sale_phrase("ALAN ADI SATILIK", &phrases),
            Some("alan adı satılık")
        );
        assert_eq!(
            body_has_sale_phrase("Alan Adi Satilik", &phrases),
            Some("alan adı satılık")
        );
        assert_eq!(
            body_has_sale_phrase("BU ALAN ADI SATILIKTIR", &phrases),
            Some("bu alan adı satılıktır")
        );
        assert_eq!(body_has_sale_phrase("on sale now", &phrases), None);
        assert_eq!(body_has_sale_phrase("anything", &list(&[""])), None);
    }
}
