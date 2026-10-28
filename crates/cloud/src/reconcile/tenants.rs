// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Step 3 of a pass: per tenant instances, launches and sweeps.

use super::{
    Credentials, DENIED, Fallible, INSTANCE_PAGES, Instance, Job, JobRow, Kind, LAUNCH_COST,
    LAUNCHES_PER_TENANT, LaunchChange, LaunchRow, LaunchState, MARK_RECONCILED, MARK_STACK_CHECKED,
    MARK_SWEPT, Pass, Plan, QueuedJob, SESSION, SWEEP_EVERY, SWEEP_MARGIN, StreamExt, Summary,
    TENANTS_AT_ONCE, TENANTS_PER_PASS, TERMINATE_BATCH, Tenant, VERSION_PAGES, aws, console_error,
    console_log, console_warn, ec2, fleet, get, instances, jobs, json, launch, now_ms, oidc,
    params, plan, queued_job, retry, send, ssm, start, status, stream, tenants_query, text, time,
};

impl Pass<'_> {
    /// Step 3: every tenant with a verified role, least recently
    /// reconciled first.
    pub(super) async fn tenants(&self) -> Fallible<Summary> {
        let tenants = self
            .db
            .all::<Tenant>(&tenants_query(), &params![TENANTS_PER_PASS])
            .await
            .map_err(text)?;
        if tenants.len() == TENANTS_PER_PASS as usize {
            self.more.set(true);
        }
        let oidc_key = self.secret_der(oidc::SIGNING_KEY);
        let oidc_kid = get(self.env, oidc::KEY_ID);
        let oidc = match (oidc_key, oidc_kid) {
            (Ok(key), Some(kid)) => Ok((key, kid)),
            (Err(err), _) => Err(err),
            (_, None) => Err(format!("{} is not set", oidc::KEY_ID)),
        };
        let results: Vec<Summary> = stream::iter(&tenants)
            .map(|tenant| self.tenant(tenant, oidc.as_ref()))
            .buffer_unordered(TENANTS_AT_ONCE)
            .collect()
            .await;
        let mut done = Summary::default();
        for result in &results {
            done.add(result);
        }
        Ok(done)
    }

    pub(super) async fn tenant(
        &self,
        tenant: &Tenant,
        oidc: Result<&(Vec<u8>, String), &String>,
    ) -> Summary {
        let mut done = Summary {
            tenants: 1,
            ..Summary::default()
        };
        if let Err(err) = self.reconcile_tenant(tenant, oidc, &mut done).await {
            console_error!("reconcile: tenant {}: {err}", tenant.id);
            done.errors += 1;
        }
        done
    }

    pub(super) async fn reconcile_tenant(
        &self,
        tenant: &Tenant,
        oidc: Result<&(Vec<u8>, String), &String>,
        done: &mut Summary,
    ) -> Fallible<()> {
        // Moved to the back of the queue first, so a tenant that fails
        // every time does not hold up the rest.
        self.db
            .run(MARK_RECONCILED, &params![tenant.id.as_str(), self.now])
            .await
            .map_err(text)?;

        let endpoint = aws::endpoint("ec2", &tenant.region, self.aws_endpoint.as_deref());
        let live = match oidc.map_err(Clone::clone) {
            Ok((key, kid)) => self.live_instances(tenant, key, kid, &endpoint).await,
            Err(err) => Err(err),
        };
        let (credentials, instances) = match live {
            Ok((credentials, instances)) => (Some(credentials), Some(instances)),
            Err(err) => {
                // Jobs that never launched can still launch; `launch`
                // records why if the role fails it too.
                console_warn!("reconcile: tenant {}: {err}", tenant.id);
                done.errors += 1;
                (None, None)
            }
        };
        if let Some(credentials) = &credentials
            && self
                .now
                .saturating_sub(tenant.stack_checked_at.unwrap_or(0))
                > crate::stack::CHECK_EVERY
            && self.spend(1)
        {
            match self.check_stack(tenant, credentials).await {
                Ok(true) => done.stacks_changed += 1,
                Ok(false) => {}
                Err(err) => {
                    console_warn!("reconcile: tenant {}: stack: {err}", tenant.id);
                    done.errors += 1;
                }
            }
        }
        let jobs = self.tenant_jobs(tenant, instances.as_deref()).await?;
        let plan = plan::plan(
            self.now,
            &self.limits,
            tenant.max_runners,
            instances.as_deref(),
            &jobs,
        );
        if let Some(credentials) = &credentials {
            self.terminate(tenant, credentials, &endpoint, &plan, done)
                .await;
        }
        self.update_jobs(tenant, &plan, done).await?;
        if let Some(credentials) = &credentials
            && self
                .now
                .saturating_sub(tenant.versions_swept_at.unwrap_or(0))
                > SWEEP_EVERY
        {
            match self
                .sweep_versions(tenant, credentials, &endpoint, &jobs)
                .await
            {
                Ok(deleted) => done.versions_deleted += deleted,
                Err(err) => {
                    console_warn!("reconcile: tenant {}: template versions: {err}", tenant.id);
                    done.errors += 1;
                }
            }
        }
        self.launch(tenant, &plan, done).await
    }

    /// Deletes the launch template versions that launches made and did
    /// not delete, because the Worker died between the two. A version
    /// made before the oldest launch in flight, less a margin, belongs
    /// to no launch still running. Returns how many it deleted.
    pub(super) async fn sweep_versions(
        &self,
        tenant: &Tenant,
        credentials: &Credentials,
        endpoint: &str,
        jobs: &[Job],
    ) -> Fallible<u32> {
        // A job is queued before it is claimed and its version made.
        let in_flight = jobs
            .iter()
            .filter(|job| job.launch_state == Some(LaunchState::Launching))
            .map(|job| job.queued_at)
            .min()
            .unwrap_or(self.now);
        let before = in_flight.min(self.now).saturating_sub(SWEEP_MARGIN);
        let template = tenant.launch_template_id.as_str();
        let mut found = Vec::new();
        let mut next: Option<String> = None;
        for _ in 0..VERSION_PAGES {
            if !self.spend(1) {
                return Ok(0);
            }
            let mut request = fleet::describe_versions(endpoint, template, next.as_deref());
            aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms())?;
            match fleet::versions(&send(&request).await?) {
                Ok((page, token)) => {
                    found.extend(page);
                    next = token;
                }
                Err(err) if DENIED.contains(&err.code.as_str()) => {
                    console_log!(
                        "reconcile: tenant {}: connect stack before version 5 may not list \
                         template versions; skipped",
                        tenant.id
                    );
                    self.swept(tenant).await?;
                    return Ok(0);
                }
                Err(err) => {
                    // Stamped all the same, so a failing call is not made
                    // every minute.
                    self.swept(tenant).await?;
                    return Err(format!("DescribeLaunchTemplateVersions: {err}"));
                }
            }
            if next.is_none() {
                break;
            }
        }
        let mut deleted = 0;
        for batch in fleet::stale(&found, before).chunks(fleet::DELETE_BATCH) {
            if !self.spend(1) {
                return Ok(deleted);
            }
            let mut request = fleet::delete_versions(endpoint, template, batch);
            aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms())?;
            let done = fleet::deleted(&send(&request).await?)
                .map_err(|err| format!("DeleteLaunchTemplateVersions: {err}"))?;
            for (version, reason) in &done.failed {
                console_warn!(
                    "reconcile: tenant {}: delete template version {version}: {reason}",
                    tenant.id
                );
            }
            deleted += u32::try_from(done.deleted.len()).unwrap_or(u32::MAX);
        }
        if deleted > 0 {
            console_log!(
                "reconcile: tenant {}: deleted {deleted} template versions left by launches",
                tenant.id
            );
        }
        // A template with more pages is swept again on the next pass,
        // from the first page, which now holds the versions left. One
        // that deleted nothing would read the same pages again, so it
        // waits a day like the rest.
        if next.is_some() && deleted > 0 {
            self.more.set(true);
        } else {
            self.swept(tenant).await?;
        }
        Ok(deleted)
    }

    pub(super) async fn swept(&self, tenant: &Tenant) -> Fallible<()> {
        self.db
            .run(MARK_SWEPT, &params![tenant.id.as_str(), self.now])
            .await
            .map_err(text)?;
        Ok(())
    }

    /// Reads the tenant's stack settings again and records what they
    /// say. Returns whether they changed; a change puts jobs waiting for
    /// the setup back in line.
    pub(super) async fn check_stack(
        &self,
        tenant: &Tenant,
        credentials: &Credentials,
    ) -> Fallible<bool> {
        let endpoint = aws::endpoint("ssm", &tenant.region, self.aws_endpoint.as_deref());
        let mut request =
            ssm::get_parameter(&endpoint, &crate::connect::parameter_name(&tenant.id));
        aws::sign_v4(&mut request, credentials, &tenant.region, "ssm", now_ms())?;
        let stack = ssm::value(&send(&request).await?)
            .map_err(|err| format!("GetParameter: {err}"))
            .and_then(|value| crate::connect::Stack::parse(&value));
        let stack = match stack {
            Ok(stack) => stack,
            Err(err) => {
                // Stamped all the same, so a deleted stack is not asked about
                // every minute; launches say what is wrong.
                self.db
                    .run(MARK_STACK_CHECKED, &params![tenant.id.as_str(), self.now])
                    .await
                    .map_err(text)?;
                return Err(err);
            }
        };
        let changed = crate::stack::record(&self.db, &tenant.id, &stack, self.now)
            .await
            .map_err(text)?;
        if changed {
            console_log!(
                "reconcile: tenant {}: connect stack changed, template version {}",
                tenant.id,
                stack.version
            );
            launch::recheck(&self.db, &tenant.id).await.map_err(text)?;
        }
        Ok(changed)
    }

    /// The tenant role's credentials and its pending and running
    /// instances.
    pub(super) async fn live_instances(
        &self,
        tenant: &Tenant,
        oidc_key: &[u8],
        oidc_kid: &str,
        endpoint: &str,
    ) -> Fallible<(Credentials, Vec<Instance>)> {
        if !self.spend(2) {
            return Err("out of requests for this pass".to_owned());
        }
        let credentials = start::role_credentials(
            oidc_key,
            oidc_kid,
            self.aws_endpoint.as_deref(),
            &tenant.id,
            &tenant.role_arn,
            &tenant.region,
            SESSION,
        )
        .await
        .map_err(|err| format!("AssumeRoleWithWebIdentity {}: {err}", tenant.role_arn))?;
        let mut found = Vec::new();
        let mut next: Option<String> = None;
        for page in 0..INSTANCE_PAGES {
            if page > 0 && !self.spend(1) {
                return Err("out of requests while describing instances".to_owned());
            }
            let mut request = instances::describe(endpoint, &tenant.id, next.as_deref());
            aws::sign_v4(&mut request, &credentials, &tenant.region, "ec2", now_ms())?;
            let (described, token) = instances::described(&send(&request).await?)
                .map_err(|err| format!("DescribeInstances: {err}"))?;
            found.extend(described);
            next = token;
            if next.is_none() {
                break;
            }
        }
        if next.is_some() {
            // Deciding on part of the fleet could terminate a runner
            // whose job is on the next page.
            return Err(format!(
                "more than {INSTANCE_PAGES} pages of instances; not deciding on part of them"
            ));
        }
        let instances = found
            .into_iter()
            .filter_map(|described| {
                let launched_at = time::parse(&described.launch_time);
                if launched_at.is_none() {
                    console_warn!(
                        "reconcile: {}: unreadable launch time {:?}",
                        described.id,
                        described.launch_time
                    );
                }
                Some(Instance {
                    launched_at: launched_at?,
                    job_id: described.job_id.and_then(|id| id.parse().ok()),
                    runner_name: described.runner_name,
                    id: described.id,
                })
            })
            .collect();
        Ok((credentials, instances))
    }

    /// The tenant's open jobs, and the finished ones its instances name.
    pub(super) async fn tenant_jobs(
        &self,
        tenant: &Tenant,
        instances: Option<&[Instance]>,
    ) -> Fallible<Vec<Job>> {
        let Some(installation_id) = tenant.installation_id else {
            return Ok(Vec::new());
        };
        let instances = instances.unwrap_or_default();
        let ids: Vec<u64> = instances
            .iter()
            .filter_map(|instance| instance.job_id)
            .collect();
        let runners: Vec<&str> = instances
            .iter()
            .filter_map(|instance| instance.runner_name.as_deref())
            .collect();
        let rows =
            jobs::tenant_jobs::<JobRow>(&self.db, installation_id, &json(&ids)?, &json(&runners)?)
                .await
                .map_err(text)?;
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                Some(Job {
                    id: row.id,
                    label: row.label,
                    status: status(&row.status)?,
                    launch_state: row.launch_state.as_deref().and_then(LaunchState::parse),
                    runner_name: row.runner_name,
                    ran_on: row.ran_on,
                    queued_at: row.queued_at,
                    updated_at: row.updated_at,
                    launch_retry: row.launch_retry.as_deref().and_then(retry::Wait::parse),
                    next_launch_at: row.next_launch_at,
                })
            })
            .collect())
    }

    pub(super) async fn terminate(
        &self,
        tenant: &Tenant,
        credentials: &Credentials,
        endpoint: &str,
        plan: &Plan,
        done: &mut Summary,
    ) {
        for batch in plan.terminate.chunks(TERMINATE_BATCH) {
            if !self.spend(1) {
                return;
            }
            let ids: Vec<&str> = batch.iter().map(|(id, _)| id.as_str()).collect();
            let listed = batch
                .iter()
                .map(|(id, reason)| format!("{id} ({})", reason.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            let mut request = instances::terminate(endpoint, &ids);
            let result =
                match aws::sign_v4(&mut request, credentials, &tenant.region, "ec2", now_ms()) {
                    Ok(()) => send(&request).await.and_then(|response| {
                        ec2::terminated(&response).map_err(|err| err.to_string())
                    }),
                    Err(err) => Err(err),
                };
            match result {
                Ok(()) => {
                    console_log!("reconcile: tenant {}: terminated {listed}", tenant.id);
                    done.terminated += u32::try_from(batch.len()).unwrap_or(u32::MAX);
                }
                Err(err) => {
                    console_warn!("reconcile: tenant {}: terminate {listed}: {err}", tenant.id);
                    done.errors += 1;
                }
            }
        }
    }

    /// Adopts, undoes and gives up launches, each only while the row
    /// still says what the plan saw.
    pub(super) async fn update_jobs(
        &self,
        tenant: &Tenant,
        plan: &Plan,
        done: &mut Summary,
    ) -> Fallible<()> {
        let mut changes: Vec<(LaunchChange, Kind)> = Vec::new();
        for (job_id, instance_id) in &plan.adopt {
            changes.push((
                LaunchChange::Adopt {
                    id: *job_id,
                    instance_id: instance_id.as_str(),
                },
                Kind::Adopted,
            ));
        }
        for reset in &plan.reset {
            changes.push((
                LaunchChange::Reset {
                    id: reset.job_id,
                    launch_state: reset.launch_state.as_str(),
                    runner_name: reset.runner_name.as_deref(),
                },
                Kind::Reset,
            ));
        }
        for (job_id, runner) in &plan.give_up {
            let reason = format!(
                "runner {runner} waited {} minutes without GitHub handing it the job",
                self.limits.idle_timeout / 60_000
            );
            changes.push((
                LaunchChange::GiveUp {
                    id: *job_id,
                    reason,
                    runner: runner.as_str(),
                },
                Kind::GaveUp,
            ));
        }
        if changes.is_empty() {
            return Ok(());
        }
        let (changes, kinds): (Vec<_>, Vec<_>) = changes.into_iter().unzip();
        let results = jobs::update_launches(&self.db, &changes, self.now)
            .await
            .map_err(text)?;
        for (changed, kind) in results.into_iter().zip(kinds) {
            if changed != 1 {
                continue;
            }
            match kind {
                Kind::Adopted => done.adopted += 1,
                Kind::Reset => done.reset += 1,
                Kind::GaveUp => done.gave_up += 1,
            }
        }
        if done.adopted + done.reset + done.gave_up > 0 {
            console_log!(
                "reconcile: tenant {}: adopted {}, reset {}, gave up {}",
                tenant.id,
                done.adopted,
                done.reset,
                done.gave_up
            );
        }
        Ok(())
    }

    /// Launches the plan's jobs through `launch`, which claims each
    /// first and stops at `max_runners`.
    pub(super) async fn launch(
        &self,
        tenant: &Tenant,
        plan: &Plan,
        done: &mut Summary,
    ) -> Fallible<()> {
        if plan.launch.len() > LAUNCHES_PER_TENANT {
            self.more.set(true);
        }
        let ids: Vec<u64> = plan
            .launch
            .iter()
            .copied()
            .take(LAUNCHES_PER_TENANT)
            .collect();
        for job in self.queued_jobs(&ids).await? {
            if !self.spend(LAUNCH_COST) {
                break;
            }
            console_log!(
                "reconcile: tenant {}: launching job {} again",
                tenant.id,
                job.id
            );
            launch::launch(self.env.clone(), job).await;
            done.launched += 1;
        }
        Ok(())
    }

    /// Jobs as `launch` takes them, oldest first.
    pub(super) async fn queued_jobs(&self, ids: &[u64]) -> Fallible<Vec<QueuedJob>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = jobs::queued_jobs::<LaunchRow>(&self.db, &json(&ids)?)
            .await
            .map_err(text)?;
        Ok(rows.into_iter().filter_map(queued_job).collect())
    }
}
