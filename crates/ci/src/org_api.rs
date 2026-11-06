// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The organization-wide routes of `/api/ci/orgs/{org}/`: runs, what
//! needs attention, repositories, setup and environments.
//!
//! The lists of runs and of what needs attention are for any active
//! member of the organization, and show the repositories the caller
//! may read. That needs no GitHub call per repository: a public
//! repository is readable by anyone, and for the private ones GitHub
//! lists what the caller's token reaches through the installation, a
//! hundred repositories to the call, kept a minute per user (see
//! [`access::readable_in_installation`]). An admin of the organization
//! reads all of them. Anyone who is not a member sees a `404`.
//!
//! The repository list answers members with the same rule. The setup,
//! environment and enable routes are for admins of the organization,
//! like the settings routes next to them.

use std::collections::{HashMap, HashSet};

use hatchrun_cloud::Session;
use serde::Deserialize;
use serde_json::{Value, json};
use worker::{Request, Response, Result};

use crate::access::{self, Access, Failure};
use crate::api::{Cx, Served, Stop, access_name, name, no_content, now_ms, reply, run_json, stop};
use crate::billing;
use crate::installation;
use crate::page::{self, Query};
use crate::settings_api::{admin_tenant, audit};
use crate::store::{
    self, After, BranchAfter, JobCounts, ListedRun, PullAfter, RepoFilter, RepoRow, RepoVisibility,
    RunFilter, RunStatus, TenantRow,
};

/// How far back the org-wide run list looks when the caller gives no
/// `since`, in milliseconds.
const DEFAULT_WINDOW_MS: u64 = 7 * 86_400_000;

/// The most entries of each list of the needs-attention answer.
const NEEDS_LIMIT: u32 = 100;

/// The most repositories one `PATCH .../repos/enabled` names.
const MAX_ENABLED: usize = 5000;

/// The longest search text, in characters.
const MAX_TEXT: usize = 100;

/// Why a handler ends early.
pub(crate) enum Fail {
    /// The answer to send.
    Stop(Stop),
    /// A failure to log; the answer is a `500`.
    Worker(worker::Error),
}

impl From<Stop> for Fail {
    fn from(stop: Stop) -> Self {
        Self::Stop(stop)
    }
}

impl From<worker::Error> for Fail {
    fn from(err: worker::Error) -> Self {
        Self::Worker(err)
    }
}

impl From<Failure> for Fail {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Retry(wait) => Self::Stop(Stop(
                503,
                "GitHub is rate limiting; try again shortly".to_owned(),
                Some(wait),
            )),
            Failure::Worker(err) => Self::Worker(err),
        }
    }
}

type Done<T = Response> = std::result::Result<T, Fail>;

/// What a route handler answers to `serve`.
fn finish(done: Done) -> Result<Served> {
    match done {
        Ok(response) => Ok(Ok(response)),
        Err(Fail::Stop(stop)) => Ok(Err(stop)),
        Err(Fail::Worker(err)) => Err(err),
    }
}

fn refuse<T>(status: u16, message: impl Into<String>) -> Done<T> {
    Err(Fail::Stop(Stop(status, message.into(), None)))
}

/// The tenant of `org` and whether the caller is its admin, for an
/// active member. The tenant is found by the organization's id from
/// GitHub, as for the settings routes.
async fn member_tenant<'a>(cx: &Cx<'a>, org: &str) -> Done<(TenantRow, bool, &'a Session)> {
    let Some(session) = cx.session else {
        return refuse(401, "sign in to see the organization");
    };
    let Some((org_id, admin)) = access::org_member(cx.settings, session, org).await? else {
        return refuse(404, "not found");
    };
    match store::tenant_by_org_id(cx.db, org_id).await? {
        Some(tenant) => Ok((tenant, admin, session)),
        None => refuse(404, "not found"),
    }
}

/// The admin's tenant of `org`.
async fn admin_of(cx: &Cx<'_>, org: &str) -> Done<TenantRow> {
    Ok(admin_tenant(cx, org).await??)
}

/// The repositories of `repos` the caller may read: those that are
/// public, and those a private repository's installation lets the
/// caller's token reach (`reach`, by installation).
#[must_use]
pub fn visible<S: std::hash::BuildHasher>(
    repos: &[RepoVisibility],
    reach: &HashMap<u64, HashSet<u64, S>, S>,
) -> Vec<u64> {
    repos
        .iter()
        .filter(|repo| {
            !repo.private
                || reach
                    .get(&repo.installation_id)
                    .is_some_and(|ids| ids.contains(&repo.id))
        })
        .map(|repo| repo.id)
        .collect()
}

