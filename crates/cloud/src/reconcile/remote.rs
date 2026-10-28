// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Steps 1 and 2 of a pass: redeliveries, open job sync and run details.

use super::{
    App, BTreeMap, CLAIM_REDELIVERY, Counted, DELIVERIES_UNTIL, DELIVERY_OVERLAP, DELIVERY_PAGES,
    DETAILS_AFTER, DETAILS_PER_PASS, Fallible, GITHUB_CALL_COST, Guid, HashSet,
    INSTALLATIONS_AT_ONCE, Kv, MOVE_DELIVERIES_UNTIL, Mark, OpenJob, Outcome, Pass,
    REDELIVERIES_PER_PASS, REMEMBERED, RunWithoutDetails, SYNC_PER_PASS, Status, StreamExt,
    Summary, Tokens, alerts, console_error, console_log, console_warn, deliveries, github, jobs,
    json, now_ms, params, runs, send, send_reading, status, stream, sync, text, usage,
};

impl Pass<'_> {
    /// Step 1: asks GitHub to deliver failed webhooks again.
    pub(super) async fn redeliver(&self) -> Fallible<Summary> {
        let mut done = Summary::default();
        let app_jwt = self.app_jwt().await?;
        let mark = self
            .db
            .first::<Mark>(DELIVERIES_UNTIL, &[])
            .await
            .map_err(text)?
            .and_then(|row| row.deliveries_until);
        let floor = self.now.saturating_sub(self.limits.redeliver_window);
        let since = mark.map_or(floor, |mark| {
            mark.saturating_sub(DELIVERY_OVERLAP).max(floor)
        });

        let (seen, complete) = self.scan(&app_jwt, since).await?;

        let chosen = deliveries::choose(&seen, since);
        let guids: Vec<&str> = chosen.iter().map(|chosen| chosen.guid.as_str()).collect();
        let remembered: HashSet<String> = self
            .db
            .all::<Guid>(REMEMBERED, &params![json(&guids)?])
            .await
            .map_err(text)?
            .into_iter()
            .map(|row| row.guid)
            .collect();
        let pending: Vec<_> = chosen
            .into_iter()
            .filter(|chosen| !remembered.contains(&chosen.guid))
            .collect();
        let mut handled = 0;
        for redelivery in pending.iter().take(REDELIVERIES_PER_PASS) {
            if !self.spend(1) {
                break;
            }
            handled += 1;
            match self.ask_again(&app_jwt, redelivery).await? {
                Some(true) => done.redelivered += 1,
                Some(false) => done.errors += 1,
                None => {}
            }
        }

        // The next scan starts where this one ended, once it saw every
        // delivery back to `since` and asked for every failed one.
        if complete && handled == pending.len() {
            if let Some(newest) = seen.iter().filter_map(deliveries::Delivery::at).max() {
                self.db
                    .run(MOVE_DELIVERIES_UNTIL, &params![newest])
                    .await
                    .map_err(text)?;
            }
        } else {
            self.more.set(true);
        }
        Ok(done)
    }

    /// Asks GitHub to deliver `redelivery` again, unless a pass already
    /// did. Whether GitHub agreed, or `None` when it was not asked.
    pub(super) async fn ask_again(
        &self,
        app_jwt: &str,
        redelivery: &deliveries::Redelivery,
    ) -> Fallible<Option<bool>> {
        // Remembered first, so two passes cannot both ask.
        let claimed = self
            .db
            .run(
                CLAIM_REDELIVERY,
                &params![
                    redelivery.guid.as_str(),
                    redelivery.id,
                    redelivery.event.as_str(),
                    self.now,
                ],
            )
            .await
            .map_err(text)?;
        if claimed != 1 {
            return Ok(None);
        }
        let request = deliveries::redeliver(&self.github_api, app_jwt, redelivery.id);
        let failure = match send(&request).await {
            Ok(response) if response.ok() => {
                console_log!(
                    "reconcile: redelivered {} {} (attempt {})",
                    redelivery.event,
                    redelivery.guid,
                    redelivery.id
                );
                return Ok(Some(true));
            }
            Ok(response) => format!("GitHub answered {}: {}", response.status, response.text()),
            Err(err) => err,
        };
        console_warn!("reconcile: redeliver {}: {failure}", redelivery.guid);
        Ok(Some(false))
    }

    /// The App's deliveries, newest first, back to `since` or as far as
    /// the pass's limits reach; and whether they reached it.
    pub(super) async fn scan(
        &self,
        app_jwt: &str,
        since: u64,
    ) -> Fallible<(Vec<deliveries::Delivery>, bool)> {
        let mut seen = Vec::new();
        let mut next = Some(deliveries::first_page(&self.github_api));
        let mut complete = false;
        for _ in 0..DELIVERY_PAGES {
            let Some(url) = next.clone() else {
                complete = true;
                break;
            };
            if !self.spend(1) {
                break;
            }
            let (response, link) =
                send_reading(&deliveries::list(&url, app_jwt), Some("Link")).await?;
            let page = deliveries::parse(&response)?;
            let reached = page.is_empty()
                || page
                    .iter()
                    .any(|delivery| delivery.at().is_some_and(|at| at < since));
            seen.extend(page);
            if reached {
                complete = true;
                break;
            }
            next = deliveries::next_page(link.as_deref());
        }
        complete |= next.is_none();
        Ok((seen, complete))
    }

    /// Step 2: asks GitHub about open jobs that have gone quiet.
    pub(super) async fn sync(&self) -> Fallible<Summary> {
        let quiet = self.now.saturating_sub(self.limits.sync_after);
        let open = jobs::open_jobs::<OpenJob>(&self.db, quiet, SYNC_PER_PASS)
            .await
            .map_err(text)?;
        if open.is_empty() {
            return Ok(Summary::default());
        }
        let calls = Counted::default();
        let tokens = self.tokens(&calls)?;
        let before = self.budget.get();
        let mut by_installation: BTreeMap<u64, Vec<OpenJob>> = BTreeMap::new();
        for job in open {
            by_installation
                .entry(job.installation_id)
                .or_default()
                .push(job);
        }
        let results: Vec<Summary> = stream::iter(by_installation)
            .map(|(installation_id, jobs)| self.sync_installation(&tokens, installation_id, jobs))
            .buffer_unordered(INSTALLATIONS_AT_ONCE)
            .collect()
            .await;
        self.settle(before, calls.sent());
        alerts::note_rate_limits(&self.db, &calls, self.now).await;
        let mut done = Summary::default();
        for result in &results {
            done.add(result);
        }
        Ok(done)
    }

    pub(super) async fn sync_installation(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        installation_id: u64,
        jobs: Vec<OpenJob>,
    ) -> Summary {
        let mut done = Summary::default();
        if !self.spend(1) {
            return done;
        }
        if let Err(err) = tokens.token(installation_id).await {
            console_warn!("reconcile: installation {installation_id}: {err}");
            done.errors += 1;
            for job in &jobs {
                if let Err(err) = self.checked(job, None).await {
                    console_error!("reconcile: job {}: {err}", job.id);
                }
            }
            return done;
        }
        for job in &jobs {
            if !self.spend(GITHUB_CALL_COST) {
                break;
            }
            done.checked += 1;
            let seen = match tokens
                .send(installation_id, |token| {
                    sync::job(&self.github_api, token, &job.repository, job.id)
                })
                .await
                .map_err(|err| err.to_string())
                .and_then(|response| sync::parse(&response))
            {
                Ok(seen) => Some(seen),
                Err(err) => {
                    console_warn!("reconcile: job {}: {err}", job.id);
                    done.errors += 1;
                    None
                }
            };
            match self.checked(job, seen.as_ref()).await {
                Ok(true) => done.closed += 1,
                Ok(false) => {}
                Err(err) => {
                    console_error!("reconcile: job {}: {err}", job.id);
                    done.errors += 1;
                }
            }
        }
        done
    }

    /// Records what GitHub said about `job`, or only that it was asked.
    /// Returns whether the job is now completed.
    pub(super) async fn checked(&self, job: &OpenJob, seen: Option<&sync::Seen>) -> Fallible<bool> {
        let stored = status(&job.status).unwrap_or(Status::Queued);
        let next = seen.map_or(stored, |seen| sync::advance(stored, seen.status));
        let runner = seen.and_then(|seen| seen.runner_name.as_deref());
        let outcome = seen.map(|seen| &seen.outcome);
        let changed = jobs::record_check(
            &self.db,
            job.id,
            next.as_str(),
            runner,
            job.status.as_str(),
            outcome,
            self.now,
        )
        .await
        .map_err(text)?;
        let closed = changed == 1 && next == Status::Completed;
        if closed {
            console_log!("reconcile: job {} completed on GitHub", job.id);
            if let Some(sync::Seen {
                outcome:
                    Outcome {
                        started_at: Some(started),
                        completed_at: Some(completed),
                        ..
                    },
                ..
            }) = seen
            {
                usage::record(&self.db, job.id, *started, *completed)
                    .await
                    .map_err(text)?;
            }
        }
        Ok(closed)
    }

    /// Step 2, continued: fetches the details of runs the webhook could
    /// not, oldest first.
    pub(super) async fn details(&self) -> Fallible<Summary> {
        let before = self.now.saturating_sub(DETAILS_AFTER);
        let runs =
            jobs::runs_without_details::<RunWithoutDetails>(&self.db, before, DETAILS_PER_PASS)
                .await
                .map_err(text)?;
        let done = Summary::default();
        if runs.is_empty() {
            return Ok(done);
        }
        let ids: Vec<u64> = runs.iter().map(|run| run.id).collect();
        jobs::details_tried(&self.db, &json(&ids)?, self.now)
            .await
            .map_err(text)?;
        let calls = Counted::default();
        let tokens = self.tokens(&calls)?;
        let before = self.budget.get();
        let mut by_installation: BTreeMap<u64, Vec<RunWithoutDetails>> = BTreeMap::new();
        for run in runs {
            by_installation
                .entry(run.installation_id)
                .or_default()
                .push(run);
        }
        let fetched = self.fetch_details(&tokens, by_installation, done).await;
        self.settle(before, calls.sent());
        alerts::note_rate_limits(&self.db, &calls, self.now).await;
        fetched
    }

    /// Fetches and records the details of `runs`, by installation.
    pub(super) async fn fetch_details(
        &self,
        tokens: &Tokens<'_, Kv, App, Counted>,
        by_installation: BTreeMap<u64, Vec<RunWithoutDetails>>,
        mut done: Summary,
    ) -> Fallible<Summary> {
        for (installation_id, runs) in by_installation {
            if !self.spend(1) {
                break;
            }
            if let Err(err) = tokens.token(installation_id).await {
                console_warn!("reconcile: installation {installation_id}: {err}");
                done.errors += 1;
                continue;
            }
            for run in runs {
                if !self.spend(GITHUB_CALL_COST) {
                    break;
                }
                match tokens
                    .send(installation_id, |token| {
                        github::workflow_run(&self.github_api, token, &run.repository, run.id)
                    })
                    .await
                    .map_err(|err| err.to_string())
                    .and_then(|response| runs::parse(&response))
                {
                    Ok(details) => {
                        jobs::record_details(&self.db, run.id, &details, now_ms())
                            .await
                            .map_err(text)?;
                        done.run_details += 1;
                    }
                    Err(err) => {
                        console_warn!("reconcile: run {}: {err}", run.id);
                        done.errors += 1;
                    }
                }
            }
        }
        Ok(done)
    }
}
