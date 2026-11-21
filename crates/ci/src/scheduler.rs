// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! One shard's schedule pass: fires the `on: schedule` workflows whose
//! minutes came due for the shard's share of the schedules.
//!
//! The cron pass fans one schedule message per shard out to the
//! `hatchrun-ci-reconcile` queue, jittered over [`crate::shard`]'s
//! minute, and each message's shard fires its own buckets' schedules
//! under its schedule lease (see [`schedule_pass`]). The delivery key
//! stays `schedule:repo:workflow:cron:minute`, so one run starts per
//! workflow and minute however often a pass repeats.

use std::collections::HashMap;

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::{Cron, Event, UtcMinute};
use serde_json::json;
use worker::{Env, console_error, console_log};

use crate::env::Settings;
use crate::intake::Repo;
use crate::reconcile::Budget;
use crate::runs::{self, Config};
use crate::shard::{SHARDS, Shard};
use crate::store::{self, CiBilling, ScheduleRow};

/// The schedule budget of one shard's pass: its share of
/// [`crate::reconcile::SCHEDULE_TOTAL`].
const SCHEDULE_BUDGET: u32 = crate::reconcile::SCHEDULE_TOTAL / (SHARDS as u32);

/// The most scheduled runs one shard's pass starts.
pub const MAX_FIRES: u32 = crate::reconcile::FIRES_TOTAL / (SHARDS as u32);

/// The most schedule minutes one shard's pass fires.
pub const MAX_MINUTES: usize = 5;

/// The schedule rows read in one query; a pass reads as many pages as
/// it needs.
pub const SCHEDULE_ROWS: u32 = 1_000;

/// Subrequests of reading a repository's head and config: a token (its
/// cache and GitHub), the commit and the file.
const HEAD_COST: u32 = 4;
/// Of starting a run: its object, and the two GitHub calls of a config
/// report.
const START_COST: u32 = 3;

/// One schedule message's pass for `shard` at `now`. Skipped when
/// another pass holds the shard's schedule lease.
pub async fn schedule_pass(env: &Env, shard: Shard, now: u64) {
    let db = match Db::connect(env).await {
        Ok(db) => db,
        Err(err) => {
            console_error!("schedule: database: {err}");
            return;
        }
    };
    let first = shard.first_bucket();
    let end = shard.range().1;
    if let Err(err) = store::ensure_shard(&db, first, end, now).await {
        console_error!("schedule: ensure shard {}: {err}", shard.index());
        return;
    }
    match store::schedule_lease(&db, first, end, now, now + crate::reconcile::LEASE_MS).await {
        Ok(true) => {}
        Ok(false) => {
            console_log!(
                "schedule: skipped, an earlier pass holds the lease of shard {}",
                shard.index()
            );
            return;
        }
        Err(err) => {
            console_error!("schedule: lease of shard {}: {err}", shard.index());
            return;
        }
    }
    let errors = match Settings::read(env) {
        Ok(settings) => {
            let github = Counted::default();
            let result = fire_schedules(
                env,
                &db,
                &settings,
                &github,
                &Budget::new(SCHEDULE_BUDGET),
                now,
                shard,
            )
            .await;
            crate::alerts::note_rate_limits(&db, &github, now).await;
            result.unwrap_or_else(|err| {
                console_error!("schedule: schedules of shard {}: {err}", shard.index());
                1
            })
        }
        Err(err) => {
            console_error!("schedule: {err}");
            1
        }
    };
    let finished = worker::Date::now().as_millis();
    if let Err(err) = store::schedule_finished(&db, first, finished, errors).await {
        console_error!("schedule: finish of shard {}: {err}", shard.index());
    }
}