/// The ids of the repositories of `tenant` the caller may read. An
/// admin of the organization reads them all.
async fn readable(
    cx: &Cx<'_>,
    session: &Session,
    tenant: &TenantRow,
    admin: bool,
) -> Done<Vec<u64>> {
    let repos = store::tenant_repo_visibility(cx.db, &tenant.id).await?;
    if admin {
        return Ok(repos.iter().map(|repo| repo.id).collect());
    }
    let mut reach: HashMap<u64, HashSet<u64>> = HashMap::new();
    for repo in repos.iter().filter(|repo| repo.private) {
        if let std::collections::hash_map::Entry::Vacant(slot) = reach.entry(repo.installation_id) {
            slot.insert(
                access::readable_in_installation(session, cx.settings, repo.installation_id)
                    .await?,
            );
        }
    }
    Ok(visible(&repos, &reach))
}

/// The filters of a run list from its query string. `me` is the
/// caller's login, for `actor=me`; `window` is how far back a list with
/// no `since` looks.
///
/// # Errors
///
/// `400` naming the parameter that is not valid.
pub fn run_filter(
    query: &Query,
    me: Option<&str>,
    now: u64,
    window: u64,
) -> std::result::Result<RunFilter, Stop> {
    let mut states = Vec::new();
    for word in query.list("status") {
        let Some(state) = RunStatus::parse(&word) else {
            return stop(
                400,
                "status is queued, running, waiting, success, failure or cancelled",
            );
        };
        if !states.contains(&state) {
            states.push(state);
        }
    }
    let repos = query.list("repo");
    if repos.iter().any(|repo| {
        repo.split_once('/')
            .is_none_or(|(owner, name_)| name(owner).is_none() || name(name_).is_none())
    }) {
        return stop(400, "repo is owner/name");
    }
    let mut actors = Vec::new();
    for actor in query.list("actor") {
        if actor == "me" {
            let Some(me) = me else {
                return stop(401, "sign in to see your runs");
            };
            actors.push(me.to_owned());
        } else {
            actors.push(actor);
        }
    }
    let text = query
        .get("q")
        .map(|text| text.trim().chars().take(MAX_TEXT).collect::<String>())
        .filter(|text| !text.is_empty());
    Ok(RunFilter {
        repo_ids: Vec::new(),
        states,
        repos,
        branches: query.list("branch"),
        workflows: query.list("workflow"),
        pull: query.number("pull")?,
        actors,
        text,
        since: query
            .number("since")?
            .unwrap_or_else(|| now.saturating_sub(window)),
        until: query.number("until")?,
    })
}

/// The page of runs `filter` holds, as the list routes answer it.
pub(crate) async fn run_list(cx: &Cx<'_>, filter: &RunFilter, query: &Query) -> Done<Value> {
    let limit = query.limit()?;
    let after = match query.cursor()?.as_deref() {
        None => None,
        Some(&[created_at, id]) => Some(After { created_at, id }),
        Some(_) => return refuse(400, "the cursor is not valid"),
    };
    // One more than the page: its existence says there is a next page.
    let (mut rows, total) = store::runs_page(cx.db, filter, after, limit + 1).await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = rows
        .last()
        .filter(|_| more)
        .map(|last| page::encode(&[last.run.created_at, last.run.id]));
    let ids: Vec<u64> = rows.iter().map(|row| row.run.id).collect();
    let counts = store::job_counts(cx.db, &ids).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            run_json(
                &row.run,
                &row.repository,
                counts.get(&row.run.id).copied().unwrap_or_default(),
            )
        })
        .collect();
    Ok(page::page(&items, total, next.as_deref()))
}

