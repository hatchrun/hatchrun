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
    ///
    /// # Errors
    ///
    /// The job token has expired already, so the runtime token the
    /// steps' toolkits would get is no good; the job fails with the
    /// message.
    pub(super) async fn start_cache(
        &mut self,
        sink: &mut dyn Sink,
    ) -> Result<Option<Proxy>, String> {
        if self.doc.cache.is_none() && self.doc.artifacts.is_none() {
            return Ok(None);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        if self.doc.token_expires_at <= now {
            return Err(format!(
                "The job token expired at {} (Unix time) and it is {now}: the control \
                 plane sent a document the job cannot run with.",
                self.doc.token_expires_at
            ));
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
                expires_at: self.doc.token_expires_at,
            },
        };
        let (ip, mut loopback_reason) = self.proxy_bind().await;
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut bound = ip;
        let started = match Proxy::start_services(services(ip != loopback), ip).await {
            // The bridge gateway can be an address this machine does not
            // have yet; the loopback still serves the host's steps.
            Err(err) if ip != loopback => {
                // `warn_loopback` logs it, once.
                loopback_reason = Some(format!("it cannot listen on {ip}: {err:#}"));
                bound = loopback;
                Proxy::start_services(services(false), loopback).await
            }
            other => other,
        };
        let proxy = match started {
            Ok(proxy) => proxy,
            Err(err) => {
                self.cache_unavailable(sink, &err.context(format!("cannot listen on {bound}")));
                return Ok(None);
            }
        };
        if let Err(err) = proxy.check().await {
            let at = format!("health check of {}", proxy.url());
            self.cache_unavailable(sink, &err.context(at));
            proxy.stop().await;
            return Ok(None);
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
        Ok(Some(proxy))
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
        // No `docker` command, or a configured one that is not there: no
        // Docker on this machine, so nothing to warn about, and no
        // containers either.
        if !self.docker_bin().is_ok_and(|bin| bin.is_file()) {
            return (loopback, None);
        }
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
            // A stop request ends the probe, and the job with it.
            Err(err) if err == super::docker::CANCELED => (loopback, None),
            // Docker is there and did not answer: a timeout or an
            // unusable client environment.
            Err(err) => (
                loopback,
                Some(format!("docker network inspect bridge failed: {err}")),
            ),
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
        // The log gets the masked text; the sink masks its own.
        warn!("{}", self.masked(&message));
        self.say(sink, Line::Warning(&message));
    }

    /// `text` as the agent's own log may show it.
    fn masked(&self, text: &str) -> String {
        self.masker.mask(text).into_owned()
    }

    /// Logs once and tells the job that it runs without the cache.
    fn cache_unavailable(&mut self, sink: &mut dyn Sink, err: &anyhow::Error) {
        warn!("cache proxy: {}", self.masked(&format!("{err:#}")));
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
