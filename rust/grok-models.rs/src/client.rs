//! Blocking JSON HTTP client.
//!
//! GETs a URL and returns the parsed JSON value.

use std::time::Duration;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Blocking client for JSON GET calls.
pub struct Client {
    agent: ureq::Agent,
}

/// Failure for a JSON GET call.
#[derive(Debug)]
pub struct Error {
    /// Human-readable reason; includes the URL and status where known.
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

fn fail(message: String) -> Error {
    Error { message }
}

impl Client {
    /// Build a client with the default timeout.
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(DEFAULT_TIMEOUT))
            .timeout_connect(Some(DEFAULT_TIMEOUT))
            .user_agent(concat!("grok-models/", env!("CARGO_PKG_VERSION")))
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// GET `url` and parse the body as JSON.
    pub fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T, Error> {
        self.get_json_with_key(url, None)
    }

    /// GET `url` with an optional bearer key and parse the body as JSON.
    pub fn get_json_with_key<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        api_key: Option<&str>,
    ) -> Result<T, Error> {
        let mut request = self.agent.get(url).header("Accept", "application/json");
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        match request.call() {
            Ok(mut resp) => {
                let status = resp.status().as_u16();
                let text = match resp.body_mut().read_to_string() {
                    Ok(text) => text,
                    Err(err) => {
                        return Err(fail(format!("HTTP failure fetching {url}: {err}")));
                    }
                };
                if !(200..300).contains(&status) {
                    let body: String = text.chars().take(300).collect();
                    return Err(fail(format!("HTTP {status} fetching {url}: {body}")));
                }
                serde_json::from_str::<T>(&text)
                    .map_err(|err| fail(format!("invalid JSON from {url}: {err}")))
            }
            Err(err) => match err {
                ureq::Error::Timeout(_) => Err(fail(format!("HTTP timeout fetching {url}"))),
                _ => Err(fail(format!("HTTP failure fetching {url}: {err}"))),
            },
        }
    }
}