/// `GET /orgs/{org}/runs`.
pub(crate) async fn runs(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Result<Served> {
    finish(runs_done(cx, org, query).await)
}

async fn runs_done(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let (tenant, admin, session) = member_tenant(cx, org).await?;
    let mut filter = run_filter(
        &query,
        Some(&session.user.login),
        now_ms(),
        DEFAULT_WINDOW_MS,
    )?;
    filter.repo_ids = readable(cx, session, &tenant, admin).await?;
    let mut body = run_list(cx, &filter, &query).await?;
    // The tabs of the page: the same filters, whatever the status.
    body["counts"] = json!(store::run_counts(cx.db, &filter).await?);
    Ok(Response::from_json(&body)?)
}

/// The buckets of an activity range: their length in milliseconds and
/// how many.
fn activity_range(word: Option<&str>) -> std::result::Result<(u64, u32), Stop> {
    match word {
        None | Some("24h") => Ok((3_600_000, 24)),
        Some("7d") => Ok((6 * 3_600_000, 28)),
        Some(_) => stop(400, "range is 24h or 7d"),
    }
}

/// `GET /orgs/{org}/activity`.
pub(crate) async fn activity(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Result<Served> {
    finish(activity_done(cx, org, query).await)
}

async fn activity_done(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let (bucket_ms, count) = activity_range(query.get("range"))?;
    let repo = query.get("repo");
    if repo.is_some_and(|repo| {
        repo.split_once('/')
            .is_none_or(|(owner, name_)| name(owner).is_none() || name(name_).is_none())
    }) {
        return refuse(400, "repo is owner/name");
    }
    let (tenant, admin, session) = member_tenant(cx, org).await?;
    let repo_ids = readable(cx, session, &tenant, admin).await?;
    // The last bucket ends at the next whole bucket, so the chart's
    // columns keep their place as time passes.
    let end = now_ms().div_ceil(bucket_ms) * bucket_ms;
    let start = end - bucket_ms * u64::from(count);
    let buckets = store::activity(cx.db, &repo_ids, repo, start, bucket_ms, count).await?;
    let (running, queued) = store::job_load(cx.db, &repo_ids, repo).await?;
    let buckets: Vec<Value> = buckets
        .iter()
        .zip(0_u64..)
        .map(|(bucket, at)| {
            json!({
                "at": start + at * bucket_ms,
                "started": bucket.started,
                "passed": bucket.passed,
                "failed": bucket.failed,
            })
        })
        .collect();
    Ok(Response::from_json(&json!({
        "bucket_ms": bucket_ms,
        "buckets": buckets,
        "concurrency": {
            "running": running,
            "queued": queued,
            "limit": hatchrun_workflow::templates::MAX_RUNNING_JOBS,
        },
    }))?)
}

/// A job that waits for the caller's approval, as `needs-you` lists it.
fn approval_json(job: &store::ApprovalJob, counts: &HashMap<u64, JobCounts>) -> Value {
    json!({
        "run": listed_json(&job.run, counts),
        "job": { "id": job.job_id, "name": job.job_name, "environment": job.environment },
        "since": job.since,
    })
}

fn listed_json(row: &ListedRun, counts: &HashMap<u64, JobCounts>) -> Value {
    run_json(
        &row.run,
        &row.repository,
        counts.get(&row.run.id).copied().unwrap_or_default(),
    )
}

/// `GET /orgs/{org}/needs-you`.
pub(crate) async fn needs_you(cx: &Cx<'_>, org: &str) -> Result<Served> {
    finish(needs_you_done(cx, org).await)
}

async fn needs_you_done(cx: &Cx<'_>, org: &str) -> Done {
    let (tenant, admin, session) = member_tenant(cx, org).await?;
    // Only an admin of the organization can approve a job.
    let approvals = if admin {
        store::approval_jobs(cx.db, &tenant.id, NEEDS_LIMIT).await?
    } else {
        Vec::new()
    };
    let ids = readable(cx, session, &tenant, admin).await?;
    let failing: Vec<ListedRun> = store::latest_on_default(cx.db, &tenant.id, &ids)
        .await?
        .into_iter()
        .filter(|row| row.run.conclusion.as_deref() == Some("failure"))
        .take(NEEDS_LIMIT as usize)
        .collect();
    let run_ids: Vec<u64> = approvals
        .iter()
        .map(|job| job.run.run.id)
        .chain(failing.iter().map(|row| row.run.id))
        .collect();
    let counts: HashMap<u64, JobCounts> = store::job_counts(cx.db, &run_ids)
        .await?
        .into_iter()
        .collect();
    let body = json!({
        "approvals": approvals.iter().map(|job| approval_json(job, &counts)).collect::<Vec<_>>(),
        "failing": failing.iter().map(|row| json!({
            "repository": row.repository,
            "branch": row.run.branch(),
            "run": listed_json(row, &counts),
        })).collect::<Vec<_>>(),
    });
    Ok(Response::from_json(&body)?)
}

/// The offset a cursor of a list that counts items holds.
fn offset(query: &Query) -> Done<u64> {
    match query.cursor()?.as_deref() {
        None => Ok(0),
        Some(&[at]) => Ok(at),
        Some(_) => refuse(400, "the cursor is not valid"),
    }
}

/// The cursor of the page that follows `len` items from `from`, out of
/// `total`.
fn next_offset(from: u64, len: usize, total: u64) -> Option<String> {
    let end = from + len as u64;
    (end < total).then(|| page::encode(&[end]))
}

/// `GET /orgs/{org}/repos`, for any active member.
pub(crate) async fn repos(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Result<Served> {
    finish(repos_done(cx, org, query).await)
}

async fn repos_done(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let (tenant, admin, session) = member_tenant(cx, org).await?;
    let limit = query.limit()?;
    let from = offset(&query)?;
    let enabled = match query.get("enabled") {
        None => None,
        Some("true") => Some(true),
        Some("false") => Some(false),
        Some(_) => return refuse(400, "enabled is true or false"),
    };
    installation::ensure(cx.env, cx.db, cx.settings, &tenant.id, now_ms()).await?;
    // A member sees the public repositories and the private ones their
    // token reaches; an admin sees all.
    let reach = if admin {
        None
    } else {
        match store::tenant_installation(cx.db, &tenant.id).await? {
            Some(installation_id) => Some(
                access::readable_in_installation(session, cx.settings, installation_id)
                    .await?
                    .into_iter()
                    .collect::<Vec<u64>>(),
            ),
            None => Some(Vec::new()),
        }
    };
    let filter = RepoFilter {
        text: query.get("q"),
        enabled,
        reach: reach.as_deref(),
    };
    let (items, total) = store::repos_page(cx.db, &tenant.id, filter, from, limit).await?;
    let next = next_offset(from, items.len(), total);
    Ok(Response::from_json(&page::page(
        &items,
        total,
        next.as_deref(),
    ))?)
}

/// The body of `PATCH .../repos/enabled`. `all` sets every repository
/// on or off; `add` and `remove` then turn the named ones on or off.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledBody {
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

/// The body of an enable `PATCH` of at most `max_body` bytes.
fn parse_enabled(body: &[u8], max_body: usize) -> std::result::Result<EnabledBody, Stop> {
    if body.len() > max_body {
        return stop(413, "the request is too large");
    }
    let Ok(asked) = serde_json::from_slice::<EnabledBody>(body) else {
        return stop(
            400,
            "expected {\"all\": false, \"add\": [\"owner/repo\"], \"remove\": [\"owner/repo\"]}",
        );
    };
    if asked.all.is_none() && asked.add.is_empty() && asked.remove.is_empty() {
        return stop(400, "name all, add or remove");
    }
    if asked.all == Some(true) && !(asked.add.is_empty() && asked.remove.is_empty()) {
        return stop(400, "all cannot be combined with add or remove");
    }
    if asked.add.len() > MAX_ENABLED || asked.remove.len() > MAX_ENABLED {
        return stop(422, format!("at most {MAX_ENABLED} repositories"));
    }
    let bad = asked.add.iter().chain(&asked.remove).find(|repo| {
        repo.split_once('/')
            .is_none_or(|(owner, name_)| name(owner).is_none() || name(name_).is_none())
    });
    if bad.is_some() {
        return stop(400, "repos are owner/name");
    }
    Ok(asked)
}

/// What the audit row of an enable change says: `all` or `none`, then
/// how many repositories were added and removed.
fn enabled_target(asked: &EnabledBody) -> String {
    let mut parts = Vec::new();
    match asked.all {
        Some(true) => parts.push("all".to_owned()),
        Some(false) => parts.push("none".to_owned()),
        None => {}
    }
    if !asked.add.is_empty() {
        parts.push(format!("+{}", asked.add.len()));
    }
    if !asked.remove.is_empty() {
        parts.push(format!("-{}", asked.remove.len()));
    }
    parts.join(" ")
}

/// `PATCH /orgs/{org}/repos/enabled`.
pub(crate) async fn patch_enabled(
    cx: &Cx<'_>,
    org: &str,
    req: &mut Request,
    max_body: usize,
) -> Result<Served> {
    finish(patch_enabled_done(cx, org, req, max_body).await)
}

async fn patch_enabled_done(cx: &Cx<'_>, org: &str, req: &mut Request, max_body: usize) -> Done {
    let tenant = admin_of(cx, org).await?;
    let body = req.bytes().await?;
    let asked = parse_enabled(&body, max_body)?;
    installation::ensure(cx.env, cx.db, cx.settings, &tenant.id, now_ms()).await?;
    let named: Vec<String> = asked.add.iter().chain(&asked.remove).cloned().collect();
    let unknown = store::unknown_repos(cx.db, &tenant.id, &named).await?;
    if !unknown.is_empty() {
        let shown: Vec<&str> = unknown.iter().take(5).map(String::as_str).collect();
        return refuse(
            422,
            format!("not repositories of the installation: {}", shown.join(", ")),
        );
    }
    // Turning a repository off while every repository runs leaves that
    // mode, or the repository would keep running.
    let all = match asked.all {
        None if !asked.remove.is_empty() => Some(false),
        other => other,
    };
    store::patch_enabled(cx.db, &tenant.id, all, &asked.add, &asked.remove).await?;
    audit(
        cx,
        &tenant.id,
        store::Action::ReposEnabled,
        "org",
        None,
        &enabled_target(&asked),
    )
    .await?;
    Ok(no_content()?)
}

/// `GET /orgs/{org}/audit`: the changes to the organization's
/// settings, newest first. The cursor holds the id of the last row
/// shown.
pub(crate) async fn audit_list(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Result<Served> {
    finish(audit_done(cx, org, query).await)
}

async fn audit_done(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let tenant = admin_of(cx, org).await?;
    let limit = query.limit()?;
    let before = match query.cursor()?.as_deref() {
        None => None,
        Some(&[id]) => Some(id),
        Some(_) => return refuse(400, "the cursor is not valid"),
    };
    let repo = query.get("repo");
    if repo.is_some_and(|repo| {
        repo.split_once('/')
            .is_none_or(|(owner, name_)| name(owner).is_none() || name(name_).is_none())
    }) {
        return refuse(400, "repo is owner/name");
    }
    // One more than the page: its existence says there is a next page.
    let (mut rows, total) = store::audit_page(cx.db, &tenant.id, repo, before, limit + 1).await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = rows
        .last()
        .filter(|_| more)
        .map(|last| page::encode(&[last.id]));
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "at": row.at,
                "actor": row.actor,
                "action": row.action,
                "scope": row.scope,
                "repo": row.repo,
                "target": row.target,
            })
        })
        .collect();
    Ok(Response::from_json(&page::page(
        &items,
        total,
        next.as_deref(),
    ))?)
}

