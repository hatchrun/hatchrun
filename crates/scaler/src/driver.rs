// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: Apache-2.0

//! Runs the scaler against GitHub and the cloud.

use std::future::Future;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use hatchrun_scaler_core::{Action, Scaler};
use hatchrun_scaleset::{
    Actions, CallError, Error, ErrorKind, Message, NewScaleSet, Operation, ScaleSet, Session,
    Transport,
};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::broker::{ConfigStore, new_ticket, user_data};
use crate::ec2::Cloud;
use crate::github::GitHub;

/// Launches started at once.
const LAUNCH_CONCURRENCY: usize = 8;
/// Failed polls in a row before the scaler gives up and exits.
const MAX_FAILURES: u32 = 10;
/// Attempts at opening a session while an old one is still open.
const SESSION_ATTEMPTS: u32 = 10;
/// Runner work directory, relative to the runner's home.
const WORK_FOLDER: &str = "_work";

/// Returns the time in seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Finds the scale set, creating it when missing and updating its labels
/// and settings when present.
pub async fn ensure_scale_set<T>(
    github: &GitHub<T>,
    actions: &Actions,
    runner_group: &str,
    mut scale_set: NewScaleSet,
) -> Result<ScaleSet>
where
    T: Transport,
    T::Error: std::error::Error + Send + Sync + 'static,
{
    let group = github
        .call(actions.runner_group(runner_group))
        .await
        .with_context(|| format!("look up runner group {runner_group}"))?
        .with_context(|| format!("runner group {runner_group} does not exist"))?;
    scale_set.runner_group_id = group.id;

    let existing = github
        .call(actions.scale_set(group.id, &scale_set.name))
        .await
        .with_context(|| format!("look up scale set {}", scale_set.name))?;
    let result = match existing {
        Some(existing) => {
            github
                .call(actions.update_scale_set(existing.id, &scale_set))
                .await
        }
        None => github.call(actions.create_scale_set(&scale_set)).await,
    };
    let scale_set = result.with_context(|| format!("save scale set {}", scale_set.name))?;
    info!(id = scale_set.id, name = scale_set.name, "scale set ready");
    Ok(scale_set)
}

/// Owns the session of one scale set and carries out the scaler's
/// decisions.
pub struct Driver<T, C, S> {
    github: Rc<GitHub<T>>,
    cloud: C,
    store: S,
    scaler: Scaler,
    scale_set_id: u64,
    actions: Actions,
    session: Session,
    last_message_id: u64,
    clock: fn() -> u64,
}

