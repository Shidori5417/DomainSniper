//! Discord webhook sender (one embed per message, mentions, 429 retry_after handling).

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use super::{transport_error, Level, Message, Reply, SendError};

const EMBED_DESCRIPTION_LIMIT: usize = 4000;

pub struct Discord {
    http: reqwest::Client,
    webhook_url: String,
    mention: String,
}

impl Discord {
    pub fn new(http: reqwest::Client, webhook_url: String, mention: String) -> Discord {
        Discord {
            http,
            webhook_url,
            mention: mention.trim().to_string(),
        }
    }

    pub async fn send(&self, message: &Message) -> Result<u16, SendError> {
        let body = self.payload(message, Utc::now());
        let response = self
            .http
            .post(&self.webhook_url)
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        Reply::read(response)
            .await
            .into_result(|body| body.get("retry_after").and_then(Value::as_f64))
    }

    /// The webhook JSON body for `message`.
    fn payload(&self, message: &Message, now: DateTime<Utc>) -> Value {
        let mut description = message.body.clone();
        if description.chars().count() > EMBED_DESCRIPTION_LIMIT {
            description = description
                .chars()
                .take(EMBED_DESCRIPTION_LIMIT)
                .collect::<String>()
                + "…";
        }
        let embed = json!({
            "title": message.title,
            "description": description,
            "color": color(message.level),
            "timestamp": now.to_rfc3339_opts(SecondsFormat::Secs, true),
        });
        if message.high_priority && !self.mention.is_empty() {
            json!({
                "content": self.mention,
                "allowed_mentions": allowed_mentions(&self.mention),
                "embeds": [embed],
            })
        } else {
            json!({ "allowed_mentions": { "parse": [] }, "embeds": [embed] })
        }
    }
}

fn color(level: Level) -> u32 {
    match level {
        Level::Info => 0x3498DB,
        Level::Warning => 0xF1C40F,
        Level::Orange => 0xE67E22,
        Level::Critical => 0xE74C3C,
        Level::Success => 0x2ECC71,
        Level::Purple => 0x9B59B6,
        Level::Muted => 0x95A5A6,
    }
}