/// `GET /orgs/{org}/setup`.
pub(crate) async fn setup(cx: &Cx<'_>, org: &str) -> Result<Served> {
    finish(setup_done(cx, org).await)
}

/// Where an organization's owners manage the App.
fn manage_url(org: &str, installation_id: Option<u64>) -> String {
    match installation_id {
        Some(id) => format!("https://github.com/organizations/{org}/settings/installations/{id}"),
        None => format!("https://github.com/organizations/{org}/settings/installations"),
    }
}

async fn setup_done(cx: &Cx<'_>, org: &str) -> Done {
    let tenant = admin_of(cx, org).await?;
    installation::ensure(cx.env, cx.db, cx.settings, &tenant.id, now_ms()).await?;
    let Some(row) = store::tenant_setup(cx.db, &tenant.id).await? else {
        return refuse(404, "not found");
    };
    let standing = store::tenant_standing(
        cx.db,
        &tenant.id,
        billing::month_start(now_ms()),
        hatchrun_workflow::templates::DEFAULT_SPEND_CAP_MICROS,
    )
    .await?;
    let Some(standing) = standing else {
        return refuse(404, "not found");
    };
    let cap_dollars = standing.cap_micros / 1_000_000;
    let first_run = row
        .first_run_id
        .zip(row.first_run_repository)
        .map(|(id, repository)| json!({ "id": id, "repository": repository }));
    let count = if row.all_repos {
        row.repos
    } else {
        row.enabled
    };
    let body = json!({
        "app": {
            "installed": row.installation_id.is_some(),
            "repos": row.repos,
            "manage_url": manage_url(&tenant.org_login, row.installation_id),
        },
        "enabled": { "all": row.all_repos, "count": count },
        "first_run": first_run,
        "billing": { "card": standing.card, "cap_dollars": cap_dollars },
    });
    Ok(reply(200, &body)?)
}

