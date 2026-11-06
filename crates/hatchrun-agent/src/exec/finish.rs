// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The start and the end of a job: its `if`, the cache proxy, stop
//! requests and the outputs it reports.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use hatchrun_workflow::{Conclusion, ExprError, JobResult, JobStatus, StepSummary};
use tracing::warn;

use super::context::Runtime;
use super::sink::Sink;
use super::{GRACE_MINUTES, Job, Line, interp};
use crate::cache::{ArtifactConfig, Proxy, Services};
use crate::source;

/// Most outputs a job reports.
const MAX_OUTPUTS: usize = 1000;
/// Most bytes of output names and values a job reports.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

impl Job<'_> {
    /// Whether the job's `if` lets it run. The job's status for
    /// `success()` is that of the jobs it needs: one that did not
    /// succeed stops the job unless its `if` calls a status function.
    pub(super) fn job_condition(&self) -> Result<bool, ExprError> {
        let status = if self
            .doc
            .needs
            .values()
            .all(|need| need.result == Conclusion::Success)
        {
            JobStatus::Success
        } else {
            JobStatus::Failure
        };
        let rt = Runtime {
            workspace: &self.layout.workspace,
            status,
            cancel: self.ctx.cancel(),
        };
        super::context::condition(self.doc.job.if_.as_deref(), &self.ctx, &rt)
    }

    /// Starts the cache proxy when the document has a cache or an
    /// artifact service and points the steps at it. Without one, or
    /// when it does not come up, steps run with neither.
    pub(super) async fn start_cache(&mut self, sink: &mut dyn Sink) -> Option<Proxy> {
        if self.doc.cache.is_none() && self.doc.artifacts.is_none() {
            return None;
        }
        // The service tokens are the agent's own credentials; an error
        // text that quotes one must not print.
        let cache = self.doc.cache.as_ref().map(|service| source::Cache {
            url: service.url.clone(),
            token: service.token.clone(),
        });
        let artifacts = self.doc.artifacts.as_ref().map(|service| ArtifactConfig {
            service: source::Cache {
                url: service.url.clone(),
                token: service.token.clone(),
            },
            run_id: self.doc.run_id.clone(),
            job_id: self.doc.job.id.clone(),
        });
        for token in cache
            .iter()
            .chain(artifacts.iter().map(|config| &config.service))
            .map(|service| service.token.clone())
            .collect::<Vec<_>>()
        {
            self.masker.add(&token);
        }
        let services = Services { cache, artifacts };
        let ip = self.proxy_bind().await;
        let proxy = match Proxy::start_services(services, ip).await {
            Ok(proxy) => proxy,
            Err(err) => {
                self.cache_unavailable(sink, &err);
                return None;
            }
        };
        if let Err(err) = proxy.check().await {
            self.cache_unavailable(sink, &err);
            proxy.stop().await;
            return None;
        }
        let token = match proxy.runtime_token().map(str::to_owned) {
            Some(token) => token,
            None => match super::random_name() {
                Ok(token) => token,
                Err(message) => {
                    self.cache_unavailable(sink, &anyhow::anyhow!(message));
                    proxy.stop().await;
                    return None;
                }
            },
        };
        self.masker.add(&token);
        // `docker buildx` with `type=gha` reads these three, and the
        // v1 URL too, when it checks that a cache is configured.
        self.cache_env = BTreeMap::from([
            ("ACTIONS_CACHE_URL".to_owned(), format!("{}/", proxy.url())),
            ("ACTIONS_RESULTS_URL".to_owned(), proxy.url().to_owned()),
            ("ACTIONS_CACHE_SERVICE_V2".to_owned(), "true".to_owned()),
            ("ACTIONS_RUNTIME_TOKEN".to_owned(), token),
        ]);
        Some(proxy)
    }

    /// Where the proxy listens: the address of Docker's default bridge
    /// when Docker answers, so a `BuildKit` container (which cannot reach
    /// the machine's loopback) and job containers can use it, and the
    /// loopback otherwise. Only the host and the containers on its
    /// bridges can connect.
    async fn proxy_bind(&mut self) -> IpAddr {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let args = [
            "network".to_owned(),
            "inspect".to_owned(),
            "bridge".to_owned(),
            "--format".to_owned(),
            "{{(index .IPAM.Config 0).Gateway}}".to_owned(),
        ];
        match self.docker_capture(&args, None).await {
            Ok(out) if out.code == 0 => out.stdout.trim().parse().unwrap_or(loopback),
            _ => loopback,
        }
    }

    fn cache_unavailable(&mut self, sink: &mut dyn Sink, err: &anyhow::Error) {
        let text = self.masker.mask(&format!("{err:#}")).into_owned();
        warn!("cache proxy: {text}");
        let message = format!("The cache service is not available, running without it: {err:#}");
        self.say(sink, Line::Warning(&message));
    }

    /// Notes a request to stop that came while no process ran: from
    /// now on the job is cancelled and has the grace period for what
    /// still runs.
    pub(super) fn note_cancel(&mut self) {
        let requests = *self.cancel_requests.borrow();
        if requests >= 2 {
            self.aborted = true;
            self.background.kill();
        }
        if requests >= 1 && self.grace_until.is_none() {
            self.grace_until = Some(Instant::now() + self.opts.minute * GRACE_MINUTES);
            self.set_status(JobStatus::Cancelled);
        }
    }

    /// The job's outputs, evaluated and masked. An output that would
    /// print a secret is left out, as GitHub does.
    pub(super) fn job_outputs(&mut self, sink: &mut dyn Sink) -> BTreeMap<String, String> {
        let mut outputs = BTreeMap::new();
        let mut bytes = 0usize;
        for (name, template) in &self.doc.job.outputs {
            let value = match interp(template, &self.ctx, &self.layout.workspace, self.status) {
                Ok(value) => value,
                Err(err) => {
                    let message = format!("Failed to evaluate output '{name}': {err}");
                    self.say(sink, Line::Warning(&message));
                    continue;
                }
            };
            if self.masker.mask(&value) != value {
                let message = format!("Skip output '{name}' since it may contain secret.");
                self.say(sink, Line::Warning(&message));
                continue;
            }
            let size = name.len() + value.len();
            if outputs.len() >= MAX_OUTPUTS || bytes + size > MAX_OUTPUT_BYTES {
                let message = format!(
                    "Skip output '{name}': a job reports at most {MAX_OUTPUTS} outputs and 1 MiB of them."
                );
                self.say(sink, Line::Warning(&message));
                continue;
            }
            bytes += size;
            outputs.insert(name.clone(), value);
        }
        outputs
    }

    /// The result to report.
    pub(super) fn result(
        &mut self,
        conclusion: Conclusion,
        outputs: BTreeMap<String, String>,
        steps: Vec<StepSummary>,
    ) -> JobResult {
        JobResult {
            conclusion,
            outputs,
            steps,
            annotations: std::mem::take(&mut self.annotations),
        }
    }
}
