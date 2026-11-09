// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The start and the end of a job: its `if`, the cache proxy, stop
//! requests and the outputs it reports.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Instant;

use hatchrun_workflow::{
    Conclusion, ExprError, JobResult, JobStatus, StepSummary, valid_environment_url,
};
use tracing::warn;

use super::context::Runtime;
use super::sink::Sink;
use super::{GRACE_MINUTES, Job, Line, interp};
use crate::cache::{ArtifactConfig, Identity, Proxy, Services};
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
        });
        for token in cache
            .iter()
            .chain(artifacts.iter().map(|config| &config.service))
            .map(|service| service.token.clone())
            .collect::<Vec<_>>()
        {
            self.masker.add(&token);
        }
        // The cache scopes go into the runtime token for `BuildKit`. A job
        // with no cache has none, and a token without them makes
        // `BuildKit` refuse it.
        let scopes = self
            .doc
            .cache
            .as_ref()
            .map(|service| service.scopes.clone());
        let lifetime_secs = self.doc.token_lifetime_secs;
        // `BuildKit` runs in a container, which reaches the proxy only
        // when it listens on the bridge: on the loopback the token
        // leaves the scopes out, and `type=gha` fails at once.
        let services = |reachable: bool| Services {
            cache: cache.clone(),
            artifacts: artifacts.clone(),
            identity: Identity {
                run_id: self.doc.run_id.clone(),
                job_id: self.doc.job.id.clone(),
                scopes: scopes.clone().filter(|_| reachable),
                lifetime_secs,
            },
        };
        let (ip, mut loopback_reason) = self.proxy_bind().await;
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut bound = ip;
        let started = match Proxy::start_services(services(ip != loopback), ip).await {
            // The bridge gateway can be an address this machine does not
            // have yet; the loopback still serves the host's steps.
            Err(err) if ip != loopback => {
                let text = self.masker.mask(&format!("{err:#}")).into_owned();
                warn!(%ip, "cache proxy: cannot listen, trying the loopback: {text}");
                loopback_reason = Some(format!("it cannot listen on {ip}: {text}"));
                bound = loopback;
                Proxy::start_services(services(false), loopback).await
            }
            other => other,
        };
        let proxy = match started {
            Ok(proxy) => proxy,
            Err(err) => {
                let text = self.masker.mask(&format!("{err:#}")).into_owned();
                warn!(addr = %bound, "cache proxy: cannot listen: {text}");
                self.cache_unavailable(sink, &err);
                return None;
            }
        };
        if let Err(err) = proxy.check().await {
            let text = self.masker.mask(&format!("{err:#}")).into_owned();
            warn!(
                url = proxy.url(),
                "cache proxy: health check failed: {text}"
            );
            self.cache_unavailable(sink, &err);
            proxy.stop().await;
            return None;
        }
        self.cache_unreachable = bound == loopback;
        if let Some(reason) = loopback_reason {
            self.warn_loopback(sink, &reason);
        }
        let token = proxy.runtime_token().to_owned();
        self.masker.add(&token);
        // The results URL and token serve the artifact toolkit and
        // `BuildKit` alike. Only a job with a cache gets the cache
        // variables: `docker buildx` with `type=gha` reads the v2 flag
        // and the v1 URL too, when it checks that a cache is configured.
        self.cache_env = BTreeMap::from([
            ("ACTIONS_RESULTS_URL".to_owned(), proxy.url().to_owned()),
            ("ACTIONS_RUNTIME_TOKEN".to_owned(), token),
        ]);
        if self.doc.cache.is_some() {
            self.cache_env
                .insert("ACTIONS_CACHE_URL".to_owned(), format!("{}/", proxy.url()));
            self.cache_env
                .insert("ACTIONS_CACHE_SERVICE_V2".to_owned(), "true".to_owned());
        }
        Some(proxy)
    }

    /// Where the proxy listens: the address of Docker's default bridge
    /// when Docker answers, so a `BuildKit` container (which cannot reach
    /// the machine's loopback) and job containers can use it, and the
    /// loopback otherwise, with the reason Docker gave no address when it
    /// is there and answered wrongly (none when it is absent). Only
    /// the host and the containers on its bridges can connect.
    async fn proxy_bind(&mut self) -> (IpAddr, Option<String>) {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let args = [
            "network".to_owned(),
            "inspect".to_owned(),
            "bridge".to_owned(),
            "--format".to_owned(),
            "{{(index .IPAM.Config 0).Gateway}}".to_owned(),
        ];
        match self.docker_capture(&args, None).await {
            Ok(out) if out.code == 0 => match out.stdout.trim().parse() {
                Ok(ip) => (ip, None),
                Err(_) => (
                    loopback,
                    Some(format!(
                        "docker network inspect bridge printed no gateway address, but {:?}",
                        out.stdout.trim()
                    )),
                ),
            },
            Ok(out) => (
                loopback,
                Some(format!(
                    "docker network inspect bridge exited with {}",
                    out.code
                )),
            ),
            // No Docker on this machine: nothing to warn about, there are
            // no containers either.
            Err(_) => (loopback, None),
        }
    }

    /// Says that the proxy listens on the loopback only, and what that
    /// costs.
    fn warn_loopback(&mut self, sink: &mut dyn Sink, reason: &str) {
        let message = format!(
            "The cache proxy listens on the loopback only ({reason}): steps in containers \
             and BuildKit builders (docker buildx with type=gha) cannot use the cache or \
             the artifact service. Steps on the host still can."
        );
        let message = self.masker.mask(&message).into_owned();
        warn!("{message}");
        self.say(sink, Line::Warning(&message));
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

    /// Evaluates the job's `environment-url` when the job succeeded. An
    /// address that fails, holds a secret or is not an `http` or `https`
    /// link is left out with a warning.
    pub(super) fn evaluate_environment_url(&mut self, sink: &mut dyn Sink) {
        let Some(template) = &self.doc.job.environment_url else {
            return;
        };
        if self.status != JobStatus::Success {
            return;
        }
        let url = match interp(template, &self.ctx, &self.layout.workspace, self.status) {
            Ok(url) => url.trim().to_owned(),
            Err(err) => {
                let message = format!("Failed to evaluate environment-url: {err}");
                self.say(sink, Line::Warning(&message));
                return;
            }
        };
        if self.masker.mask(&url) != url {
            let message = "Skip environment-url since it may contain secret.";
            self.say(sink, Line::Warning(message));
        } else if !valid_environment_url(&url) {
            let message = format!("Skip environment-url '{url}': it is not an http or https link.");
            self.say(sink, Line::Warning(&message));
        } else {
            self.environment_url = Some(url);
        }
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
            environment_url: self
                .environment_url
                .take()
                .filter(|_| conclusion == Conclusion::Success),
        }
    }
}