/// `GET /orgs/{org}/environments`.
pub(crate) async fn environments(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Result<Served> {
    finish(environments_done(cx, org, query).await)
}

async fn environments_done(cx: &Cx<'_>, org: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let tenant = admin_of(cx, org).await?;
    let limit = query.limit()?;
    let from = offset(&query)?;
    let (rows, total) = store::org_environments(
        cx.db,
        &tenant.id,
        query.get("q"),
        query.get("repo"),
        from,
        limit,
    )
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "repository": row.repository,
                "name": row.environment.name,
                "branches": row.environment.branches,
                "approval": row.environment.approval,
                "updatedAt": row.environment.updated_at,
                "secrets": row.secrets,
                "vars": row.vars,
            })
        })
        .collect();
    let next = next_offset(from, items.len(), total);
    Ok(Response::from_json(&page::page(
        &items,
        total,
        next.as_deref(),
    ))?)
}

/// How far back the counts of a workflow look, in milliseconds.
const WORKFLOW_WINDOW_MS: u64 = 7 * 86_400_000;

/// The repository of `owner/name`, when the caller may read it.
async fn readable_repo(cx: &Cx<'_>, owner: &str, name: &str) -> Done<RepoRow> {
    cx.repo(owner, name, Access::Read)
        .await?
        .map_err(Fail::Stop)
}