/// Fires the schedules of `shard`'s buckets in the minutes since the
/// shard's last pass. Returns how many repositories and fires failed.
/// Schedules are read [`SCHEDULE_ROWS`] at a time until none are left.
/// When `budget` runs out, or the pass reaches
/// [`crate::reconcile::PASS_DEADLINE_MS`], the pass stops before the
/// next fire; the next pass starts again from that minute (from the
/// oldest one when schedules were left unread), and the runs already
/// made are skipped without cost.
#[allow(clippy::too_many_lines)] // One page loop, read top to bottom.
async fn fire_schedules(
    env: &Env,
    db: &Db,
    settings: &Settings,
    github: &Counted,
    budget: &Budget,
    now: u64,
    shard: Shard,
) -> Result<u32, String> {
    let last = store::shard_last_minute(db, shard.first_bucket())
        .await
        .map_err(|e| e.to_string())?;
    let due = minutes(last, now);
    let deadline = now + crate::reconcile::PASS_DEADLINE_MS;
    let Some(&newest) = due.last() else {
        return Ok(0);
    };
    let mut failed = 0;
    let tokens = Tokens::new(
        Kv::new(env),
        App::new(
            settings.github_api.clone(),
            settings.app_id.clone(),
            settings.app_key.clone(),
        ),
        github,
        now,
    );
    // The repository, its head and its config are read once per pass.
    let mut heads: HashMap<u64, Option<Head>> = HashMap::new();
    let mut stopped_at = None;
    let mut cap = FireCap::default();
    let mut cursor: Option<ScheduleRow> = None;
    let range = shard.range();
    // Schedules are read a page at a time, in key order after the last
    // row of the page before.
    'pages: loop {
        // A pass past its deadline stops before reading more, so its
        // last minute stays pending and the next pass resumes here.
        if worker::Date::now().as_millis() >= deadline {
            stopped_at = Some(due[0]);
            break;
        }
        let rows = store::schedules_after(db, cursor.as_ref(), SCHEDULE_ROWS, Some(range))
            .await
            .map_err(|e| e.to_string())?;
        let last_page = rows.len() < SCHEDULE_ROWS as usize;
        cursor = rows.last().cloned();
        let page = fires(&rows, &due);
        for (i, fire) in page.iter().enumerate() {
            // Once the cap is full the rest of the page starts nothing,
            // so it is counted as skipped without looking up its runs.
            if cap.full() {
                cap.skip(page.len() - i);
                break;
            }
            // Pages not read yet have fired no minute, so the pass
            // resumes at the oldest one.
            let resume = if last_page { fire.minute_ms } else { due[0] };
            // A pass past its deadline stops before the lookup, so it
            // ends inside its lease and the next pass resumes here.
            if worker::Date::now().as_millis() >= deadline {
                stopped_at = Some(resume);
                break 'pages;
            }
            let made = store::run_for_delivery(db, &fire.delivery(), &fire.workflow)
                .await
                .map_err(|e| e.to_string())?;
            if made.is_some_and(|run| run.status != "queued") {
                continue;
            }
            // A pass past its deadline stops, so it ends inside its
            // lease and the next pass resumes here.
            if worker::Date::now().as_millis() >= deadline {
                stopped_at = Some(resume);
                break 'pages;
            }
            if let std::collections::hash_map::Entry::Vacant(slot) = heads.entry(fire.repo_id) {
                if !budget.take(HEAD_COST) {
                    stopped_at = Some(resume);
                    break 'pages;
                }
                let head = match head(db, settings, &tokens, fire.repo_id).await {
                    Ok(head) => head,
                    Err(err) => {
                        console_error!("reconcile: repository {}: {err}", fire.repo_id);
                        failed += 1;
                        None
                    }
                };
                slot.insert(head);
            }
            let Some(Some(head)) = heads.get(&fire.repo_id) else {
                continue;
            };
            // A head read may have taken the pass past its deadline;
            // recheck before paying for a start, without taking a slot.
            if worker::Date::now().as_millis() >= deadline {
                stopped_at = Some(resume);
                break 'pages;
            }
            if !budget.take(START_COST) {
                stopped_at = Some(resume);
                break 'pages;
            }
            // A slot is used only here, by a firing that gets to start.
            cap.start();
            if let Err(err) = start(env, db, settings, &tokens, head, fire, now).await {
                console_error!(
                    "reconcile: schedule {} of {}: {err}",
                    fire.cron,
                    head.repo.full_name
                );
                failed += 1;
            }
        }
        if last_page {
            break;
        }
        if cap.full() {
            console_error!(
                "reconcile: schedules past the limit of {MAX_FIRES} scheduled runs skipped"
            );
            break;
        }
    }
    if cap.skipped > 0 {
        console_error!(
            "reconcile: up to {} scheduled runs over the limit of {MAX_FIRES} skipped",
            cap.skipped
        );
    }
    let done = match stopped_at {
        Some(minute) => {
            console_error!("schedule: budget or deadline reached, resuming at minute {minute}");
            minute - 60_000
        }
        None => newest,
    };
    store::set_shard_last_minute(db, shard.first_bucket(), done)
        .await
        .map_err(|e| e.to_string())?;
    Ok(failed)
}

