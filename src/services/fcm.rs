//! Firebase Cloud Messaging client. iOS is delivered through FCM as well.
//!
//! Follows the same mock-when-unconfigured convention as the SMTP and
//! SafeHaven integrations: when `FCM_SERVER_KEY` is unset the client logs and
//! reports delivery without making a network call, so local/CI runs need no
//! credentials.

use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum FcmError {
    #[error("FCM request failed: {0}")]
    Request(String),
}

/// Outcome of a single-token send, used by the caller to prune dead tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Delivered,
    /// FCM reports the token is no longer valid; the caller should revoke it.
    InvalidToken,
}

pub struct FcmClient {
    http: reqwest::Client,
    /// Legacy FCM server key. `None` puts the client in mock mode.
    server_key: Option<String>,
}

impl FcmClient {
    const ENDPOINT: &'static str = "https://fcm.googleapis.com/fcm/send";

    pub fn new(server_key: Option<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            server_key: server_key.filter(|k| !k.trim().is_empty()),
        }
    }

    pub fn from_env() -> Self {
        Self::new(std::env::var("FCM_SERVER_KEY").ok())
    }

    /// Whether the client will actually contact FCM.
    pub fn is_live(&self) -> bool {
        self.server_key.is_some()
    }

    /// Deliver a notification to a single device token.
    pub async fn send(
        &self,
        token: &str,
        title: &str,
        body: &str,
        data: &serde_json::Value,
    ) -> Result<PushOutcome, FcmError> {
        let Some(key) = self.server_key.as_deref() else {
            tracing::info!("[PUSH mock] token={token} title={title} body={body}");
            return Ok(PushOutcome::Delivered);
        };

        let payload = json!({
            "to": token,
            "notification": { "title": title, "body": body },
            "data": data,
        });

        let resp = self
            .http
            .post(Self::ENDPOINT)
            .header("Authorization", format!("key={key}"))
            .json(&payload)
            .send()
            .await
            .map_err(|e| FcmError::Request(e.to_string()))?;

        let status = resp.status();
        let json: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| FcmError::Request(e.to_string()))?;

        if !status.is_success() {
            return Err(FcmError::Request(format!("FCM returned {status}: {json}")));
        }

        // A `NotRegistered` / `InvalidRegistration` result means the token is
        // dead and should be pruned.
        let dead = json
            .get("results")
            .and_then(|r| r.as_array())
            .and_then(|arr| arr.first())
            .and_then(|first| first.get("error"))
            .and_then(|e| e.as_str())
            .map(|e| matches!(e, "NotRegistered" | "InvalidRegistration"))
            .unwrap_or(false);

        Ok(if dead {
            PushOutcome::InvalidToken
        } else {
            PushOutcome::Delivered
        })
    }
}