/// The newest runs `ids` name, as the run lists show them, by id.
async fn runs_by_id(cx: &Cx<'_>, ids: &[u64]) -> Done<HashMap<u64, Value>> {
    let rows = store::listed_runs(cx.db, ids).await?;
    let found: Vec<u64> = rows.iter().map(|row| row.run.id).collect();
    let counts: HashMap<u64, JobCounts> = store::job_counts(cx.db, &found)
        .await?
        .into_iter()
        .collect();
    Ok(rows
        .iter()
        .map(|row| (row.run.id, listed_json(row, &counts)))
        .collect())
}

/// `GET /repos/{owner}/{repo}`.
pub(crate) async fn repo(cx: &Cx<'_>, owner: &str, name: &str) -> Result<Served> {
    finish(repo_done(cx, owner, name).await)
}

async fn repo_done(cx: &Cx<'_>, owner: &str, name: &str) -> Done {
    let (repo, access) = cx
        .repo_access(owner, name, Access::Read)
        .await?
        .map_err(Fail::Stop)?;
    let counts = store::repo_counts(cx.db, repo.id, &repo.tenant_id).await?;
    let enabled = store::repo_enabled(cx.db, &repo.tenant_id, repo.id).await?;
    Ok(Response::from_json(&repo_body(
        &repo, enabled, &counts, access,
    ))?)
}

/// The answer of `GET /repos/{owner}/{repo}`.
fn repo_body(repo: &RepoRow, enabled: bool, counts: &store::RepoCounts, access: Access) -> Value {
    json!({
        "name": repo.full_name,
        "default_branch": repo.default_branch,
        "private": repo.private,
        "enabled": enabled,
        "html_url": format!("https://github.com/{}", repo.full_name),
        "access": access_name(access),
        "counts": counts,
    })
}

/// The cursor of a page of workflows or branches that ends at `run`.
fn after_run(cursor: Option<&[u64]>, parts: usize) -> Done<Option<&[u64]>> {
    match cursor {
        Some(found) if found.len() != parts => refuse(400, "the cursor is not valid"),
        other => Ok(other),
    }
}

/// `GET /repos/{owner}/{repo}/workflows`.
pub(crate) async fn workflows(
    cx: &Cx<'_>,
    owner: &str,
    name: &str,
    query: Option<&str>,
) -> Result<Served> {
    finish(workflows_done(cx, owner, name, query).await)
}

async fn workflows_done(cx: &Cx<'_>, owner: &str, name: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let repo = readable_repo(cx, owner, name).await?;
    let limit = query.limit()?;
    let cursor = query.cursor()?;
    let after = after_run(cursor.as_deref(), 2)?.map(|c| After {
        created_at: c[0],
        id: c[1],
    });
    let since = now_ms().saturating_sub(WORKFLOW_WINDOW_MS);
    let (mut rows, total) =
        store::workflows_page(cx.db, repo.id, &repo.tenant_id, since, after, limit + 1).await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = rows
        .last()
        .filter(|_| more)
        .map(|last| page::encode(&[last.last_at, last.last_run_id]));
    let ids: Vec<u64> = rows.iter().map(|row| row.last_run_id).collect();
    let runs = runs_by_id(cx, &ids).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "last_run": runs.get(&row.last_run_id),
                "runs_7d": row.runs_7d,
                "passed_7d": row.passed_7d,
                "failed_7d": row.failed_7d,
            })
        })
        .collect();
    Ok(Response::from_json(&page::page(
        &items,
        total,
        next.as_deref(),
    ))?)
}

/// `GET /repos/{owner}/{repo}/branches`.
pub(crate) async fn branches(
    cx: &Cx<'_>,
    owner: &str,
    name: &str,
    query: Option<&str>,
) -> Result<Served> {
    finish(branches_done(cx, owner, name, query).await)
}

