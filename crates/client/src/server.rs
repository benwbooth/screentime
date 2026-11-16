//! HTTP client for the screentime server's client API.

use screentime_common::{AccountsReply, AccountsReport, Policy, TimeRequest, UsageReport};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use std::time::Duration;

pub struct ServerApi {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl ServerApi {
    pub fn new(
        base: String,
        token: Option<String>,
        ca_file: Option<&Path>,
        fingerprint: Option<[u8; 32]>,
    ) -> Result<ServerApi> {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
        if let Some(pin) = fingerprint {
            builder = builder.use_preconfigured_tls(crate::pinning::pinned_config(pin));
        } else if let Some(path) = ca_file {
            let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(&pem)?);
        }
        let http = builder.build()?;
        Ok(ServerApi { http, base, token })
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// The user's policy, and the server's clock from the reply's Date header.
    pub async fn policy(&self, user: &str) -> Result<(Policy, Option<DateTime<Utc>>)> {
        let url = format!("{}/api/client/policy/{user}", self.base);
        let resp = self.auth(self.http.get(&url)).send().await?.error_for_status()?;
        let server_time = resp
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|d| d.to_str().ok())
            .and_then(|d| DateTime::parse_from_rfc2822(d).ok())
            .map(|d| d.with_timezone(&Utc));
        Ok((resp.json().await.context("decoding policy")?, server_time))
    }

    pub async fn accounts(&self, report: &AccountsReport) -> Result<AccountsReply> {
        let url = format!("{}/api/client/accounts", self.base);
        let resp = self.auth(self.http.post(&url)).json(report).send().await?.error_for_status()?;
        resp.json().await.context("decoding account mapping")
    }

    pub async fn report_usage(&self, report: &UsageReport) -> Result<()> {
        let url = format!("{}/api/client/usage", self.base);
        self.auth(self.http.post(&url)).json(report).send().await?.error_for_status()?;
        Ok(())
    }

    /// Tell the server this computer has carried out quick lock `id` for `user`.
    pub async fn ack_quick_lock(&self, user: &str, id: i64) -> Result<()> {
        let url = format!("{}/api/client/quick-lock/{user}/{id}", self.base);
        self.auth(self.http.post(&url)).send().await?.error_for_status()?;
        Ok(())
    }

    pub async fn request_time(&self, request: &TimeRequest) -> Result<()> {
        let url = format!("{}/api/client/requests", self.base);
        self.auth(self.http.post(&url)).json(request).send().await?.error_for_status()?;
        Ok(())
    }
}
