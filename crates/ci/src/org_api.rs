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

use crate::access::{self, Failure};
use crate::api::{Cx, Served, Stop, name, no_content, now_ms, reply, run_json, stop};
use crate::billing;
use crate::installation;
use crate::page::{self, Query};
use crate::settings_api::admin_tenant;
use crate::store::{
    self, After, JobCounts, ListedRun, RepoFilter, RepoVisibility, RunFilter, RunStatus, TenantRow,
};

/// How far back the org-wide run list looks when the caller gives no
/// `since`, in milliseconds.
const DEFAULT_WINDOW_MS: u64 = 7 * 86_400_000;

/// The most entries of each list of the needs-attention answer.
const NEEDS_LIMIT: u32 = 100;

/// The most repositories one `PUT .../repos/enabled` names.
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
    let body = run_list(cx, &filter, &query).await?;
    Ok(Response::from_json(&body)?)
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

/// The body of `PUT .../repos/enabled`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledBody {
    all: bool,
    repos: Vec<String>,
}

/// The body of an enable `PUT` of at most `max_body` bytes.
fn parse_enabled(body: &[u8], max_body: usize) -> std::result::Result<EnabledBody, Stop> {
    if body.len() > max_body {
        return stop(413, "the request is too large");
    }
    let Ok(asked) = serde_json::from_slice::<EnabledBody>(body) else {
        return stop(
            400,
            "expected {\"all\": false, \"repos\": [\"owner/repo\"]}",
        );
    };
    if asked.repos.len() > MAX_ENABLED {
        return stop(422, format!("at most {MAX_ENABLED} repositories"));
    }
    let bad = asked.repos.iter().find(|repo| {
        repo.split_once('/')
            .is_none_or(|(owner, name_)| name(owner).is_none() || name(name_).is_none())
    });
    if bad.is_some() {
        return stop(400, "repos are owner/name");
    }
    Ok(asked)
}

/// `PUT /orgs/{org}/repos/enabled`.
pub(crate) async fn set_enabled(
    cx: &Cx<'_>,
    org: &str,
    req: &mut Request,
    max_body: usize,
) -> Result<Served> {
    finish(set_enabled_done(cx, org, req, max_body).await)
}

async fn set_enabled_done(cx: &Cx<'_>, org: &str, req: &mut Request, max_body: usize) -> Done {
    let tenant = admin_of(cx, org).await?;
    let body = req.bytes().await?;
    let asked = parse_enabled(&body, max_body)?;
    installation::ensure(cx.env, cx.db, cx.settings, &tenant.id, now_ms()).await?;
    let unknown = store::unknown_repos(cx.db, &tenant.id, &asked.repos).await?;
    if !asked.all && !unknown.is_empty() {
        let shown: Vec<&str> = unknown.iter().take(5).map(String::as_str).collect();
        return refuse(
            422,
            format!("not repositories of the installation: {}", shown.join(", ")),
        );
    }
    store::set_enabled(cx.db, &tenant.id, asked.all, &asked.repos).await?;
    Ok(no_content()?)
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
    let bill = billing::view(&standing);
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
        "billing": { "card": bill.card, "cap_dollars": bill.cap_dollars },
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
    fn enable_bodies() {
        let ok = parse_enabled(br#"{"all":false,"repos":["acme/app"]}"#, 1000).unwrap();
        assert!(!ok.all);
        assert_eq!(ok.repos, ["acme/app"]);
        for body in [
            &br#"{"all":false}"#[..],
            br#"{"all":false,"repos":["app"]}"#,
            br#"{"all":false,"repos":[],"x":1}"#,
            b"nope",
        ] {
            assert_eq!(parse_enabled(body, 1000).unwrap_err().0, 400);
        }
        assert_eq!(
            parse_enabled(br#"{"all":true,"repos":[]}"#, 5)
                .unwrap_err()
                .0,
            413
        );
    }

    #[test]
    fn offsets_page_through_a_total() {
        assert_eq!(next_offset(0, 2, 5), Some(page::encode(&[2])));
        assert_eq!(next_offset(4, 1, 5), None);
        assert_eq!(next_offset(0, 0, 0), None);
    }
}