async fn branches_done(cx: &Cx<'_>, owner: &str, name: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let repo = readable_repo(cx, owner, name).await?;
    let limit = query.limit()?;
    let text = search_text(&query);
    let cursor = query.cursor()?;
    let after = after_run(cursor.as_deref(), 3)?
        .map(|c| {
            if c[0] > 1 {
                return refuse(400, "the cursor is not valid");
            }
            Ok(BranchAfter {
                rank: c[0],
                run: After {
                    created_at: c[1],
                    id: c[2],
                },
            })
        })
        .transpose()?;
    let (mut rows, total) = store::branches_page(
        cx.db,
        repo.id,
        &repo.tenant_id,
        &repo.default_branch,
        text.as_deref(),
        after,
        limit + 1,
    )
    .await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = rows
        .last()
        .filter(|_| more)
        .map(|last| page::encode(&[u64::from(!last.default), last.last_at, last.last_run_id]));
    let ids: Vec<u64> = rows.iter().map(|row| row.last_run_id).collect();
    let runs = runs_by_id(cx, &ids).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "name": row.name,
                "default": row.default,
                "last_run": runs.get(&row.last_run_id),
                "runs": row.runs,
            })
        })
        .collect();
    Ok(Response::from_json(&page::page(
        &items,
        total,
        next.as_deref(),
    ))?)
}

/// The `q` of a query, trimmed and cut, `None` when empty.
fn search_text(query: &Query) -> Option<String> {
    query
        .get("q")
        .map(|text| text.trim().chars().take(MAX_TEXT).collect::<String>())
        .filter(|text| !text.is_empty())
}

/// The pull request state a `state` word names: `None` for all.
fn pull_state(word: Option<&str>) -> std::result::Result<Option<&'static str>, Stop> {
    match word {
        None | Some("open") => Ok(Some("open")),
        Some("closed") => Ok(Some("closed")),
        Some("merged") => Ok(Some("merged")),
        Some("all") => Ok(None),
        Some(_) => stop(400, "state is open, closed, merged or all"),
    }
}

/// `GET /repos/{owner}/{repo}/pulls`.
pub(crate) async fn pulls(
    cx: &Cx<'_>,
    owner: &str,
    name: &str,
    query: Option<&str>,
) -> Result<Served> {
    finish(pulls_done(cx, owner, name, query).await)
}