/// `allowed_mentions` that lets exactly the configured mentions ping: `@everyone` / `@here`
/// through `parse`, `<@&ROLE>` and `<@USER>` through explicit id lists.
fn allowed_mentions(mention: &str) -> Value {
    let (mut everyone, mut roles, mut users) = (false, Vec::new(), Vec::new());
    for token in mention.split_whitespace() {
        if token == "@everyone" || token == "@here" {
            everyone = true;
        } else if let Some(id) = token.strip_prefix("<@&").and_then(|t| t.strip_suffix('>')) {
            roles.push(id.to_string());
        } else if let Some(id) = token.strip_prefix("<@").and_then(|t| t.strip_suffix('>')) {
            users.push(id.trim_start_matches('!').to_string());
        }
    }
    let mut allowed = serde_json::Map::new();
    allowed.insert(
        "parse".into(),
        json!(if everyone { vec!["everyone"] } else { vec![] }),
    );
    if !roles.is_empty() {
        allowed.insert("roles".into(), json!(roles));
    }
    if !users.is_empty() {
        allowed.insert("users".into(), json!(users));
    }
    Value::Object(allowed)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::TimeZone;

    use super::*;
    use crate::notify::mock::{Canned, MockServer};

    fn sender(url: String, mention: &str) -> Discord {
        Discord::new(reqwest::Client::new(), url, mention.into())
    }

    fn message(high_priority: bool, level: Level) -> Message {
        Message {
            title: "🟢 a.com BOŞTA".into(),
            body: "gövde".into(),
            level,
            high_priority,
        }
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap()
    }

    #[test]
    fn colors_follow_level() {
        assert_eq!(color(Level::Success), 0x2ECC71);
        assert_eq!(color(Level::Critical), 0xE74C3C);
        assert_eq!(color(Level::Orange), 0xE67E22);
        assert_eq!(color(Level::Purple), 0x9B59B6);
        let all = [
            Level::Info,
            Level::Warning,
            Level::Orange,
            Level::Critical,
            Level::Success,
            Level::Purple,
            Level::Muted,
        ];
        let distinct: std::collections::HashSet<u32> = all.into_iter().map(color).collect();
        assert_eq!(distinct.len(), all.len());
    }

    #[test]
    fn everyone_mention_pings_on_high_priority() {
        let body =
            sender("http://x".into(), " @everyone ").payload(&message(true, Level::Success), now());
        assert_eq!(body["content"], "@everyone");
        assert_eq!(body["allowed_mentions"], json!({ "parse": ["everyone"] }));
        let embed = &body["embeds"][0];
        assert_eq!(embed["title"], "🟢 a.com BOŞTA");
        assert_eq!(embed["description"], "gövde");
        assert_eq!(embed["color"], 0x2ECC71);
        assert_eq!(embed["timestamp"], "2026-11-09T12:00:00Z");
    }

    #[test]
    fn role_mention_uses_explicit_role_list() {
        let body =
            sender("http://x".into(), "<@&12345>").payload(&message(true, Level::Success), now());
        assert_eq!(body["content"], "<@&12345>");
        assert_eq!(
            body["allowed_mentions"],
            json!({ "parse": [], "roles": ["12345"] })
        );
    }

    #[test]
    fn user_mention_uses_explicit_user_list() {
        let body =
            sender("http://x".into(), "<@!42>").payload(&message(true, Level::Success), now());
        assert_eq!(
            body["allowed_mentions"],
            json!({ "parse": [], "users": ["42"] })
        );
    }

    #[test]
    fn no_mention_for_normal_priority_or_empty_config() {
        let normal =
            sender("http://x".into(), "@everyone").payload(&message(false, Level::Info), now());
        assert!(normal.get("content").is_none());
        assert_eq!(normal["allowed_mentions"], json!({ "parse": [] }));
        let empty = sender("http://x".into(), "  ").payload(&message(true, Level::Success), now());
        assert!(empty.get("content").is_none());
        assert_eq!(empty["allowed_mentions"], json!({ "parse": [] }));
    }

    #[test]
    fn long_description_is_truncated() {
        let mut m = message(false, Level::Info);
        m.body = "x".repeat(5000);
        let body = sender("http://x".into(), "").payload(&m, now());
        assert_eq!(
            body["embeds"][0]["description"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            4001
        );
    }

    #[tokio::test]
    async fn no_content_204_is_ok_and_request_shape_is_right() {
        let server = MockServer::start(vec![Canned::status(204)]).await;
        let discord = sender(server.url("/api/webhooks/1/tok"), "@everyone");
        assert_eq!(discord.send(&message(true, Level::Success)).await, Ok(204));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/api/webhooks/1/tok");
        let body: Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["content"], "@everyone");
        assert_eq!(body["embeds"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rate_limit_reads_retry_after_from_json_body() {
        let server = MockServer::start(vec![Canned::json(
            429,
            r#"{"retry_after": 2.5, "global": false}"#,
        )])
        .await;
        let err = sender(server.url("/hook"), "")
            .send(&message(false, Level::Info))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            SendError::RateLimited {
                retry_after: Duration::from_millis(2500)
            }
        );
    }

    #[tokio::test]
    async fn rate_limit_falls_back_to_header() {
        let server = MockServer::start(vec![Canned::status(429).header("Retry-After", "7")]).await;
        let err = sender(server.url("/hook"), "")
            .send(&message(false, Level::Info))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            SendError::RateLimited {
                retry_after: Duration::from_secs(7)
            }
        );
    }

    #[tokio::test]
    async fn server_error_is_a_failure_without_url() {
        let server = MockServer::start(vec![Canned::json(500, r#"{"message":"boom"}"#)]).await;
        let url = server.url("/secret-token-path");
        let err = sender(url.clone(), "")
            .send(&message(false, Level::Info))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            SendError::Failed {
                status: Some(500),
                reason: r#"{"message":"boom"}"#.into()
            }
        );
        assert!(!err.to_string().contains("secret-token-path"));
    }

    #[tokio::test]
    async fn connection_failure_does_not_leak_url() {
        let url = {
            let server = MockServer::start(vec![Canned::status(204)]).await;
            server.url("/secret-token-path")
        };
        tokio::task::yield_now().await;
        let err = sender(url, "")
            .send(&message(false, Level::Info))
            .await
            .unwrap_err();
        assert!(
            matches!(err, SendError::Failed { status: None, .. }),
            "{err:?}"
        );
        assert!(!err.to_string().contains("secret-token-path"), "{err}");
        assert!(!err.to_string().contains("127.0.0.1"), "{err}");
    }
}