/// The schedule minutes to fire at `now`, as Unix milliseconds at the
/// start of each minute, oldest first: those after `last` up to the
/// current one, the latest [`MAX_MINUTES`] of them. Without a `last`
/// only the current minute fires.
#[must_use]
pub fn minutes(last: Option<u64>, now: u64) -> Vec<u64> {
    let current = now / 60_000 * 60_000;
    let first = match last {
        Some(last) => (last / 60_000 + 1) * 60_000,
        None => current,
    };
    let from = first.max(current.saturating_sub(60_000 * (MAX_MINUTES as u64 - 1)));
    (from..=current).step_by(60_000).collect()
}

/// One schedule firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fire {
    /// GitHub's repository id.
    pub repo_id: u64,
    /// The workflow's file name.
    pub workflow: String,
    /// The cron expression that matched.
    pub cron: String,
    /// The minute, in Unix milliseconds.
    pub minute_ms: u64,
}

impl Fire {
    /// The delivery key of the run it starts: one run per workflow and
    /// minute, however often the pass repeats.
    #[must_use]
    pub fn delivery(&self) -> String {
        format!(
            "schedule:{}:{}:{}:{}",
            self.repo_id, self.workflow, self.cron, self.minute_ms
        )
    }
}

/// The firings of `schedules` in `minutes`, oldest minute first. A cron
/// that does not parse never fires. The cap of [`MAX_FIRES`] is not
/// applied here: a firing whose run already exists costs nothing, so
/// [`FireCap`] counts the ones that start a run.
#[must_use]
pub fn fires(schedules: &[ScheduleRow], minutes: &[u64]) -> Vec<Fire> {
    let parsed: Vec<(&ScheduleRow, Cron)> = schedules
        .iter()
        .filter_map(|row| Some((row, Cron::parse(&row.cron).ok()?)))
        .collect();
    let mut fired = Vec::new();
    for &minute_ms in minutes {
        let at = UtcMinute::from_unix(minute_ms / 1000);
        for (row, cron) in &parsed {
            if !cron.matches(&at) {
                continue;
            }
            fired.push(Fire {
                repo_id: row.repo_id,
                workflow: row.workflow.clone(),
                cron: row.cron.clone(),
                minute_ms,
            });
        }
    }
    fired
}

/// Counts the firings of one pass that start a run, against
/// [`MAX_FIRES`]. A firing whose run an earlier pass made, or whose
/// repository has no head, takes no slot. A start that fails is: the
/// attempt was made and paid for.
#[derive(Default)]
struct FireCap {
    started: usize,
    skipped: usize,
}

impl FireCap {
    /// Counts a firing that is about to start a run.
    fn start(&mut self) {
        self.started += 1;
    }

    /// Counts `n` firings left unchecked because the cap is full. Some
    /// of them may already have their run, so this is an upper bound.
    fn skip(&mut self, n: usize) {
        self.skipped += n;
    }

    fn full(&self) -> bool {
        self.started >= MAX_FIRES as usize
    }
}

/// A repository ready to start scheduled runs: where it is now, the
/// head of its default branch and the config there.
struct Head {
    repo: Repo,
    tenant_id: String,
    installation_id: u64,
    git_ref: String,
    sha: String,
    config: Config,
}

/// The head of repository `repo_id`'s default branch, under the tenant
/// that owns the repository now (a transfer leaves old schedules
/// behind). `None` when the repository or its tenant is gone.
async fn head(
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    repo_id: u64,
) -> Result<Option<Head>, String> {
    let Some(row) = store::repo(db, repo_id).await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    match store::tenant(db, &row.tenant_id)
        .await
        .map_err(|e| e.to_string())?
    {
        Some(tenant) if tenant.ci_billing == CiBilling::Active => {}
        _ => return Ok(None),
    }
    // A repository that is not enabled starts no scheduled runs.
    if !store::repo_enabled(db, &row.tenant_id, row.id)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(None);
    }
    let repo = Repo {
        id: row.id,
        full_name: row.full_name,
        default_branch: row.default_branch,
        private: row.private,
    };
    let Some((sha, config)) = runs::default_head(tokens, settings, row.installation_id, &repo)
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    Ok(Some(Head {
        git_ref: format!("refs/heads/{}", repo.default_branch),
        repo,
        tenant_id: row.tenant_id,
        installation_id: row.installation_id,
        sha,
        config,
    }))
}