async fn pulls_done(cx: &Cx<'_>, owner: &str, name: &str, query: Option<&str>) -> Done {
    let query = Query::parse(query);
    let repo = readable_repo(cx, owner, name).await?;
    let limit = query.limit()?;
    let state = pull_state(query.get("state"))?;
    let text = search_text(&query);
    let cursor = query.cursor()?;
    let after = after_run(cursor.as_deref(), 2)?.map(|c| PullAfter {
        updated_at: c[0],
        number: c[1],
    });
    let (mut rows, total) = store::pulls_page(
        cx.db,
        repo.id,
        &repo.tenant_id,
        state,
        text.as_deref(),
        after,
        limit + 1,
    )
    .await?;
    let more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let next = rows
        .last()
        .filter(|_| more)
        .map(|last| page::encode(&[last.updated_at, last.number]));
    let ids: Vec<u64> = rows.iter().filter_map(|row| row.last_run_id).collect();
    let runs = runs_by_id(cx, &ids).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "number": row.number,
                "title": row.title,
                "head_ref": row.head_ref,
                "base_ref": row.base_ref,
                "author": row.author,
                "state": row.state,
                "updated_at": row.updated_at,
                "last_run": row.last_run_id.and_then(|id| runs.get(&id)),
            })
        })
        .collect();
    Ok(Response::from_json(&page::page(
        &items,
        total,
        next.as_deref(),
    ))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(query: &str, me: Option<&str>) -> std::result::Result<RunFilter, Stop> {
        run_filter(
            &Query::parse(Some(query)),
            me,
            10 * DEFAULT_WINDOW_MS,
            DEFAULT_WINDOW_MS,
        )
    }

    #[test]
    fn a_run_filter_reads_its_lists() {
        let f = filter(
            "status=failure,waiting&repo=acme/app&branch=main,dev&actor=me,bob&q=%20fix%20&until=99",
            Some("octocat"),
        )
        .unwrap();
        assert_eq!(f.states, [RunStatus::Failure, RunStatus::Waiting]);
        assert_eq!(f.repos, ["acme/app"]);
        assert_eq!(f.branches, ["main", "dev"]);
        assert_eq!(f.actors, ["octocat", "bob"]);
        assert_eq!(f.text.as_deref(), Some("fix"));
        assert_eq!(f.since, 9 * DEFAULT_WINDOW_MS);
        assert_eq!(f.until, Some(99));
    }

    #[test]
    fn workflow_and_pull_filters_are_read() {
        let f = filter("workflow=ci.yml,lint.yml&pull=12", None).unwrap();
        assert_eq!(f.workflows, ["ci.yml", "lint.yml"]);
        assert_eq!(f.pull, Some(12));
        assert!(filter("pull=x", None).is_err());
    }

    #[test]
    fn the_repository_answer_names_the_access() {
        let repo = RepoRow {
            id: 7,
            tenant_id: "t".into(),
            installation_id: 1,
            full_name: "acme/app".into(),
            default_branch: "main".into(),
            private: true,
        };
        let counts = store::RepoCounts {
            workflows: 2,
            branches: 3,
            open_pulls: 1,
        };
        for (access, name) in [
            (Access::Read, "read"),
            (Access::Write, "write"),
            (Access::Admin, "admin"),
        ] {
            let body = repo_body(&repo, true, &counts, access);
            assert_eq!(body["access"], name);
            assert_eq!(body["name"], "acme/app");
            assert_eq!(body["html_url"], "https://github.com/acme/app");
            assert_eq!(body["counts"]["open_pulls"], 1);
        }
    }

    #[test]
    fn pull_states_default_to_open() {
        assert_eq!(pull_state(None), Ok(Some("open")));
        assert_eq!(pull_state(Some("merged")), Ok(Some("merged")));
        assert_eq!(pull_state(Some("all")), Ok(None));
        assert!(pull_state(Some("draft")).is_err());
    }

    #[test]
    fn a_bad_run_filter_is_refused() {
        for query in [
            "status=done",
            "repo=acme",
            "repo=a/b/c",
            "since=x",
            "until=-1",
        ] {
            assert_eq!(filter(query, None).unwrap_err().0, 400, "{query}");
        }
        assert_eq!(filter("actor=me", None).unwrap_err().0, 401);
        assert_eq!(filter("since=5", None).unwrap().since, 5);
    }

    #[test]
    fn private_repositories_need_the_installation_to_reach_them() {
        let repo = |id, private, installation_id| RepoVisibility {
            id,
            private,
            installation_id,
        };
        let repos = [
            repo(1, false, 9),
            repo(2, true, 9),
            repo(3, true, 9),
            repo(4, true, 8),
        ];
        let reach = HashMap::from([(9, HashSet::from([2])), (8, HashSet::new())]);
        assert_eq!(visible(&repos, &reach), [1, 2]);
        assert_eq!(visible(&repos, &HashMap::new()), [1]);
    }

    #[test]
    fn activity_ranges() {
        assert_eq!(activity_range(None).unwrap(), (3_600_000, 24));
        assert_eq!(activity_range(Some("24h")).unwrap(), (3_600_000, 24));
        assert_eq!(activity_range(Some("7d")).unwrap(), (21_600_000, 28));
        assert_eq!(activity_range(Some("30d")).unwrap_err().0, 400);
    }

    #[test]
    fn enable_bodies() {
        let ok = parse_enabled(
            br#"{"all":false,"add":["acme/app"],"remove":["acme/web"]}"#,
            1000,
        )
        .unwrap();
        assert_eq!(ok.all, Some(false));
        assert_eq!(ok.add, ["acme/app"]);
        assert_eq!(ok.remove, ["acme/web"]);
        assert_eq!(enabled_target(&ok), "none +1 -1");
        let all = parse_enabled(br#"{"all":true}"#, 1000).unwrap();
        assert_eq!(enabled_target(&all), "all");
        let add = parse_enabled(br#"{"add":["acme/app"]}"#, 1000).unwrap();
        assert_eq!(add.all, None);
        for body in [
            &br"{}"[..],
            br#"{"add":[]}"#,
            br#"{"all":true,"add":["acme/app"]}"#,
            br#"{"add":["app"]}"#,
            br#"{"remove":["acme/a/b"]}"#,
            br#"{"all":false,"repos":[]}"#,
            b"nope",
        ] {
            assert_eq!(parse_enabled(body, 1000).unwrap_err().0, 400);
        }
        assert_eq!(parse_enabled(br#"{"all":true}"#, 5).unwrap_err().0, 413);
    }

    #[test]
    fn offsets_page_through_a_total() {
        assert_eq!(next_offset(0, 2, 5), Some(page::encode(&[2])));
        assert_eq!(next_offset(4, 1, 5), None);
        assert_eq!(next_offset(0, 0, 0), None);
    }
}
