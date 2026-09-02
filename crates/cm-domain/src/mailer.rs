//! Outbound email.
//!
//! An enum rather than a trait object, for the same reason `Store` is: there
//! are exactly two implementations — Resend, and memory for development and
//! tests — and there is no plausible third, so the dispatch is not worth a
//! `dyn` and the match is worth reading.
//!
//! Written against Resend's HTTP API with `reqwest` rather than an SDK: we
//! need exactly one operation — send one message — and `reqwest` is already a
//! workspace dependency. The outbox row's id is sent as the idempotency key,
//! so a retry after an ambiguous failure (timeout after the provider accepted)
//! cannot deliver the same message twice.

use cm_core::AppError;
use std::sync::{Arc, Mutex};

const DEFAULT_ENDPOINT: &str = "https://api.resend.com";

/// One fully rendered message, as the worker hands it to a provider.
#[derive(Debug, Clone)]
pub struct Email {
    /// The outbox row id; doubles as the provider idempotency key.
    pub id: uuid::Uuid,
    pub to: String,
    pub subject: String,
    pub body_text: String,
    pub body_html: Option<String>,
    /// When present, sent as `List-Unsubscribe` + `List-Unsubscribe-Post`
    /// headers — the one-click unsubscribe Gmail and Yahoo require of bulk
    /// senders.
    pub unsubscribe_url: Option<String>,
}

#[derive(Clone)]
pub enum Mailer {
    Resend(ResendMailer),
    Memory(MemoryMailer),
}

impl Mailer {
    pub fn memory() -> Self {
        Self::Memory(MemoryMailer::default())
    }

    /// Deliver one message. Returns the provider's message id, when it gives
    /// one. An error means the message may retry; it never means it must not.
    pub async fn send(&self, email: Email) -> Result<Option<String>, AppError> {
        match self {
            Self::Resend(resend) => resend.send(email).await,
            Self::Memory(memory) => Ok(memory.send(email)),
        }
    }

    /// For the startup log line.
    pub fn describe(&self) -> String {
        match self {
            Self::Resend(resend) => format!("resend ({})", resend.from),
            Self::Memory(_) => "memory (mail is logged, not delivered)".to_owned(),
        }
    }
}

pub struct ResendMailer {
    endpoint: String,
    from: String,
    api_key: cm_core::Secret<String>,
    http: reqwest::Client,
}

impl Clone for ResendMailer {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            from: self.from.clone(),
            api_key: self.api_key.clone(),
            http: self.http.clone(),
        }
    }
}

#[derive(serde::Serialize)]
struct SendRequest<'a> {
    from: &'a str,
    to: [&'a str; 1],
    subject: &'a str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    headers: Option<std::collections::BTreeMap<&'static str, String>>,
}

#[derive(serde::Deserialize)]
struct SendResponse {
    id: Option<String>,
}

impl ResendMailer {
    /// `endpoint` overrides the API base URL, for a stub or recorded service —
    /// the same escape hatch the geocoder's `--endpoint` provides.
    pub fn new(mail: &cm_core::MailConfig, endpoint: Option<String>) -> Result<Self, AppError> {
        let http = reqwest::Client::builder()
            // A send that has not completed in half a minute is not going to.
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| AppError::internal(format!("building an HTTP client: {error}")))?;

        Ok(Self {
            endpoint: endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned()),
            from: mail.from.clone(),
            api_key: mail.resend_api_key.clone(),
            http,
        })
    }

    async fn send(&self, email: Email) -> Result<Option<String>, AppError> {
        let headers = email.unsubscribe_url.as_ref().map(|url| {
            std::collections::BTreeMap::from([
                ("List-Unsubscribe", format!("<{url}>")),
                (
                    "List-Unsubscribe-Post",
                    "List-Unsubscribe=One-Click".to_owned(),
                ),
            ])
        });

        let response = self
            .http
            .post(format!("{}/emails", self.endpoint))
            .bearer_auth(self.api_key.expose())
            // A retried outbox row must not become a second email.
            .header("Idempotency-Key", email.id.to_string())
            .json(&SendRequest {
                from: &self.from,
                to: [email.to.as_str()],
                subject: &email.subject,
                text: &email.body_text,
                html: email.body_html.as_deref(),
                headers,
            })
            .send()
            .await
            .map_err(|error| AppError::internal(format!("reaching Resend: {error}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::internal(format!(
                "Resend refused a send: {status} {body}"
            )));
        }

        let parsed: SendResponse = response
            .json()
            .await
            .map_err(|error| AppError::internal(format!("parsing a Resend response: {error}")))?;
        Ok(parsed.id)
    }
}

/// Records instead of delivering. Development runs on this so the codes and
/// links land in the log; tests assert against `sent()`.
#[derive(Clone, Default)]
pub struct MemoryMailer {
    sent: Arc<Mutex<Vec<Email>>>,
}

impl MemoryMailer {
    fn send(&self, email: Email) -> Option<String> {
        tracing::info!(
            to = %email.to,
            subject = %email.subject,
            body = %email.body_text,
            "mail (memory mailer — not delivered)"
        );
        self.sent.lock().expect("mailer lock").push(email);
        None
    }

    /// For assertions.
    pub fn sent(&self) -> Vec<Email> {
        self.sent.lock().expect("mailer lock").clone()
    }
}