/// Starts the workflow for one due schedule fire at the given head and logs the runs it created.
async fn start(
    env: &Env,
    db: &Db,
    settings: &Settings,
    tokens: &Tokens<'_, Kv, App, Counted>,
    head: &Head,
    fire: &Fire,
    now: u64,
) -> Result<(), String> {
    let payload = json!({
        "schedule": fire.cron,
        "repository": { "full_name": head.repo.full_name },
    });
    let trigger = Event::Schedule {
        cron: fire.cron.clone(),
    };
    let delivery = fire.delivery();
    let ids = runs::create(
        env,
        db,
        settings,
        tokens,
        &runs::Request {
            tenant_id: &head.tenant_id,
            installation_id: head.installation_id,
            repo: &head.repo,
            delivery: &delivery,
            event_name: "schedule",
            event: &payload,
            trigger: &trigger,
            compile_ref: &head.git_ref,
            git_ref: &head.git_ref,
            sha: &head.sha,
            actor: "schedule",
            head_ref: "",
            base_ref: "",
            workflows: Some(std::slice::from_ref(&fire.workflow)),
            origin: runs::Origin::own("schedule"),
        },
        &head.config,
        now,
    )
    .await
    .map_err(|e| e.to_string())?;
    if !ids.is_empty() {
        console_log!(
            "reconcile: {} {}: started runs {ids:?}",
            head.repo.full_name,
            fire.cron
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;
    const NOW: u64 = 1_792_676_700_000; // 2026-10-22 13:45:00 UTC

    #[test]
    fn fires_each_missed_minute_up_to_five() {
        assert_eq!(minutes(Some(NOW - 2 * MIN), NOW), [NOW - MIN, NOW]);
        assert_eq!(minutes(Some(NOW), NOW), [] as [u64; 0]);
        assert_eq!(minutes(None, NOW + 7_000), [NOW]);
        assert_eq!(
            minutes(Some(NOW - 30 * MIN), NOW),
            [NOW - 4 * MIN, NOW - 3 * MIN, NOW - 2 * MIN, NOW - MIN, NOW]
        );
        // A last minute that is not on the boundary counts from the next one.
        assert_eq!(minutes(Some(NOW - MIN + 5_000), NOW), [NOW]);

        let every = |cron: &str, workflow: &str| ScheduleRow {
            repo_id: 7,
            workflow: workflow.into(),
            cron: cron.into(),
        };
        let rows = [
            every("* * * * *", "ci"),
            every("*/15 * * * *", "slow"),
            every("nonsense", "bad"),
        ];
        let due = minutes(Some(NOW - 8 * MIN), NOW);
        let fired = fires(&rows, &due);
        // 13:41 to 13:45: every minute for ci, and :45 for slow.
        assert_eq!(fired.iter().filter(|f| f.workflow == "ci").count(), 5);
        let slow: Vec<_> = fired.iter().filter(|f| f.workflow == "slow").collect();
        assert_eq!(slow.len(), 1);
        assert_eq!(slow[0].minute_ms, NOW);
        assert_eq!(
            slow[0].delivery(),
            format!("schedule:7:slow:*/15 * * * *:{NOW}")
        );
        assert!(fired.iter().all(|f| f.workflow != "bad"));
    }

    #[test]
    fn the_cap_counts_only_runs_that_start() {
        let mut cap = FireCap::default();
        // Firings whose run exists never reach the cap.
        for _ in 0..MAX_FIRES as usize - 1 {
            cap.start();
        }
        assert!(!cap.full());
        cap.start();
        assert!(cap.full());
        cap.skip(2);
        assert_eq!((cap.started, cap.skipped), (MAX_FIRES as usize, 2));
    }

    #[test]
    fn firings_are_not_capped_before_the_run_check() {
        let rows: Vec<ScheduleRow> = (0..130)
            .map(|repo_id| ScheduleRow {
                repo_id,
                workflow: "ci".into(),
                cron: "* * * * *".into(),
            })
            .collect();
        let due = minutes(Some(NOW - 2 * MIN), NOW);
        let fired = fires(&rows, &due);
        assert_eq!(fired.len(), 260);
        assert!(fired[..130].iter().all(|f| f.minute_ms == NOW - MIN));
    }
}
