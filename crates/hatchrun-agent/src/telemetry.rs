// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Reports the instance's metrics to hatchrun-telemetry while the runner
//! runs, for launches whose boot document says where.
//!
//! The [`Sampler`] closes a window every ten seconds. Windows wait in a
//! buffer and go out once a minute, and once more, briefly, when the
//! runner exits. A report that fails stays in the buffer for the next
//! one; a full buffer drops its oldest windows. Nothing here fails or
//! holds up the job: errors are logged and the windows move on.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, warn};

use crate::broker::https_url;
use crate::metrics::{Sampler, Window};
use crate::source::Telemetry;

/// Windows kept while reports fail: an hour.
const BUFFER: usize = 360;
/// Windows in one report. hatchrun-telemetry takes at most 60.
const BATCH: usize = 60;
/// How often windows go out.
const SEND_EVERY: Duration = Duration::from_secs(60);
/// How long the last report may take once the runner exited.
const FINAL_TIMEOUT: Duration = Duration::from_secs(3);

/// What a report says about the instance it ran on.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Machine {
    /// The EC2 instance type, such as `m7i.large`.
    pub instance_type: String,
    /// `x86_64` or `aarch64`.
    pub arch: &'static str,
    /// The agent's version.
    pub agent_version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Report<'a> {
    #[serde(flatten)]
    machine: &'a Machine,
    windows: &'a [Window],
}

/// Samples and reports for one runner.
pub struct Reporter {
    http: Client,
    url: Url,
    token: String,
    machine: Machine,
    sampler: Sampler,
    buffer: VecDeque<Window>,
}

impl Reporter {
    /// A reporter for the boot document's telemetry.
    pub fn new(telemetry: Telemetry, machine: Machine, sampler: Sampler) -> Result<Self> {
        let mut url = https_url(&telemetry.url, "telemetry")?;
        url.path_segments_mut()
            .map_err(|()| anyhow::anyhow!("telemetry URL {:?} cannot have a path", telemetry.url))?
            .pop_if_empty()
            .extend(["v1", "metrics"]);
        let http = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()
            .context("build telemetry client")?;
        Ok(Self {
            http,
            url,
            token: telemetry.token,
            machine,
            sampler,
            buffer: VecDeque::with_capacity(BUFFER),
        })
    }

    /// Samples every second and reports every minute, until dropped.
    pub async fn collect(&mut self) -> Infallible {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut sent = Instant::now();
        loop {
            tick.tick().await;
            if let Some(window) = self.sampler.sample() {
                self.keep(window);
            }
            if sent.elapsed() >= SEND_EVERY {
                self.send_all().await;
                sent = Instant::now();
            }
        }
    }

    /// Closes the open window and reports what is left, within
    /// [`FINAL_TIMEOUT`].
    pub async fn finish(mut self) {
        if let Some(window) = self.sampler.finish() {
            self.keep(window);
        }
        if tokio::time::timeout(FINAL_TIMEOUT, self.send_all())
            .await
            .is_err()
        {
            warn!(windows = self.buffer.len(), "last metrics report timed out");
        }
    }

    fn keep(&mut self, window: Window) {
        if self.buffer.len() == BUFFER {
            self.buffer.pop_front();
        }
        self.buffer.push_back(window);
    }

    /// Reports the buffer in batches until it is empty or a report
    /// fails.
    async fn send_all(&mut self) {
        while !self.buffer.is_empty() {
            if !self.send_batch().await {
                return;
            }
        }
    }

