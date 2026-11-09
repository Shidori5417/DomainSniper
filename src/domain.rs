//! Domain input normalization and IDN/punycode conversion.

use anyhow::{bail, Result};

const MAX_LABEL_LEN: usize = 63;
const MAX_DOMAIN_LEN: usize = 253;

/// A validated domain in both its DNS (punycode) and human-readable forms.
#[derive(Debug, PartialEq, Eq)]
pub struct NormalizedDomain {
    /// Lowercase punycode form, used as the database key.
    pub ascii: String,
    /// Unicode form shown to the user.
    pub display: String,
}

/// Turns user input such as `HTTPS://www.Örnek.com.tr/` into a normalized domain.
pub fn normalize(input: &str) -> Result<NormalizedDomain> {
    let mut host = input.trim();
    if let Some((_, rest)) = host.split_once("://") {
        host = rest;
    }
    if let Some(end) = host.find(['/', '?', '#']) {
        host = &host[..end];
    }
    if let Some((_, rest)) = host.rsplit_once('@') {
        host = rest;
    }
    if let Some((rest, port)) = host.rsplit_once(':') {
        if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
            host = rest;
        }
    }
    let host = host.trim_end_matches('.').to_lowercase();
    let host = match host.strip_prefix("www.") {
        Some(rest) if rest.contains('.') => rest,
        _ => &host,
    };

    if host.is_empty() {
        bail!("boş domain");
    }
    let ascii = idna::domain_to_ascii_strict(host)
        .map_err(|_| anyhow::anyhow!("geçersiz domain: {host}"))?;
    validate_ascii(&ascii)?;
    let (display, _) = idna::domain_to_unicode(&ascii);
    Ok(NormalizedDomain { ascii, display })
}

fn validate_ascii(ascii: &str) -> Result<()> {
    if !ascii.contains('.') {
        bail!("domain en az iki etiket içermeli (örn. ornek.com): {ascii}");
    }
    if ascii.len() > MAX_DOMAIN_LEN {
        bail!("domain {MAX_DOMAIN_LEN} karakterden uzun olamaz");
    }
    for label in ascii.split('.') {
        if label.is_empty() {
            bail!("domain boş etiket içeriyor: {ascii}");
        }
        if label.len() > MAX_LABEL_LEN {
            bail!("etiket {MAX_LABEL_LEN} karakterden uzun olamaz: {label}");
        }
        let valid_chars = label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !valid_chars || label.starts_with('-') || label.ends_with('-') {
            bail!("geçersiz etiket: {label}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idn_url_is_normalized() {
        let d = normalize("HTTPS://www.Örnek.com.tr/").unwrap();
        assert_eq!(d.ascii, "xn--rnek-4qa.com.tr");
        assert_eq!(d.display, "örnek.com.tr");
    }

    #[test]
    fn strips_decorations() {
        for input in [
            "  Example.COM. ",
            "http://example.com/path?q=1#frag",
            "ftp://user:pw@example.com:8080/x",
            "www.example.com/",
            "custom+scheme://example.com",
        ] {
            assert_eq!(normalize(input).unwrap().ascii, "example.com", "{input}");
        }
    }

    #[test]
    fn keeps_www_when_it_is_the_label_before_tld() {
        assert_eq!(normalize("www.com").unwrap().ascii, "www.com");
    }

    #[test]
    fn rejects_invalid_input() {
        let long_label = format!("{}.com", "a".repeat(64));
        let long_domain = format!("{}.com", vec!["a".repeat(60); 5].join("."));
        for input in [
            "",
            "   ",
            "https://",
            "localhost",
            "a..com",
            "exa mple.com",
            "-bad.com",
            "bad-.com",
            "under_score.com",
            &long_label,
            &long_domain,
        ] {
            assert!(normalize(input).is_err(), "{input:?} should be rejected");
        }
    }
}
