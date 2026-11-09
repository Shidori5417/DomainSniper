//! Telegram Bot API sender (sendMessage, HTML parse mode, 429 handling).

use serde_json::{json, Value};

use super::{transport_error, Message, Reply, SendError};

const DEFAULT_BASE_URL: &str = "https://api.telegram.org";

pub struct Telegram {
    http: reqwest::Client,
    /// API root without trailing slash; a field so tests can point it at a mock server.
    base_url: String,
    bot_token: String,
    chat_id: String,
}

impl Telegram {
    pub fn new(http: reqwest::Client, bot_token: String, chat_id: String) -> Telegram {
        Telegram {
            http,
            base_url: DEFAULT_BASE_URL.into(),
            bot_token,
            chat_id,
        }
    }

    pub async fn send(&self, message: &Message) -> Result<u16, SendError> {
        let url = format!("{}/bot{}/sendMessage", self.base_url, self.bot_token);
        let response = self
            .http
            .post(url)
            .json(&self.payload(message))
            .send()
            .await
            .map_err(transport_error)?;
        Reply::read(response).await.into_result(|body| {
            body.pointer("/parameters/retry_after")
                .and_then(Value::as_f64)
        })
    }

    fn payload(&self, message: &Message) -> Value {
        json!({
            "chat_id": self.chat_id,
            "text": render(message),
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        })
    }
}

/// Escapes the characters Telegram's HTML parse mode treats specially.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Bold title, then the body; every dynamic part is escaped.
fn render(message: &Message) -> String {
    let title = escape_html(&message.title);
    if message.body.is_empty() {
        format!("<b>{title}</b>")
    } else {
        format!("<b>{title}</b>\n{}", escape_html(&message.body))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::notify::mock::{Canned, MockServer};
    use crate::notify::Level;

    fn message(title: &str, body: &str) -> Message {
        Message {
            title: title.into(),
            body: body.into(),
            level: Level::Info,
            high_priority: false,
        }
    }

    fn sender(base: String) -> Telegram {
        Telegram {
            http: reqwest::Client::new(),
            base_url: base,
            bot_token: "123:SECRET".into(),
            chat_id: "-100".into(),
        }
    }

    #[test]
    fn html_special_characters_are_escaped() {
        let m = message("🔴 <a&b>.com", "x < y && y > z");
        assert_eq!(
            render(&m),
            "<b>🔴 &lt;a&amp;b&gt;.com</b>\nx &lt; y &amp;&amp; y &gt; z"
        );
        assert_eq!(render(&message("t", "")), "<b>t</b>");
    }

    #[tokio::test]
    async fn ok_response_and_request_shape() {
        let server = MockServer::start(vec![Canned::json(200, r#"{"ok":true,"result":{}}"#)]).await;
        let telegram = sender(server.url(""));
        assert_eq!(telegram.send(&message("başlık", "g&vde")).await, Ok(200));
        let requests = server.requests();
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/bot123:SECRET/sendMessage");
        let body: Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["chat_id"], "-100");
        assert_eq!(body["text"], "<b>başlık</b>\ng&amp;vde");
        assert_eq!(body["parse_mode"], "HTML");
        assert_eq!(body["disable_web_page_preview"], true);
    }

    #[tokio::test]
    async fn rate_limit_reads_parameters_retry_after() {
        let body = r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3","parameters":{"retry_after":3}}"#;
        let server = MockServer::start(vec![Canned::json(429, body)]).await;
        let err = sender(server.url(""))
            .send(&message("t", "b"))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            SendError::RateLimited {
                retry_after: Duration::from_secs(3)
            }
        );
    }

    #[tokio::test]
    async fn server_error_is_a_failure_and_never_mentions_token() {
        let server = MockServer::start(vec![Canned::json(500, r#"{"ok":false}"#)]).await;
        let err = sender(server.url(""))
            .send(&message("t", "b"))
            .await
            .unwrap_err();
        assert_eq!(
            err,
            SendError::Failed {
                status: Some(500),
                reason: r#"{"ok":false}"#.into()
            }
        );
        assert!(!err.to_string().contains("SECRET"));
    }

    #[tokio::test]
    async fn connection_failure_does_not_leak_token() {
        let base = {
            let server = MockServer::start(vec![Canned::status(200)]).await;
            server.url("")
        };
        tokio::task::yield_now().await;
        let err = sender(base).send(&message("t", "b")).await.unwrap_err();
        assert!(
            matches!(err, SendError::Failed { status: None, .. }),
            "{err:?}"
        );
        assert!(!err.to_string().contains("SECRET"), "{err}");
    }
}