    /// Reports the oldest windows. Returns whether they left the buffer.
    async fn send_batch(&mut self) -> bool {
        let count = self.buffer.len().min(BATCH);
        let windows: Vec<Window> = self.buffer.iter().take(count).cloned().collect();
        let report = Report {
            machine: &self.machine,
            windows: &windows,
        };
        let response = self
            .http
            .post(self.url.clone())
            .bearer_auth(&self.token)
            .json(&report)
            .send()
            .await;
        let status = match response {
            Ok(response) => response.status(),
            Err(err) => {
                debug!("metrics report failed: {err}");
                return false;
            }
        };
        if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
            debug!(%status, "metrics report refused for now");
            return false;
        }
        if !status.is_success() {
            // Sending the same windows again gets the same answer.
            warn!(%status, windows = count, "metrics report refused, dropping it");
        }
        self.buffer.drain(..count);
        true
    }
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn window(at: u64) -> Window {
        Window {
            at,
            seconds: 10,
            cpu_min: 1.0,
            cpu_avg: 2.5,
            cpu_max: 4.0,
            cpu_steal: 0.0,
            mem_max: 2048,
            mem_total: 8192,
            cgroup_peak: 1024,
            oom_kills: 0,
            disk_used: 4096,
            net_rx: 10,
            net_tx: 20,
        }
    }

    fn new_reporter(url: &str) -> Reporter {
        Reporter::new(
            Telemetry {
                url: url.into(),
                token: "a.b.c".into(),
            },
            Machine {
                instance_type: "m7i.large".into(),
                arch: "x86_64",
                agent_version: "1.2.3",
            },
            Sampler::new("/nonexistent"),
        )
        .unwrap()
    }

    async fn telemetry(status: u16) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/metrics"))
            .and(header("authorization", "Bearer a.b.c"))
            .respond_with(ResponseTemplate::new(status))
            .mount(&server)
            .await;
        server
    }

    #[test]
    fn url_must_be_https() {
        assert_eq!(
            new_reporter("https://telemetry.example.com/").url.as_str(),
            "https://telemetry.example.com/v1/metrics"
        );
        let refused = Reporter::new(
            Telemetry {
                url: "http://telemetry.example.com".into(),
                token: "t".into(),
            },
            new_reporter("https://x.example").machine,
            Sampler::new("/nonexistent"),
        );
        assert!(refused.is_err());
    }

    #[tokio::test]
    async fn reports_in_batches() {
        let server = telemetry(204).await;
        let mut reporter = new_reporter(&server.uri());
        for at in 0..(BATCH as u64 + 5) {
            reporter.keep(window(at));
        }
        reporter.send_all().await;
        assert!(reporter.buffer.is_empty());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let first: serde_json::Value = requests[0].body_json().unwrap();
        assert_eq!(first["instanceType"], "m7i.large");
        assert_eq!(first["arch"], "x86_64");
        assert_eq!(first["agentVersion"], "1.2.3");
        assert_eq!(first["windows"].as_array().unwrap().len(), BATCH);
        assert_eq!(
            first["windows"][0],
            serde_json::json!({
                "at": 0, "seconds": 10, "cpuMin": 1.0, "cpuAvg": 2.5, "cpuMax": 4.0,
                "cpuSteal": 0.0, "memMax": 2048, "memTotal": 8192, "cgroupPeak": 1024,
                "oomKills": 0, "diskUsed": 4096, "netRx": 10, "netTx": 20,
            })
        );
        let second: serde_json::Value = requests[1].body_json().unwrap();
        assert_eq!(second["windows"].as_array().unwrap().len(), 5);
    }

    #[tokio::test]
    async fn keeps_windows_while_the_service_is_down() {
        let server = telemetry(503).await;
        let mut reporter = new_reporter(&server.uri());
        reporter.keep(window(1));
        reporter.send_all().await;
        assert_eq!(reporter.buffer.len(), 1);

        // Nothing listens on port 9 of loopback.
        let mut reporter = new_reporter("http://127.0.0.1:9");
        reporter.keep(window(1));
        reporter.send_all().await;
        assert_eq!(reporter.buffer.len(), 1);
    }

    #[tokio::test]
    async fn drops_refused_reports() {
        let server = telemetry(400).await;
        let mut reporter = new_reporter(&server.uri());
        reporter.keep(window(1));
        reporter.send_all().await;
        assert!(reporter.buffer.is_empty());
    }

    #[test]
    fn a_full_buffer_drops_the_oldest() {
        let mut reporter = new_reporter("https://telemetry.example.com");
        for at in 0..(BUFFER as u64 + 2) {
            reporter.keep(window(at));
        }
        assert_eq!(reporter.buffer.len(), BUFFER);
        assert_eq!(reporter.buffer.front().unwrap().at, 2);
    }

    #[tokio::test]
    async fn the_last_report_gives_up_in_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_secs(30)))
            .mount(&server)
            .await;
        let mut reporter = new_reporter(&server.uri());
        reporter.keep(window(1));
        let started = std::time::Instant::now();
        reporter.finish().await;
        assert!(started.elapsed() < FINAL_TIMEOUT + Duration::from_secs(2));
    }
}
