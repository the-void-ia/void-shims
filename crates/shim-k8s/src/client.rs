//! Blocking HTTP client for the voidbox daemon.

use serde::{Deserialize, Serialize};
use shim_core::ShimError;
use std::time::Duration;

/// HTTP client for talking to a voidbox daemon over a bearer-authed TCP listener.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    base_url: String,
    token: String,
    http: reqwest::blocking::Client,
}

#[derive(Serialize)]
struct SendMessageBody<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize, Debug)]
pub struct RunSummary {
    pub run_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub state: serde_json::Value,
}

#[derive(Deserialize, Debug)]
pub struct ListRunsResponse {
    pub runs: Vec<RunSummary>,
}

impl DaemonClient {
    /// Builds a client with the given base URL, bearer token, and per-request timeout.
    pub fn new(
        base_url: impl Into<String>,
        token: impl Into<String>,
        timeout_secs: u64,
    ) -> Result<Self, ShimError> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            // Force HTTP/1.1 and disable connection pooling. kubectl
            // port-forward proxies HTTP/1.1 reliably but seems to drop the
            // first HTTP/2 attempt; disabling pool also avoids reusing a
            // half-closed connection across requests.
            .http1_only()
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| ShimError::DaemonHttp(format!("build http client: {e}")))?;
        Ok(Self {
            base_url: base_url.into(),
            token: token.into(),
            http,
        })
    }

    /// Fetches `GET /v1/runs` and returns the single `run_id` from the response.
    ///
    /// Returns [`ShimError::DaemonRunNotFound`] if the daemon reports zero or
    /// more than one run (Sandbox renders assume one run per daemon).
    pub fn first_run_id(&self) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs", self.base_url);
        // First request through a fresh kubectl port-forward sometimes fails
        // with "error sending request" (proxy not fully wired). Retry 5x.
        let mut last_err: Option<reqwest::Error> = None;
        let mut resp = None;
        for _ in 0..5 {
            match self.http.get(&url).bearer_auth(&self.token).send() {
                Ok(r) => {
                    resp = Some(r);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
        let resp = match resp {
            Some(r) => r,
            None => {
                let e = last_err.expect("at least one attempt was made");
                return Err(ShimError::DaemonHttp(format!("GET {url}: {e}")));
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(ShimError::DaemonHttp(format!(
                "GET {url}: {status}: {body}"
            )));
        }
        let text = resp
            .text()
            .map_err(|e| ShimError::DaemonHttp(format!("read body: {e}")))?;
        // The daemon may return either `{"runs": [...]}` or a bare array; try both.
        let runs: Vec<RunSummary> = match serde_json::from_str::<ListRunsResponse>(&text) {
            Ok(r) => r.runs,
            Err(_) => serde_json::from_str::<Vec<RunSummary>>(&text).map_err(|e| {
                let preview: String = text.chars().take(200).collect();
                ShimError::DaemonHttp(format!("parse runs list: {e}; body: {preview}"))
            })?,
        };
        match runs.len() {
            1 => Ok(runs.into_iter().next().unwrap().run_id),
            run_count => Err(ShimError::DaemonRunNotFound(run_count)),
        }
    }

    /// POSTs a message to a running service-mode agent.
    pub fn send_message(
        &self,
        run_id: &str,
        role: &str,
        content: &str,
    ) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs/{}/messages", self.base_url, run_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .json(&SendMessageBody { role, content })
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("POST {url}: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(ShimError::DaemonHttp(format!(
                "POST {url}: {status}: {body}"
            )));
        }
        Ok(body)
    }

    /// POSTs a cancel request to a running service-mode agent.
    pub fn cancel(&self, run_id: &str) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs/{}/cancel", self.base_url, run_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.token)
            .body("")
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("POST {url}: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(ShimError::DaemonHttp(format!(
                "POST {url}: {status}: {body}"
            )));
        }
        Ok(body)
    }

    /// Fetches a telemetry snapshot for the given run.
    pub fn telemetry(&self, run_id: &str) -> Result<String, ShimError> {
        let url = format!("{}/v1/runs/{}/telemetry", self.base_url, run_id);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| ShimError::DaemonHttp(format!("GET {url}: {e}")))?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if !status.is_success() {
            return Err(ShimError::DaemonHttp(format!(
                "GET {url}: {status}: {body}"
            )));
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_builds_with_valid_url() {
        let c = DaemonClient::new("http://127.0.0.1:12345", "tok", 30).unwrap();
        assert_eq!(c.base_url, "http://127.0.0.1:12345");
        assert_eq!(c.token, "tok");
    }

    // Live HTTP behavior is covered end-to-end by scripts/kind_smoke.sh
    // (mode: service path). No mock-server tests here to keep dev-deps small.
}
