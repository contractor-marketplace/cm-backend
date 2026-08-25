//! Google Cloud Storage, over the JSON API.
//!
//! Written against the HTTP API with `reqwest` rather than pulling in an SDK:
//! we need exactly three operations — upload one object, delete one object,
//! build one URL — and `reqwest` is already a workspace dependency, whereas a
//! Google SDK is a large tree carrying auth flows we do not use.
//!
//! Credentials come from the instance metadata server, so there is no key file
//! on disk and nothing to rotate. That is the reason the VM carries a dedicated
//! service account with `objectAdmin` on this one bucket rather than a broader
//! grant: the blast radius of the box being compromised is that bucket.
//!
//! Tokens last an hour. This caches one and refreshes it early, because a token
//! that expires mid-flight turns into a 401 on somebody's photo upload.

use cm_core::AppError;
use std::sync::Arc;
use tokio::sync::RwLock;

const METADATA_TOKEN_URL: &str = "http://metadata.google.internal/computeMetadata/v1/\
                                  instance/service-accounts/default/token";

/// Refresh this long before expiry. A minute is far longer than any request
/// here takes, so a token handed out is a token that stays valid for the whole
/// call.
const REFRESH_MARGIN: chrono::TimeDelta = chrono::TimeDelta::seconds(120);

#[derive(Clone)]
pub struct Bucket {
    name: Arc<str>,
    http: reqwest::Client,
    token: Arc<RwLock<Option<CachedToken>>>,
}

#[derive(Clone)]
struct CachedToken {
    value: String,
    expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: i64,
}

impl Bucket {
    pub fn new(name: &str) -> Result<Self, AppError> {
        let http = reqwest::Client::builder()
            // A photo upload that has not completed in half a minute is not
            // going to. Failing fast beats holding a request handler open.
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| AppError::internal(format!("building an HTTP client: {error}")))?;

        Ok(Self {
            name: Arc::from(name),
            http,
            token: Arc::new(RwLock::new(None)),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The public URL of an object.
    ///
    /// Objects are world-readable: a job's photos are exactly as public as its
    /// description, which is the decision this board already made. The URL
    /// contains two v7 UUIDs, so it is not guessable, but it is also not a
    /// secret — anyone who saved it keeps it. That is why cancelling a job
    /// deletes the objects rather than merely unlinking the rows.
    pub fn url_for(&self, key: &str) -> String {
        format!("https://storage.googleapis.com/{}/{}", self.name, key)
    }

    pub async fn put(&self, key: &str, bytes: &[u8]) -> Result<String, AppError> {
        let token = self.access_token().await?;
        let url = format!(
            "https://storage.googleapis.com/upload/storage/v1/b/{}/o\
             ?uploadType=media&name={}",
            self.name,
            urlencode(key)
        );

        let response = self
            .http
            .post(&url)
            .bearer_auth(token)
            .header(reqwest::header::CONTENT_TYPE, super::Normalised::CONTENT_TYPE)
            // A year, immutable: the key contains a fresh UUID, so an object at
            // a given key never changes. Re-uploading produces a new key.
            .header("Cache-Control", "public, max-age=31536000, immutable")
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|error| AppError::internal(format!("uploading to GCS: {error}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(AppError::internal(format!(
                "GCS refused an upload: {status} {body}"
            )));
        }

        Ok(self.url_for(key))
    }

    pub async fn delete(&self, key: &str) -> Result<(), AppError> {
        let token = self.access_token().await?;
        let url = format!(
            "https://storage.googleapis.com/storage/v1/b/{}/o/{}",
            self.name,
            urlencode(key)
        );

        let response = self
            .http
            .delete(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| AppError::internal(format!("deleting from GCS: {error}")))?;

        // 404 is success: the object is not there, which is what was asked for.
        // Treating it as an error would make a retried delete fail.
        if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(AppError::internal(format!(
            "GCS refused a delete: {status} {body}"
        )))
    }

    /// A valid access token, cached.
    async fn access_token(&self) -> Result<String, AppError> {
        let now = chrono::Utc::now();

        if let Some(cached) = self.token.read().await.as_ref() {
            if cached.expires_at > now + REFRESH_MARGIN {
                return Ok(cached.value.clone());
            }
        }

        // Re-check under the write lock: several uploads can arrive together and
        // find the token stale, and one fetch between them is enough.
        let mut slot = self.token.write().await;
        if let Some(cached) = slot.as_ref() {
            if cached.expires_at > now + REFRESH_MARGIN {
                return Ok(cached.value.clone());
            }
        }

        let fetched: TokenResponse = self
            .http
            .get(METADATA_TOKEN_URL)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|error| {
                AppError::internal(format!("reaching the instance metadata server: {error}"))
            })?
            .error_for_status()
            .map_err(|error| {
                AppError::internal(format!("the metadata server refused a token: {error}"))
            })?
            .json()
            .await
            .map_err(|error| {
                AppError::internal(format!("parsing a metadata token response: {error}"))
            })?;

        let token = CachedToken {
            value: fetched.access_token,
            expires_at: now + chrono::TimeDelta::seconds(fetched.expires_in),
        };
        let value = token.value.clone();
        *slot = Some(token);

        tracing::debug!(expires_in = fetched.expires_in, "refreshed a GCS access token");
        Ok(value)
    }
}

/// Percent-encode an object name for a URL path segment.
///
/// Keys here are `jobs/{uuid}/{uuid}.jpg`, so in practice only the slashes need
/// it — but encoding by rule rather than by what today's keys happen to contain
/// is what stops a future key shape from silently producing a wrong URL.
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_names_are_encoded_for_the_path() {
        assert_eq!(urlencode("jobs/abc/def.jpg"), "jobs%2Fabc%2Fdef.jpg");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("plain-name_1.0~x"), "plain-name_1.0~x");
    }

    #[test]
    fn the_public_url_is_the_bucket_and_the_raw_key() {
        let bucket = Bucket::new("cm-job-photos-test").expect("client");
        assert_eq!(
            bucket.url_for("jobs/a/b.jpg"),
            "https://storage.googleapis.com/cm-job-photos-test/jobs/a/b.jpg"
        );
    }
}
