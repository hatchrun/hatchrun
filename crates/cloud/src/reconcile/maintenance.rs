// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Steps 4 to 7 of a pass: catalog, unconnected installations, meter and retention.

use super::{
    CACHE_UPLOAD_MEMORY, CATALOG_COST, CATALOG_PAGES, CatalogTenant, DELETE_BATCH,
    DELETE_OFFERINGS, DELETE_REDELIVERIES, DELETE_SESSIONS, DELETE_USAGE, FAILED_LAUNCH_MEMORY,
    Fallible, LaunchRow, POLAR_ACCESS_TOKEN, POLAR_API_URL, Pass, RATE_LIMIT_MEMORY,
    REDELIVERY_MEMORY, SESSION, SWEEP_WINDOW, Summary, UNCONNECTED_PER_PASS, UNSET_RETENTION,
    USAGE_MEMORY, alerts, aws, catalog, catalog_query, console_log, describe, get, instance_types,
    jobs, launch, now_ms, oidc, params, polar, queued_job, send, start, text, time, usage,
};

impl Pass<'_> {
    /// Step 4: refreshes the catalog of the region least recently
    /// refreshed among those that are due, through the role of a tenant
    /// there chosen at random, so one tenant whose role refuses does not
    /// hold the region back.
    pub(super) async fn catalog(&self) -> Fallible<Summary> {
        let due: Option<CatalogTenant> = self
            .db
            .first(
                &catalog_query(),
                &params![
                    self.now.saturating_sub(instance_types::FRESH_FOR),
                    self.now.saturating_sub(instance_types::RETRY_AFTER),
                ],
            )
            .await
            .map_err(text)?;
        let Some(tenant) = due else {
            return Ok(Summary::default());
        };
        if !self.spend(CATALOG_COST) {
            return Ok(Summary::default());
        }
        match self.refresh_catalog(&tenant).await {
            Ok(entries) => {
                catalog::replace(&self.db, &tenant.region, &entries, self.now)
                    .await
                    .map_err(text)?;
                console_log!(
                    "reconcile: catalog of {}: {} instance types",
                    tenant.region,
                    entries.len()
                );
                Ok(Summary {
                    catalog_types: u32::try_from(entries.len()).unwrap_or(u32::MAX),
                    ..Summary::default()
                })
            }
            Err(err) => {
                let err = format!("through tenant {}: {err}", tenant.id);
                catalog::failed(&self.db, &tenant.region, &err, self.now)
                    .await
                    .map_err(text)?;
                Err(format!("catalog of {}: {err}", tenant.region))
            }
        }
    }

    /// Reads `tenant`'s region's instance types, offerings and prices.
    pub(super) async fn refresh_catalog(
        &self,
        tenant: &CatalogTenant,
    ) -> Fallible<Vec<instance_types::Entry>> {
        let price_url = instance_types::price_list_url(&tenant.region)
            .ok_or_else(|| format!("no price list for {}", tenant.region))?;
        let key = self.secret_der(oidc::SIGNING_KEY)?;
        let kid =
            get(self.env, oidc::KEY_ID).ok_or_else(|| format!("{} is not set", oidc::KEY_ID))?;
        let credentials = start::role_credentials(
            &key,
            &kid,
            self.aws_endpoint.as_deref(),
            &tenant.id,
            &tenant.role_arn,
            &tenant.region,
            SESSION,
        )
        .await
        .map_err(|err| format!("AssumeRoleWithWebIdentity {}: {err}", tenant.role_arn))?;
        let endpoint = aws::endpoint("ec2", &tenant.region, self.aws_endpoint.as_deref());
        let call = |mut request: crate::http::Request| {
            let credentials = &credentials;
            let region = tenant.region.as_str();
            async move {
                aws::sign_v4(&mut request, credentials, region, "ec2", now_ms())?;
                send(&request).await
            }
        };

        let mut types = Vec::new();
        let mut next = None;
        for _ in 0..CATALOG_PAGES {
            let response = call(describe::instance_types(&endpoint, next.as_deref())).await?;
            let (page, token) = describe::described_types(&response)
                .map_err(|err| format!("DescribeInstanceTypes: {err}"))?;
            types.extend(page);
            next = token;
            if next.is_none() {
                break;
            }
        }
        let mut offerings = Vec::new();
        for _ in 0..CATALOG_PAGES {
            let response = call(describe::offerings(&endpoint, next.as_deref())).await?;
            let (page, token) = describe::described_offerings(&response)
                .map_err(|err| format!("DescribeInstanceTypeOfferings: {err}"))?;
            offerings.extend(page);
            next = token;
            if next.is_none() {
                break;
            }
        }
        let now = time::format(self.now);
        let mut spot = Vec::new();
        for _ in 0..CATALOG_PAGES {
            let response = call(describe::spot_prices(&endpoint, &now, next.as_deref())).await?;
            let (page, token) = describe::described_spot_prices(&response)
                .map_err(|err| format!("DescribeSpotPriceHistory: {err}"))?;
            spot.extend(page);
            next = token;
            if next.is_none() {
                break;
            }
        }
        if next.is_some() {
            return Err(format!(
                "more than {CATALOG_PAGES} pages of one describe call"
            ));
        }
        let prices = send(&crate::http::Request::new("GET", price_url)).await?;
        if !prices.ok() {
            return Err(format!("price list answered {}", prices.status));
        }
        let on_demand = instance_types::on_demand_prices(&prices.body)?;
        Ok(instance_types::merge(types, &offerings, &on_demand, &spot))
    }

    /// Step 5: fails the lost launches of installations with no AWS
    /// connected. `launch` finds no tenant and says so on the job.
    pub(super) async fn unconnected(&self) -> Fallible<Summary> {
        let before = self.now.saturating_sub(self.limits.queued_grace);
        let rows = jobs::unconnected::<LaunchRow>(&self.db, before, UNCONNECTED_PER_PASS)
            .await
            .map_err(text)?;
        let mut done = Summary::default();
        for job in rows.into_iter().filter_map(queued_job) {
            launch::launch(self.env.clone(), job).await;
            done.unconnected += 1;
        }
        Ok(done)
    }

    /// Step 6: records the runner seconds of jobs that completed in the
    /// last [`SWEEP_WINDOW`] with none recorded, then sends the seconds
    /// Polar has not taken yet, oldest first, one batch a pass. Without
    /// Polar's token nothing is sent and the events wait.
    pub(super) async fn meter(&self) -> Fallible<Summary> {
        let swept = usage::sweep(
            &self.db,
            self.now.saturating_sub(SWEEP_WINDOW),
            DELETE_BATCH,
        )
        .await
        .map_err(text)?;
        if swept > 0 {
            console_log!("reconcile: recorded the runner time of {swept} jobs no webhook recorded");
        }
        let Some(token) = get(self.env, POLAR_ACCESS_TOKEN) else {
            return Ok(Summary::default());
        };
        let batch = u32::try_from(polar::INGEST_BATCH).unwrap_or(u32::MAX);
        let unsent = usage::unsent(&self.db, batch).await.map_err(text)?;
        if unsent.is_empty() || !self.spend(1) {
            return Ok(Summary::default());
        }
        let api = get(self.env, POLAR_API_URL)
            .unwrap_or_else(|| polar::API.to_owned())
            .trim_end_matches('/')
            .to_owned();
        let events: Vec<polar::JobSeconds<'_>> = unsent
            .iter()
            .map(|event| polar::JobSeconds {
                job_id: event.job_id,
                tenant: &event.tenant_id,
                seconds: event.seconds,
                at: time::format(event.ended_at),
            })
            .collect();
        polar::ingested(&send(&polar::ingest(&api, &token, &events)).await?)?;
        let jobs: Vec<u64> = unsent.iter().map(|event| event.job_id).collect();
        usage::sent(&self.db, &jobs, self.now).await.map_err(text)?;
        if unsent.len() == polar::INGEST_BATCH {
            self.more.set(true);
        }
        Ok(Summary {
            metered: u32::try_from(jobs.len()).unwrap_or(u32::MAX),
            ..Summary::default()
        })
    }

    /// Step 7: deletes expired sessions, jobs past the retention,
    /// redeliveries GitHub no longer lists, lapsed unavailable offerings,
    /// cache uploads nobody finished, whose parts S3's lifecycle rule
    /// removes after a day, runner seconds older than [`USAGE_MEMORY`]
    /// that Polar has or never needed, and old failed launches and rate
    /// limits.
    pub(super) async fn retention(&self) -> Fallible<Summary> {
        let now = self.now;
        let results = self
            .db
            .batch(&[
                // Before the deletes, so a row without an expiry that
                // is past it goes in the same pass.
                (jobs::BACKFILL_JOBS, params![UNSET_RETENTION, DELETE_BATCH]),
                (jobs::BACKFILL_RUNS, params![UNSET_RETENTION, DELETE_BATCH]),
                (DELETE_SESSIONS, params![now, DELETE_BATCH]),
                (jobs::DELETE_JOBS, params![now, DELETE_BATCH]),
                (
                    DELETE_REDELIVERIES,
                    params![now.saturating_sub(REDELIVERY_MEMORY), DELETE_BATCH],
                ),
                (DELETE_OFFERINGS, params![now]),
                (
                    jobs::DELETE_UPLOADS,
                    params![now.saturating_sub(CACHE_UPLOAD_MEMORY), DELETE_BATCH],
                ),
                (
                    alerts::DELETE_FAILED,
                    params![now.saturating_sub(FAILED_LAUNCH_MEMORY), DELETE_BATCH],
                ),
                (
                    alerts::DELETE_RATE_LIMITS,
                    params![now.saturating_sub(RATE_LIMIT_MEMORY)],
                ),
                // After the jobs, so a run goes with its last job.
                (jobs::DELETE_RUNS, params![now, DELETE_BATCH]),
                (
                    DELETE_USAGE,
                    params![now.saturating_sub(USAGE_MEMORY), DELETE_BATCH],
                ),
            ])
            .await
            .map_err(text)?;
        let deleted = |at: usize| {
            results
                .get(at)
                .map_or(0, |changed| u32::try_from(*changed).unwrap_or(u32::MAX))
        };
        if [0, 1, 2, 3, 4, 6, 7, 9, 10]
            .into_iter()
            .any(|at| deleted(at) == DELETE_BATCH)
        {
            self.more.set(true);
        }
        Ok(Summary {
            sessions_deleted: deleted(2),
            jobs_deleted: deleted(3),
            runs_deleted: deleted(9),
            ..Summary::default()
        })
    }
}