impl<T, C, S> Driver<T, C, S>
where
    T: Transport + 'static,
    T::Error: std::error::Error + Send + Sync + 'static,
    C: Cloud,
    S: ConfigStore,
{
    /// Opens a message session. A session left open by a scaler that
    /// crashed blocks a new one until the service expires it, so conflicts
    /// are retried for a few minutes.
    pub async fn start(
        github: GitHub<T>,
        actions: Actions,
        cloud: C,
        store: S,
        mut scaler: Scaler,
        scale_set_id: u64,
        owner: &str,
    ) -> Result<Self> {
        let mut attempt = 1;
        let session = loop {
            match github
                .call(actions.create_session(scale_set_id, owner))
                .await
            {
                Ok(session) => break session,
                Err(err) if attempt < SESSION_ATTEMPTS && is_kind(&err, ErrorKind::Conflict) => {
                    warn!(
                        attempt,
                        "another session is open on the scale set, retrying: {err}"
                    );
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                Err(err) => return Err(err).context("open message session"),
            }
        };
        let statistics = session.statistics.unwrap_or_default();
        info!(
            session = session.id,
            assigned_jobs = statistics.total_assigned_jobs,
            "message session open"
        );
        scaler.handle_jobs([], Some(statistics.total_assigned_jobs), now());
        Ok(Self {
            github: Rc::new(github),
            cloud,
            store,
            scaler,
            scale_set_id,
            actions,
            session,
            last_message_id: 0,
            clock: now,
        })
    }

    /// Polls for messages and compares runners with instances every
    /// `reconcile_every` until `shutdown` resolves, then closes the
    /// session. Instances keep running their jobs; the next scaler adopts
    /// them.
    pub async fn run(
        mut self,
        shutdown: impl Future<Output = ()>,
        reconcile_every: Duration,
    ) -> Result<()> {
        tokio::pin!(shutdown);
        if let Err(err) = self.reconcile().await {
            // An open session blocks the next scaler for minutes.
            self.close().await;
            return Err(err);
        }

        let mut tick = tokio::time::interval_at(Instant::now() + reconcile_every, reconcile_every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut poll = Box::pin(self.poll());
        let mut failures = 0;

        let result = loop {
            tokio::select! {
                () = &mut shutdown => break Ok(()),
                _ = tick.tick() => {
                    if let Err(err) = self.reconcile().await {
                        warn!("reconcile: {err:#}");
                    }
                }
                result = &mut poll => {
                    let outcome = match result {
                        Ok(Some(message)) => self.handle(message).await,
                        Ok(None) => {
                            let actions = self.scaler.plan((self.clock)());
                            self.execute(actions).await
                        }
                        Err(err) if is_kind(&err, ErrorKind::Unauthorized) => {
                            self.refresh_session().await
                        }
                        Err(err) => Err(err).context("poll for messages"),
                    };
                    match outcome {
                        Ok(()) => failures = 0,
                        Err(err) => {
                            failures += 1;
                            if failures >= MAX_FAILURES {
                                break Err(err);
                            }
                            warn!(failures, "{err:#}");
                            tokio::time::sleep(Duration::from_secs(5 * u64::from(failures))).await;
                        }
                    }
                    poll = Box::pin(self.poll());
                }
            }
        };
        self.close().await;
        result
    }

    /// The poll owns everything it needs, so the driver stays free to
    /// reconcile while it waits.
    fn poll(&self) -> impl Future<Output = Result<Option<Message>, CallError<T::Error>>> + 'static {
        let github = Rc::clone(&self.github);
        let operation = self
            .session
            .get_message(self.last_message_id, self.scaler.max_runners());
        async move { github.call(operation).await }
    }

    /// Acquires the message's available jobs, acts on it and deletes it.
    /// On error the message stays in the queue and comes back on the next
    /// poll.
    async fn handle(&mut self, message: Message) -> Result<()> {
        let now = (self.clock)();
        let ids = self.scaler.handle_message(&message, now);
        debug!(id = message.id, jobs = message.jobs.len(), "message");
        if !ids.is_empty() {
            let scale_set_id = self.scale_set_id;
            let acquired = self
                .with_session(|actions, session| actions.acquire_jobs(scale_set_id, session, &ids))
                .await
                .context("acquire jobs")?;
            info!(
                available = ids.len(),
                acquired = acquired.len(),
                "acquired jobs"
            );
        }
        let actions = self.scaler.plan(now);
        self.execute(actions).await?;

        let id = message.id;
        self.with_session(|_, session| session.delete_message(id))
            .await
            .context("delete message")?;
        self.last_message_id = id;
        Ok(())
    }

    /// Compares tracked runners with the cloud's instances.
    async fn reconcile(&mut self) -> Result<()> {
        let instances = self.cloud.instances().await?;
        let now = (self.clock)();
        let mut actions = self.scaler.reconcile(&instances, now);
        actions.extend(self.scaler.plan(now));
        debug!(
            instances = instances.len(),
            runners = self.scaler.runners().len(),
            "reconciled"
        );
        self.execute(actions).await
    }

    /// Carries out actions and the follow-ups they produce. Failures of
    /// single actions are logged and left for the scaler to retry.
    async fn execute(&mut self, mut actions: Vec<Action>) -> Result<()> {
        while !actions.is_empty() {
            self.renew().await?;
            let mut launches = Vec::new();
            let mut pool_launches = Vec::new();
            let mut next = Vec::new();
            for action in actions.drain(..) {
                match action {
                    Action::Launch { name, pooled } => launches.push((name, pooled)),
                    Action::LaunchPool { name } => pool_launches.push(name),
                    Action::Stop { instance_id } => {
                        if let Err(err) = self.cloud.stop(&instance_id).await {
                            warn!(instance_id, "{err:#}");
                        }
                    }
                    Action::RemoveRunner { name, runner_id } => {
                        next.extend(self.remove_runner(&name, runner_id).await);
                    }
                    Action::Terminate { instance_id } => {
                        if let Err(err) = self.cloud.terminate(&instance_id).await {
                            warn!(instance_id, "{err:#}");
                        }
                    }
                }
            }

            let this = &*self;
            let results: Vec<_> = futures_util::stream::iter(launches)
                .map(|(name, pooled)| async move {
                    let result = this.launch(&name, pooled.as_deref()).await;
                    (name, result)
                })
                .buffer_unordered(LAUNCH_CONCURRENCY)
                .collect()
                .await;
            let pooled: Vec<_> = futures_util::stream::iter(pool_launches)
                .map(|name| async move {
                    let result = this.cloud.launch_pool(&name).await;
                    (name, result)
                })
                .buffer_unordered(LAUNCH_CONCURRENCY)
                .collect()
                .await;
            let now = (self.clock)();
            for (name, result) in pooled {
                match result {
                    Ok(instance_id) => self.scaler.pool_launched(&name, instance_id, now),
                    Err(err) => {
                        warn!(name, "pool launch failed: {err:#}");
                        self.scaler.pool_launch_failed(&name, now);
                    }
                }
            }
            for (name, result) in results {
                match result {
                    Ok((runner_id, instance_id)) => {
                        self.scaler.launched(&name, runner_id, instance_id, now);
                    }
                    Err((runner_id, err)) => {
                        warn!(runner = name, "launch failed: {err:#}");
                        next.extend(self.scaler.launch_failed(&name, runner_id, now));
                    }
                }
            }
            actions = next;
        }
        Ok(())
    }

    /// Registers the runner, starts its instance with a ticket in user
    /// data and stores the config in the broker under that ticket. The
    /// instance is a new one, or the stopped pool instance `pooled`. On
    /// failure, returns the runner id when the registration had already
    /// happened.
    ///
    /// The config is stored only once the instance exists, so nothing can
    /// claim it before there is an instance to claim it.
    async fn launch(
        &self,
        name: &str,
        pooled: Option<&str>,
    ) -> Result<(u64, String), (Option<u64>, anyhow::Error)> {
        let config = self
            .github
            .call(
                self.actions
                    .generate_jit_config(self.scale_set_id, name, WORK_FOLDER),
            )
            .await
            .map_err(|err| (None, anyhow::Error::new(err).context("generate JIT config")))?;
        let runner_id = config.runner.id;
        let ticket = new_ticket().map_err(|err| (Some(runner_id), err))?;
        let user_data = user_data(self.store.url(), &ticket);
        let instance_id = match pooled {
            // On failure the scaler removes the runner, which terminates
            // the pool instance: it may already carry the runner's ticket.
            Some(instance_id) => {
                self.cloud
                    .start_pooled(instance_id, name, runner_id, &user_data)
                    .await
                    .map_err(|err| (Some(runner_id), err))?;
                instance_id.to_owned()
            }
            None => self
                .cloud
                .launch(name, runner_id, &user_data)
                .await
                .map_err(|err| (Some(runner_id), err))?,
        };
        if let Err(err) = self.store.store(&ticket, &config.encoded_jit_config).await {
            // Without its config the instance would sit out the agent's
            // timeout before powering off.
            if let Err(err) = self.cloud.terminate(&instance_id).await {
                warn!(instance_id, "{err:#}");
            }
            return Err((Some(runner_id), err));
        }
        Ok((runner_id, instance_id))
    }

    async fn remove_runner(&mut self, name: &str, runner_id: u64) -> Option<Action> {
        let result = self
            .github
            .call(self.actions.remove_runner(runner_id))
            .await;
        let now = (self.clock)();
        match result {
            Ok(()) => {
                info!(runner = name, "removed runner");
                self.scaler.runner_removed(name)
            }
            Err(err)
                if is_kind(&err, ErrorKind::RunnerNotFound)
                    || is_kind(&err, ErrorKind::NotFound) =>
            {
                self.scaler.runner_removed(name)
            }
            Err(err) if is_kind(&err, ErrorKind::JobStillRunning) => {
                self.scaler.remove_failed(name, true, now);
                None
            }
            Err(err) => {
                warn!(runner = name, "remove runner: {err}");
                self.scaler.remove_failed(name, false, now);
                None
            }
        }
    }

    /// Sends a call made with the session token, refreshing the session
    /// once when the token expired.
    async fn with_session<R>(
        &mut self,
        operation: impl Fn(&Actions, &Session) -> Result<Operation<R>, Error>,
    ) -> Result<R> {
        self.renew().await?;
        match self
            .github
            .call(operation(&self.actions, &self.session))
            .await
        {
            Err(err) if is_kind(&err, ErrorKind::Unauthorized) => {
                self.refresh_session().await?;
                Ok(self
                    .github
                    .call(operation(&self.actions, &self.session))
                    .await?)
            }
            result => Ok(result?),
        }
    }

    async fn refresh_session(&mut self) -> Result<()> {
        self.renew().await?;
        self.session = self
            .github
            .call(
                self.actions
                    .refresh_session(self.scale_set_id, &self.session),
            )
            .await
            .context("refresh message session")?;
        debug!("refreshed message session");
        Ok(())
    }

    /// Gets a new Actions service connection before the current one
    /// expires.
    async fn renew(&mut self) -> Result<()> {
        let now = (self.clock)();
        if self.actions.expires_soon(now) {
            self.actions = self.github.connect(now).await?;
        }
        Ok(())
    }

    async fn close(&mut self) {
        let result = match self.renew().await {
            Ok(()) => self
                .github
                .call(
                    self.actions
                        .delete_session(self.scale_set_id, &self.session),
                )
                .await
                .map_err(anyhow::Error::from),
            Err(err) => Err(err),
        };
        match result {
            Ok(()) => info!("message session closed"),
            Err(err) => warn!("close message session: {err:#}"),
        }
    }
}

fn is_kind<E>(err: &CallError<E>, kind: ErrorKind) -> bool {
    matches!(err, CallError::Api(err) if err.kind() == Some(kind))
}

/// Rejects settings the scaler cannot work with.
pub fn validate(min_runners: u32, max_runners: u32) -> Result<()> {
    if max_runners == 0 {
        bail!("--max-runners must be at least 1");
    }
    if min_runners > max_runners {
        bail!("--min-runners ({min_runners}) is above --max-runners ({max_runners})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use hatchrun_scaler_core::{Config, Instance, Phase};
    use hatchrun_scaleset::{ConfigUrl, Secret};
    use http::{Method, Request, Response};

    use super::*;
    use crate::github::Credentials;

    /// A JWT with `{"exp":1900000000}` as its payload.
    const ADMIN_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjE5MDAwMDAwMDB9.c2ln";
    const NOW: u64 = 1_000_000;

    type Route = (Method, &'static str, u16, String);

    /// Answers by method and path suffix and records every request.
    #[derive(Clone, Default)]
    struct FakeGitHub {
        routes: Rc<RefCell<Vec<Route>>>,
        sent: Rc<RefCell<Vec<(Method, String)>>>,
    }

    impl FakeGitHub {
        fn route(&self, method: Method, suffix: &'static str, status: u16, body: &str) {
            self.routes
                .borrow_mut()
                .push((method, suffix, status, body.to_owned()));
        }

        fn sent(&self) -> Vec<(Method, String)> {
            self.sent.borrow().clone()
        }
    }

    impl Transport for FakeGitHub {
        type Error = std::io::Error;

        fn send(
            &self,
            request: Request<Vec<u8>>,
        ) -> impl Future<Output = Result<Response<Vec<u8>>, Self::Error>> {
            let path = request.uri().path().to_owned();
            self.sent
                .borrow_mut()
                .push((request.method().clone(), path.clone()));
            let routes = self.routes.borrow();
            let route = routes
                .iter()
                .find(|(method, suffix, ..)| method == request.method() && path.ends_with(suffix));
            let response = match route {
                Some((.., status, body)) => Ok(Response::builder()
                    .status(*status)
                    .body(body.clone().into_bytes())
                    .unwrap()),
                None => Err(std::io::Error::other(format!("no route for {path}"))),
            };
            std::future::ready(response)
        }
    }

    #[derive(Default)]
    struct FakeCloud {
        fail_launch: bool,
        fail_start: bool,
        fail_instances: bool,
        instances: Vec<Instance>,
        launched: RefCell<Vec<(String, u64, String)>>,
        pool_launched: RefCell<Vec<String>>,
        started: RefCell<Vec<(String, String, u64, String)>>,
        stopped: RefCell<Vec<String>>,
        terminated: RefCell<Vec<String>>,
    }

    impl Cloud for FakeCloud {
        fn launch(
            &self,
            name: &str,
            runner_id: u64,
            user_data: &str,
        ) -> impl Future<Output = Result<String>> {
            self.launched
                .borrow_mut()
                .push((name.to_owned(), runner_id, user_data.to_owned()));
            std::future::ready(if self.fail_launch {
                Err(anyhow::anyhow!("InsufficientInstanceCapacity"))
            } else {
                Ok(format!("i-{runner_id}"))
            })
        }

        fn launch_pool(&self, name: &str) -> impl Future<Output = Result<String>> {
            self.pool_launched.borrow_mut().push(name.to_owned());
            std::future::ready(Ok(format!("i-{name}")))
        }

        fn start_pooled(
            &self,
            instance_id: &str,
            name: &str,
            runner_id: u64,
            user_data: &str,
        ) -> impl Future<Output = Result<()>> {
            self.started.borrow_mut().push((
                instance_id.to_owned(),
                name.to_owned(),
                runner_id,
                user_data.to_owned(),
            ));
            std::future::ready(if self.fail_start {
                Err(anyhow::anyhow!("IncorrectInstanceState"))
            } else {
                Ok(())
            })
        }

        fn stop(&self, instance_id: &str) -> impl Future<Output = Result<()>> {
            self.stopped.borrow_mut().push(instance_id.to_owned());
            std::future::ready(Ok(()))
        }

        fn instances(&self) -> impl Future<Output = Result<Vec<Instance>>> {
            std::future::ready(if self.fail_instances {
                Err(anyhow::anyhow!(
                    "credentials provider was not properly configured"
                ))
            } else {
                Ok(self.instances.clone())
            })
        }

        fn terminate(&self, instance_id: &str) -> impl Future<Output = Result<()>> {
            self.terminated.borrow_mut().push(instance_id.to_owned());
            std::future::ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct FakeStore {
        fail: bool,
        stored: RefCell<Vec<(String, String)>>,
    }

    impl ConfigStore for FakeStore {
        fn url(&self) -> &'static str {
            "https://broker.example"
        }

        fn store(&self, ticket: &str, config: &Secret) -> impl Future<Output = Result<()>> {
            self.stored
                .borrow_mut()
                .push((ticket.to_owned(), config.expose().to_owned()));
            std::future::ready(if self.fail {
                Err(anyhow::anyhow!("broker answered 500"))
            } else {
                Ok(())
            })
        }
    }

    type TestDriver = Driver<FakeGitHub, FakeCloud, FakeStore>;

    fn driver(github: &FakeGitHub, cloud: FakeCloud) -> TestDriver {
        driver_with_store(github, cloud, FakeStore::default())
    }

    fn driver_with_store(github: &FakeGitHub, cloud: FakeCloud, store: FakeStore) -> TestDriver {
        let session: Session = serde_json::from_str(
            r#"{"sessionId":"s1","messageQueueUrl":"https://broker.example/message","messageQueueAccessToken":"q1"}"#,
        )
        .unwrap();
        Driver {
            github: Rc::new(GitHub::new(
                github.clone(),
                ConfigUrl::parse("https://github.com/hatchrun").unwrap(),
                Credentials::Token(Secret::new("pat")),
            )),
            cloud,
            store,
            scaler: Scaler::new(Config {
                name_prefix: "linux".into(),
                ..Config::default()
            }),
            scale_set_id: 9,
            actions: Actions::new("https://pipelines.example/t/", Secret::new(ADMIN_TOKEN))
                .unwrap(),
            session,
            last_message_id: 0,
            clock: || NOW,
        }
    }

    fn jit_route(github: &FakeGitHub) {
        github.route(
            Method::POST,
            "/generatejitconfig",
            200,
            r#"{"runner":{"id":4,"name":"x"},"encodedJITConfig":"jit-4"}"#,
        );
    }

    fn message(id: u64, jobs: &serde_json::Value, assigned: u32) -> Message {
        let body = serde_json::json!({
            "messageId": id,
            "messageType": "RunnerScaleSetJobMessages",
            "body": jobs.to_string(),
            "statistics": {"totalAssignedJobs": assigned},
        })
        .to_string();
        let session: Session = serde_json::from_str(
            r#"{"sessionId":"s","messageQueueUrl":"https://q.example/m","messageQueueAccessToken":"t"}"#,
        )
        .unwrap();
        session
            .get_message(0, 1)
            .unwrap()
            .parser
            .parse(
                &Response::builder()
                    .status(200)
                    .body(body.into_bytes())
                    .unwrap(),
            )
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn message_acquires_launches_and_deletes() {
        let github = FakeGitHub::default();
        jit_route(&github);
        github.route(
            Method::POST,
            "/acquirejobs",
            200,
            r#"{"count":1,"value":[11]}"#,
        );
        github.route(Method::DELETE, "/message/42", 204, "");
        let mut driver = driver(&github, FakeCloud::default());

        let jobs = serde_json::json!([{"messageType": "JobAvailable", "runnerRequestId": 11}]);
        driver.handle(message(42, &jobs, 1)).await.unwrap();

        let paths: Vec<String> = github.sent().into_iter().map(|(_, path)| path).collect();
        assert_eq!(
            paths,
            [
                "/t/_apis/runtime/runnerscalesets/9/acquirejobs",
                "/t/_apis/runtime/runnerscalesets/9/generatejitconfig",
                "/message/42",
            ]
        );
        // User data carries the broker and a ticket, never the config.
        let launched = driver.cloud.launched.borrow();
        assert_eq!(launched[0].1, 4);
        assert!(!launched[0].2.contains("jit-4"));
        let boot: serde_json::Value = serde_json::from_str(&launched[0].2).unwrap();
        assert_eq!(boot["broker"], "https://broker.example");
        let stored = driver.store.stored.borrow();
        assert_eq!(
            stored[0],
            (
                boot["ticket"].as_str().unwrap().to_owned(),
                "jit-4".to_owned()
            )
        );
        let runner = driver.scaler.runners().values().next().unwrap();
        assert_eq!(runner.phase, Phase::Idle);
        assert_eq!(runner.instance_id.as_deref(), Some("i-4"));
        assert_eq!(driver.last_message_id, 42);
    }

    #[tokio::test]
    async fn failed_launch_removes_registered_runner() {
        let github = FakeGitHub::default();
        jit_route(&github);
        github.route(Method::DELETE, "/agents/4", 204, "");
        let mut driver = driver(
            &github,
            FakeCloud {
                fail_launch: true,
                ..FakeCloud::default()
            },
        );
        driver.scaler.handle_jobs([], Some(1), NOW);
        let actions = driver.scaler.plan(NOW);
        driver.execute(actions).await.unwrap();

        assert!(github.sent().contains(&(
            Method::DELETE,
            "/t/_apis/distributedtask/pools/0/agents/4".into()
        )));
        assert_eq!(driver.scaler.runners().len(), 0);
        assert_eq!(driver.cloud.terminated.borrow().len(), 0);
    }

    #[tokio::test]
    async fn failed_store_terminates_instance_and_removes_runner() {
        let github = FakeGitHub::default();
        jit_route(&github);
        github.route(Method::DELETE, "/agents/4", 204, "");
        let mut driver = driver_with_store(
            &github,
            FakeCloud::default(),
            FakeStore {
                fail: true,
                ..FakeStore::default()
            },
        );
        driver.scaler.handle_jobs([], Some(1), NOW);
        let actions = driver.scaler.plan(NOW);
        driver.execute(actions).await.unwrap();

        assert_eq!(*driver.cloud.terminated.borrow(), ["i-4"]);
        assert!(github.sent().contains(&(
            Method::DELETE,
            "/t/_apis/distributedtask/pools/0/agents/4".into()
        )));
        assert_eq!(driver.scaler.runners().len(), 0);
    }

    #[tokio::test]
    async fn expired_session_token_is_refreshed() {
        let github = FakeGitHub::default();
        github.route(Method::DELETE, "/message/7", 401, "");
        github.route(
            Method::PATCH,
            "/sessions/s1",
            200,
            r#"{"sessionId":"s1","messageQueueUrl":"https://broker.example/message2","messageQueueAccessToken":"q2"}"#,
        );
        let mut driver = driver(&github, FakeCloud::default());

        // The retry goes to the refreshed queue URL, which has no route,
        // so the call fails after the refresh.
        let err = driver
            .with_session(|_, session| session.delete_message(7))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no route for /message2/7"),
            "{err:#}"
        );
        assert_eq!(driver.session.message_queue_access_token.expose(), "q2");
    }

    #[tokio::test]
    async fn reconcile_adopts_and_removes_runners() {
        let github = FakeGitHub::default();
        github.route(Method::DELETE, "/agents/5", 204, "");
        let mut driver = driver(
            &github,
            FakeCloud {
                instances: vec![Instance {
                    id: "i-5".into(),
                    runner_name: Some("linux-old".into()),
                    runner_id: Some(5),
                    launched_at: NOW - 3600,
                    stopped: false,
                }],
                ..FakeCloud::default()
            },
        );

        // Adopted, idle for an hour, so removed and terminated.
        driver.reconcile().await.unwrap();
        assert_eq!(*driver.cloud.terminated.borrow(), ["i-5"]);
        assert_eq!(driver.scaler.runners().len(), 0);
    }

    #[tokio::test]
    async fn busy_runner_is_kept() {
        let github = FakeGitHub::default();
        github.route(
            Method::DELETE,
            "/agents/5",
            400,
            r#"{"typeName":"JobStillRunningException","message":"busy"}"#,
        );
        let mut driver = driver(
            &github,
            FakeCloud {
                instances: vec![Instance {
                    id: "i-5".into(),
                    runner_name: Some("linux-old".into()),
                    runner_id: Some(5),
                    launched_at: NOW - 3600,
                    stopped: false,
                }],
                ..FakeCloud::default()
            },
        );
        driver.reconcile().await.unwrap();
        assert_eq!(driver.scaler.runners()["linux-old"].phase, Phase::Busy);
        assert_eq!(driver.cloud.terminated.borrow().len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_closes_the_session() {
        let github = FakeGitHub::default();
        github.route(Method::GET, "/message", 503, "");
        github.route(Method::DELETE, "/sessions/s1", 204, "");
        let driver = driver(&github, FakeCloud::default());

        driver
            .run(
                tokio::time::sleep(Duration::from_secs(1)),
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(
            github.sent().last().unwrap(),
            &(
                Method::DELETE,
                "/t/_apis/runtime/runnerscalesets/9/sessions/s1".into()
            )
        );
    }

    #[tokio::test]
    async fn failed_first_reconcile_closes_the_session() {
        let github = FakeGitHub::default();
        github.route(Method::DELETE, "/sessions/s1", 204, "");
        let driver = driver(
            &github,
            FakeCloud {
                fail_instances: true,
                ..FakeCloud::default()
            },
        );

        let err = driver
            .run(std::future::pending(), Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("credentials"), "{err:#}");
        assert_eq!(
            github.sent(),
            [(
                Method::DELETE,
                "/t/_apis/runtime/runnerscalesets/9/sessions/s1".into()
            )]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_repeated_failures() {
        let github = FakeGitHub::default();
        github.route(Method::GET, "/message", 503, "");
        github.route(Method::DELETE, "/sessions/s1", 204, "");
        let driver = driver(&github, FakeCloud::default());

        let err = driver
            .run(std::future::pending(), Duration::from_secs(60))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("503"), "{err:#}");
        let polls = github
            .sent()
            .iter()
            .filter(|(method, _)| method == Method::GET)
            .count();
        assert_eq!(polls, MAX_FAILURES as usize);
    }

    /// A driver with a one-instance pool and a stopped pool instance in
    /// the inventory.
    fn pool_driver(github: &FakeGitHub, fail_start: bool) -> TestDriver {
        let mut driver = driver(
            github,
            FakeCloud {
                fail_start,
                instances: vec![Instance {
                    id: "i-pool".into(),
                    runner_name: None,
                    runner_id: None,
                    launched_at: NOW - 600,
                    stopped: true,
                }],
                ..FakeCloud::default()
            },
        );
        driver.scaler = Scaler::new(Config {
            name_prefix: "linux".into(),
            pool_size: 1,
            ..Config::default()
        });
        driver
    }

    #[tokio::test]
    async fn job_starts_a_pool_instance_and_the_pool_refills() {
        let github = FakeGitHub::default();
        jit_route(&github);
        github.route(
            Method::POST,
            "/acquirejobs",
            200,
            r#"{"count":1,"value":[11]}"#,
        );
        github.route(Method::DELETE, "/message/42", 204, "");
        let mut driver = pool_driver(&github, false);
        driver.reconcile().await.unwrap();
        assert_eq!(driver.cloud.pool_launched.borrow().len(), 0);

        let jobs = serde_json::json!([{"messageType": "JobAvailable", "runnerRequestId": 11}]);
        driver.handle(message(42, &jobs, 1)).await.unwrap();

        assert_eq!(driver.cloud.launched.borrow().len(), 0);
        let started = driver.cloud.started.borrow();
        assert_eq!(started[0].0, "i-pool");
        assert_eq!(started[0].2, 4);
        let boot: serde_json::Value = serde_json::from_str(&started[0].3).unwrap();
        assert_eq!(boot["broker"], "https://broker.example");
        assert_eq!(driver.store.stored.borrow()[0].1, "jit-4");
        let runner = driver.scaler.runners().values().next().unwrap();
        assert_eq!(runner.phase, Phase::Idle);
        assert_eq!(runner.instance_id.as_deref(), Some("i-pool"));
        // The pool refills with a new instance.
        assert_eq!(driver.cloud.pool_launched.borrow().len(), 1);
        assert_eq!(driver.scaler.pool().len(), 1);
    }

    #[tokio::test]
    async fn failed_pool_start_terminates_the_instance() {
        let github = FakeGitHub::default();
        jit_route(&github);
        github.route(Method::DELETE, "/agents/4", 204, "");
        let mut driver = pool_driver(&github, true);
        driver.reconcile().await.unwrap();
        driver.scaler.handle_jobs([], Some(1), NOW);
        let actions = driver.scaler.plan(NOW);
        driver.execute(actions).await.unwrap();

        assert_eq!(*driver.cloud.terminated.borrow(), ["i-pool"]);
        assert_eq!(driver.store.stored.borrow().len(), 0);
        assert!(driver.scaler.runners().is_empty());
    }

    #[test]
    fn validates_limits() {
        assert!(validate(0, 0).is_err());
        assert!(validate(3, 2).is_err());
        validate(0, 1).unwrap();
    }
}
