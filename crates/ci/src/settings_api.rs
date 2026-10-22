// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The settings routes of `/api/ci/`: an organization's billing, and
//! the secrets and variables of an organization or a repository.
//!
//! | Route | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /api/ci/orgs/{org}/billing` | org admin | [`BillingView`](crate::billing::BillingView) |
//! | `PUT /api/ci/orgs/{org}/billing/cap` | org admin, CSRF | `204` |
//! | `GET /api/ci/orgs/{org}/secrets` | org admin | `[{"name", "updatedAt", "repositories"}]` |
//! | `PUT, DELETE /api/ci/orgs/{org}/secrets/{name}` | org admin, CSRF | `204` |
//! | `GET /api/ci/orgs/{org}/vars` | org admin | `[{"name", "value", "updatedAt", "repositories"}]` |
//! | `PUT, DELETE /api/ci/orgs/{org}/vars/{name}` | org admin, CSRF | `204` |
//! | `GET /api/ci/orgs/{org}/repositories` | org admin | `[{"name"}]` |
//! | `GET /api/ci/repos/{owner}/{repo}/environments` | repo admin | `[{"name", "branches", "approval", "updatedAt"}]` |
//! | `PUT /api/ci/repos/{owner}/{repo}/environments/{env}` | repo admin, CSRF | the environment |
//! | `DELETE /api/ci/repos/{owner}/{repo}/environments/{env}` | repo admin, CSRF | `204` |
//! | `GET /api/ci/repos/{owner}/{repo}/environments/{env}/(secrets\|vars)` | repo admin | as for an organization |
//! | `PUT, DELETE /api/ci/repos/{owner}/{repo}/environments/{env}/(secrets\|vars)/{name}` | repo admin, CSRF | `204` |
//!
//! The secret and variable routes under `/api/ci/repos/{owner}/{repo}/`
//! need admin access to the repository, as do the environment routes.
//! No route answers a secret's value.
//!
//! An organization secret or variable reaches every repository
//! (`repositories` is `null`) or the repositories named in the list.
//! Repository and environment rows have no such list.
//!
//! An organization with no tenant, and a user who is not an active
//! admin of it, both get `404`. `api::serve` runs the Origin, CSRF and
//! body checks of every mutating method before it calls [`handle`].

use hatchrun_cloud::ci_wake::{self, Woken};
use hatchrun_cloud::db::Db;
use hatchrun_cloud::tenant::valid_login;
use hatchrun_workflow::valid_environment_name;
use serde::Deserialize;
use serde_json::json;
use worker::{Method, Request, Result, console_error};

use crate::access::{self, Access, Failure};
use crate::api::{Cx, Served, Stop, body_cap, name, no_content, now_ms, reply, stop};
use crate::billing;
use crate::environments::{self, environment_json};
use crate::launches;
use crate::secrets::{self, Aad, Keyring};
use crate::store::{self, Owner, Put, TenantRow};

/// Where a secret or variable lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// An organization, for all of its repositories.
    Org(&'a str),
    /// One repository.
    Repo {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// One environment of a repository.
    Environment {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
        /// The environment.
        environment: &'a str,
    },
}

/// Where a settings path goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRoute<'a> {
    /// What the organization has used and may spend.
    Billing {
        /// The organization.
        org: &'a str,
    },
    /// Set the monthly spend cap.
    Cap {
        /// The organization.
        org: &'a str,
    },
    /// The names of a scope's secrets.
    Secrets(Scope<'a>),
    /// Set or delete one secret.
    Secret(Scope<'a>, &'a str),
    /// A scope's variables.
    Vars(Scope<'a>),
    /// Set or delete one variable.
    Var(Scope<'a>, &'a str),
    /// The organization's repositories, for choosing who a secret
    /// reaches.
    Repositories {
        /// The organization.
        org: &'a str,
    },
    /// The environments of a repository.
    Environments {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
    },
    /// Set or delete one environment.
    Environment {
        /// The owner.
        owner: &'a str,
        /// The repository.
        repo: &'a str,
        /// The environment.
        name: &'a str,
    },
}

impl SettingsRoute<'_> {
    /// Whether the route answers to `method`.
    #[must_use]
    pub fn allows(&self, method: &Method) -> bool {
        match self {
            SettingsRoute::Billing { .. }
            | SettingsRoute::Secrets(_)
            | SettingsRoute::Vars(_)
            | SettingsRoute::Repositories { .. }
            | SettingsRoute::Environments { .. } => *method == Method::Get,
            SettingsRoute::Cap { .. } => *method == Method::Put,
            SettingsRoute::Secret(..)
            | SettingsRoute::Var(..)
            | SettingsRoute::Environment { .. } => {
                matches!(method, Method::Put | Method::Delete)
            }
        }
    }
}

/// The settings route of `path`, the part after `/api/ci/`, whatever
/// its method. `None` when there is none.
#[must_use]
pub fn find(path: &str) -> Option<SettingsRoute<'_>> {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.as_slice() {
        ["orgs", org, "billing"] => Some(SettingsRoute::Billing { org: login(org)? }),
        ["orgs", org, "billing", "cap"] => Some(SettingsRoute::Cap { org: login(org)? }),
        ["orgs", org, "repositories"] => Some(SettingsRoute::Repositories { org: login(org)? }),
        ["orgs", org, kind @ ("secrets" | "vars")] => {
            let scope = Scope::Org(login(org)?);
            Some(if *kind == "secrets" {
                SettingsRoute::Secrets(scope)
            } else {
                SettingsRoute::Vars(scope)
            })
        }
        ["orgs", org, kind @ ("secrets" | "vars"), item] => {
            Some(item_route(kind, Scope::Org(login(org)?), name(item)?))
        }
        ["repos", owner, repo, kind @ ("secrets" | "vars")] => {
            let scope = Scope::Repo {
                owner: name(owner)?,
                repo: name(repo)?,
            };
            Some(if *kind == "secrets" {
                SettingsRoute::Secrets(scope)
            } else {
                SettingsRoute::Vars(scope)
            })
        }
        ["repos", owner, repo, kind @ ("secrets" | "vars"), item] => {
            let scope = Scope::Repo {
                owner: name(owner)?,
                repo: name(repo)?,
            };
            Some(item_route(kind, scope, name(item)?))
        }
        ["repos", owner, repo, "environments"] => Some(SettingsRoute::Environments {
            owner: name(owner)?,
            repo: name(repo)?,
        }),
        ["repos", owner, repo, "environments", env] => Some(SettingsRoute::Environment {
            owner: name(owner)?,
            repo: name(repo)?,
            name: environment_name(env)?,
        }),
        [
            "repos",
            owner,
            repo,
            "environments",
            env,
            kind @ ("secrets" | "vars"),
        ] => {
            let scope = environment_scope(owner, repo, env)?;
            Some(if *kind == "secrets" {
                SettingsRoute::Secrets(scope)
            } else {
                SettingsRoute::Vars(scope)
            })
        }
        [
            "repos",
            owner,
            repo,
            "environments",
            env,
            kind @ ("secrets" | "vars"),
            item,
        ] => {
            let scope = environment_scope(owner, repo, env)?;
            Some(item_route(kind, scope, name(item)?))
        }
        _ => None,
    }
}

fn environment_name(env: &str) -> Option<&str> {
    valid_environment_name(env).is_ok().then_some(env)
}

fn environment_scope<'a>(owner: &'a str, repo: &'a str, env: &'a str) -> Option<Scope<'a>> {
    Some(Scope::Environment {
        owner: name(owner)?,
        repo: name(repo)?,
        environment: environment_name(env)?,
    })
}

fn item_route<'a>(kind: &str, scope: Scope<'a>, item: &'a str) -> SettingsRoute<'a> {
    if kind == "secrets" {
        SettingsRoute::Secret(scope, item)
    } else {
        SettingsRoute::Var(scope, item)
    }
}

fn login(org: &str) -> Option<&str> {
    valid_login(org).then_some(org)
}

/// The settings route of `path` (after `/api/ci/`) for `method`, `None`
/// when the path has none or the route does not answer to the method.
#[must_use]
pub fn settings_route<'a>(path: &'a str, method: &Method) -> Option<SettingsRoute<'a>> {
    find(path).filter(|route| route.allows(method))
}

/// Answers a settings request that passed the guards.
pub(crate) async fn handle(
    cx: &Cx<'_>,
    route: SettingsRoute<'_>,
    req: &mut Request,
) -> Result<Served> {
    match route {
        SettingsRoute::Billing { org } => billing_view(cx, org).await,
        SettingsRoute::Cap { org } => cap(cx, org, req, body_cap(&route)).await,
        SettingsRoute::Repositories { org } => repositories(cx, org).await,
        SettingsRoute::Environments { owner, repo } => list_environments(cx, owner, repo).await,
        SettingsRoute::Environment { owner, repo, name } => {
            if req.method() == Method::Put {
                put_environment(cx, owner, repo, name, req, body_cap(&route)).await
            } else {
                delete_environment(cx, owner, repo, name).await
            }
        }
        SettingsRoute::Secrets(scope) => list_secrets(cx, scope).await,
        SettingsRoute::Vars(scope) => list_vars(cx, scope).await,
        SettingsRoute::Secret(scope, item) => {
            if req.method() == Method::Put {
                put_secret(cx, scope, item, req, body_cap(&route)).await
            } else {
                delete(cx, scope, Kind::Secrets, item).await
            }
        }
        SettingsRoute::Var(scope, item) => {
            if req.method() == Method::Put {
                put_var(cx, scope, item, req, body_cap(&route)).await
            } else {
                delete(cx, scope, Kind::Vars, item).await
            }
        }
    }
}

/// The tenant of `org`, when the caller is an admin of the GitHub
/// organization. The tenant is found by the organization's id from
/// GitHub, never by the login, which a renamed organization leaves
/// behind and another can take. Anyone else sees a `404`.
async fn admin_tenant(cx: &Cx<'_>, org: &str) -> Result<Served<TenantRow>> {
    let Some(session) = cx.session else {
        return Ok(stop(401, "sign in to manage billing"));
    };
    let org_id = match access::org_admin(cx.settings, session, org).await {
        Ok(Some(org_id)) => org_id,
        Ok(None) => return Ok(stop(404, "not found")),
        Err(Failure::Retry(wait)) => {
            return Ok(Err(Stop(
                503,
                "GitHub is rate limiting; try again shortly".to_owned(),
                Some(wait),
            )));
        }
        Err(Failure::Worker(err)) => return Err(err),
    };
    match store::tenant_by_org_id(cx.db, org_id).await? {
        Some(tenant) => Ok(Ok(tenant)),
        None => Ok(stop(404, "not found")),
    }
}

async fn billing_view(cx: &Cx<'_>, org: &str) -> Result<Served> {
    let tenant = match admin_tenant(cx, org).await? {
        Ok(tenant) => tenant,
        Err(stop) => return Ok(Err(stop)),
    };
    let standing = store::tenant_standing(
        cx.db,
        &tenant.id,
        billing::month_start(now_ms()),
        hatchrun_workflow::templates::DEFAULT_SPEND_CAP_MICROS,
    )
    .await?;
    let Some(standing) = standing else {
        return Ok(stop(404, "not found"));
    };
    reply(200, &serde_json::to_value(billing::view(&standing))?).map(Ok)
}

/// The body of `PUT .../billing/cap`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CapBody {
    cap_dollars: u64,
}

async fn cap(cx: &Cx<'_>, org: &str, req: &mut Request, max_body: usize) -> Result<Served> {
    let tenant = match admin_tenant(cx, org).await? {
        Ok(tenant) => tenant,
        Err(stop) => return Ok(Err(stop)),
    };
    let body = req.bytes().await?;
    if body.len() > max_body {
        return Ok(stop(413, "the request is too large"));
    }
    let Ok(asked) = serde_json::from_slice::<CapBody>(&body) else {
        return Ok(stop(400, "expected {\"capDollars\": 75}"));
    };
    let micros = match billing::cap_micros(asked.cap_dollars) {
        Ok(micros) => micros,
        Err(message) => return Ok(stop(422, message)),
    };
    store::set_spend_cap(cx.db, &tenant.id, micros, now_ms()).await?;
    // The jobs waiting on the old cap re-check it now, the rest drain in
    // order as jobs end. A failure here leaves them to the reconciler's
    // backstop: the cap is saved.
    match ci_wake::standing_improved(cx.db, &tenant.id).await {
        Ok(woken) => {
            if let Err(err) = launches::send_wakes(cx.env, &woken).await {
                console_error!("tenant {}: wake held jobs: {err}", tenant.id);
            }
        }
        Err(err) => console_error!("tenant {}: find held jobs: {err}", tenant.id),
    }
    no_content().map(Ok)
}

/// Secrets or variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Secrets,
    Vars,
}

impl Kind {
    const fn word(self) -> &'static str {
        match self {
            Kind::Secrets => "secrets",
            Kind::Vars => "vars",
        }
    }
}

/// Whose rows a request reads or changes.
struct Target {
    tenant: String,
    owner: Owner,
    /// The organization login or `owner/repo`, for messages.
    label: String,
}

/// The rows `scope` names, when the caller may manage them: an admin of
/// the organization, or of the repository. A repository's rows belong
/// to the tenant that owns the repository, and are keyed by its id.
async fn target(cx: &Cx<'_>, scope: Scope<'_>) -> Result<Served<Target>> {
    match scope {
        Scope::Org(org) => Ok(admin_tenant(cx, org).await?.map(|tenant| Target {
            tenant: tenant.id,
            owner: Owner::Org,
            label: tenant.org_login,
        })),
        Scope::Repo { owner, repo } => {
            Ok(cx
                .repo(owner, repo, Access::Admin)
                .await?
                .map(|repo| Target {
                    tenant: repo.tenant_id,
                    owner: Owner::Repo(repo.id),
                    label: repo.full_name,
                }))
        }
        Scope::Environment {
            owner,
            repo,
            environment,
        } => {
            let repo = match cx.repo(owner, repo, Access::Admin).await? {
                Ok(repo) => repo,
                Err(stop) => return Ok(Err(stop)),
            };
            let Some(found) = store::environment(cx.db, repo.id, environment).await? else {
                return Ok(stop(404, "not found"));
            };
            Ok(Ok(Target {
                tenant: repo.tenant_id,
                owner: Owner::Environment {
                    repo_id: repo.id,
                    environment_id: found.id,
                },
                label: format!("{} environment {}", repo.full_name, found.name),
            }))
        }
    }
}

/// The message of a `409`: `label` already holds `limit` rows of `kind`.
fn too_many_message(label: &str, kind: Kind, limit: u32) -> String {
    format!("{label} has {limit} {}, the most allowed", kind.word())
}

/// The most rows of one kind a repository or an organization holds.
const fn limit(repo: bool) -> u32 {
    if repo {
        secrets::MAX_PER_REPO
    } else {
        secrets::MAX_PER_ORG
    }
}

/// The body of a secret or variable `PUT`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValueBody {
    value: String,
    /// The repositories an organization row reaches, by name; `None`
    /// for all of them.
    #[serde(default)]
    repositories: Option<Vec<String>>,
}

/// The body of a `PUT` of at most `max_body` bytes. Nothing of the body
/// goes into an error.
fn parse_body(body: &[u8], max_body: usize) -> std::result::Result<ValueBody, Stop> {
    if body.len() > max_body {
        return stop(413, "the request is too large");
    }
    let Ok(parsed) = serde_json::from_slice::<ValueBody>(body) else {
        return stop(400, "expected {\"value\": \"...\"}");
    };
    if parsed.value.is_empty() {
        return stop(400, "Values cannot be empty");
    }
    if parsed.value.len() > secrets::MAX_VALUE_BYTES {
        return stop(413, "Values are at most 48 KB");
    }
    Ok(parsed)
}

/// The `ci_repos` ids a `PUT` selects for its row. `None` stores NULL,
/// every repository. Only an organization row may select.
async fn selection(
    cx: &Cx<'_>,
    target: &Target,
    names: Option<&[String]>,
) -> Result<Served<Option<Vec<u64>>>> {
    let names = match check_selection(target.owner, names) {
        Ok(Some(names)) => names,
        Ok(None) => return Ok(Ok(None)),
        Err(stop) => return Ok(Err(stop)),
    };
    let found = store::repo_ids(cx.db, &target.tenant, names).await?;
    Ok(resolve_selection(names, &found).map(Some))
}

/// The checks that need no database: no names at all means every
/// repository, only an organization row may select, and at most
/// `MAX_SELECTED_REPOS` names.
fn check_selection(
    owner: Owner,
    names: Option<&[String]>,
) -> std::result::Result<Option<&[String]>, Stop> {
    let Some(names) = names else {
        return Ok(None);
    };
    if owner != Owner::Org {
        return stop(
            400,
            "Only organization secrets and variables have repository access",
        );
    }
    if names.len() > secrets::MAX_SELECTED_REPOS {
        return stop(422, "Select at most 1000 repositories");
    }
    Ok(Some(names))
}

/// The sorted, distinct ids of `names` among the tenant's repositories
/// `found` (name as stored, id). A name that is not there is a `422`
/// that lists each such name once, in the order given.
fn resolve_selection(
    names: &[String],
    found: &[(String, u64)],
) -> std::result::Result<Vec<u64>, Stop> {
    let mut unknown: Vec<&str> = Vec::new();
    for name in names {
        let known = found
            .iter()
            .any(|(stored, _)| stored.eq_ignore_ascii_case(name));
        let listed = unknown.iter().any(|seen| seen.eq_ignore_ascii_case(name));
        if !known && !listed {
            unknown.push(name);
        }
    }
    if !unknown.is_empty() {
        return stop(422, format!("Unknown repositories: {}", unknown.join(", ")));
    }
    let mut ids: Vec<u64> = found.iter().map(|(_, id)| *id).collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

/// The uppercased name, or the `400` that says what is wrong with it.
fn item_name(item: &str) -> std::result::Result<String, Stop> {
    secrets::valid_name(item).or_else(|message| stop(400, message))
}

async fn repositories(cx: &Cx<'_>, org: &str) -> Result<Served> {
    let tenant = match admin_tenant(cx, org).await? {
        Ok(tenant) => tenant,
        Err(stop) => return Ok(Err(stop)),
    };
    let names = store::tenant_repos(cx.db, &tenant.id).await?;
    let list: Vec<_> = names
        .into_iter()
        .map(|name| json!({ "name": name }))
        .collect();
    reply(200, &json!(list)).map(Ok)
}

async fn list_secrets(cx: &Cx<'_>, scope: Scope<'_>) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let rows = store::secret_names(cx.db, &target.tenant, target.owner).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|item| {
            json!({
                "name": item.name,
                "updatedAt": item.updated_at,
                "repositories": item.repositories,
            })
        })
        .collect();
    reply(200, &json!(list)).map(Ok)
}

async fn list_vars(cx: &Cx<'_>, scope: Scope<'_>) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let rows = store::vars(cx.db, &target.tenant, target.owner).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|item| {
            json!({
                "name": item.name,
                "value": item.value,
                "updatedAt": item.updated_at,
                "repositories": item.repositories,
            })
        })
        .collect();
    reply(200, &json!(list)).map(Ok)
}

async fn put_secret(
    cx: &Cx<'_>,
    scope: Scope<'_>,
    item: &str,
    req: &mut Request,
    max_body: usize,
) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let name = match item_name(item) {
        Ok(name) => name,
        Err(stop) => return Ok(Err(stop)),
    };
    let body = match parse_body(&req.bytes().await?, max_body) {
        Ok(body) => body,
        Err(stop) => return Ok(Err(stop)),
    };
    let repositories = match selection(cx, &target, body.repositories.as_deref()).await? {
        Ok(repositories) => repositories,
        Err(stop) => return Ok(Err(stop)),
    };
    // A keyring that does not load is the Worker's fault: 500, with the
    // binding named in the log and no key anywhere.
    let keyring = Keyring::load(cx.env)
        .await
        .map_err(worker::Error::RustError)?;
    let nonce = secrets::fresh_nonce().map_err(worker::Error::RustError)?;
    let aad = Aad {
        tenant: &target.tenant,
        owner: target.owner,
        name: &name,
    };
    let sealed = secrets::seal(&keyring.current, &aad, nonce, &body.value);
    let max = limit(target.owner != Owner::Org);
    let put = store::put_secret(
        cx.db,
        &target.tenant,
        target.owner,
        repositories.as_deref(),
        &name,
        &keyring.current.id,
        &sealed,
        max,
        now_ms(),
    )
    .await?;
    saved(put, &target, Kind::Secrets, max)
}

async fn put_var(
    cx: &Cx<'_>,
    scope: Scope<'_>,
    item: &str,
    req: &mut Request,
    max_body: usize,
) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let name = match item_name(item) {
        Ok(name) => name,
        Err(stop) => return Ok(Err(stop)),
    };
    let body = match parse_body(&req.bytes().await?, max_body) {
        Ok(body) => body,
        Err(stop) => return Ok(Err(stop)),
    };
    let repositories = match selection(cx, &target, body.repositories.as_deref()).await? {
        Ok(repositories) => repositories,
        Err(stop) => return Ok(Err(stop)),
    };
    let max = limit(target.owner != Owner::Org);
    let put = store::put_var(
        cx.db,
        &target.tenant,
        target.owner,
        repositories.as_deref(),
        &name,
        &body.value,
        max,
        now_ms(),
    )
    .await?;
    saved(put, &target, Kind::Vars, max)
}

async fn list_environments(cx: &Cx<'_>, owner: &str, repo: &str) -> Result<Served> {
    let repo = match cx.repo(owner, repo, Access::Admin).await? {
        Ok(repo) => repo,
        Err(stop) => return Ok(Err(stop)),
    };
    let rows = store::environments(cx.db, repo.id).await?;
    let list: Vec<_> = rows.iter().map(environment_json).collect();
    reply(200, &json!(list)).map(Ok)
}

/// The body of `PUT .../environments/{env}`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentBody {
    branches: Vec<String>,
    approval: bool,
}

/// The body of an environment `PUT` of at most `max_body` bytes.
fn parse_environment_body(
    body: &[u8],
    max_body: usize,
) -> std::result::Result<EnvironmentBody, Stop> {
    if body.len() > max_body {
        return stop(413, "the request is too large");
    }
    let Ok(parsed) = serde_json::from_slice::<EnvironmentBody>(body) else {
        return stop(400, "expected {\"branches\": [...], \"approval\": false}");
    };
    if let Err(message) = environments::valid_branches(&parsed.branches) {
        return stop(422, message);
    }
    Ok(parsed)
}

async fn put_environment(
    cx: &Cx<'_>,
    owner: &str,
    repo: &str,
    name: &str,
    req: &mut Request,
    max_body: usize,
) -> Result<Served> {
    let repo = match cx.repo(owner, repo, Access::Admin).await? {
        Ok(repo) => repo,
        Err(stop) => return Ok(Err(stop)),
    };
    let body = match parse_environment_body(&req.bytes().await?, max_body) {
        Ok(body) => body,
        Err(stop) => return Ok(Err(stop)),
    };
    let now = now_ms();
    let saved = store::put_environment(
        cx.db,
        &repo.tenant_id,
        repo.id,
        name,
        &body.branches,
        body.approval,
        environments::MAX_PER_REPO,
        now,
    )
    .await?;
    let row = match saved {
        store::PutEnvironment::Saved(row) => row,
        store::PutEnvironment::TooMany => {
            return Ok(stop(
                409,
                format!(
                    "{} has {} environments, the most allowed",
                    repo.full_name,
                    environments::MAX_PER_REPO
                ),
            ));
        }
    };
    release_and_wake(cx, repo.id, &row.name, row.approval, false, now).await;
    reply(200, &environment_json(&row)).map(Ok)
}

async fn delete_environment(cx: &Cx<'_>, owner: &str, repo: &str, name: &str) -> Result<Served> {
    let repo = match cx.repo(owner, repo, Access::Admin).await? {
        Ok(repo) => repo,
        Err(stop) => return Ok(Err(stop)),
    };
    if !store::delete_environment(cx.db, repo.id, name).await? {
        return Ok(stop(404, "not found"));
    }
    release_and_wake(cx, repo.id, name, false, true, now_ms()).await;
    no_content().map(Ok)
}

/// Whether a change to an environment frees the jobs that wait for its
/// approval: deleting it does, and so does turning approval off.
const fn releases(approval_after: bool, deleted: bool) -> bool {
    deleted || !approval_after
}

/// The jobs a change to environment `name` frees, with their holds
/// cleared; none when the change keeps the approval rule.
pub(crate) async fn release_holds(
    db: &Db,
    repo_id: u64,
    name: &str,
    approval_after: bool,
    deleted: bool,
    now: u64,
) -> Result<Vec<Woken>> {
    if !releases(approval_after, deleted) {
        return Ok(Vec::new());
    }
    store::release_approval_holds(db, repo_id, name, now).await
}

/// Frees the jobs a change to environment `name` leaves without an
/// approval rule and sends them to the launch queue. The change is
/// saved already, so a failure is logged and the reconciler's backstop
/// picks the jobs up.
async fn release_and_wake(
    cx: &Cx<'_>,
    repo_id: u64,
    name: &str,
    approval_after: bool,
    deleted: bool,
    now: u64,
) {
    let woken = match release_holds(cx.db, repo_id, name, approval_after, deleted, now).await {
        Ok(woken) => woken,
        Err(err) => {
            console_error!("repo {repo_id}: release approval holds of {name}: {err}");
            return;
        }
    };
    if let Err(err) = launches::send_wakes(cx.env, &woken).await {
        console_error!("repo {repo_id}: wake jobs held for {name}: {err}");
    }
}

fn saved(put: Put, target: &Target, kind: Kind, max: u32) -> Result<Served> {
    match put {
        Put::Saved => no_content().map(Ok),
        Put::TooMany => Ok(stop(409, too_many_message(&target.label, kind, max))),
    }
}

async fn delete(cx: &Cx<'_>, scope: Scope<'_>, kind: Kind, item: &str) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let name = match item_name(item) {
        Ok(name) => name,
        Err(stop) => return Ok(Err(stop)),
    };
    let existed = match kind {
        Kind::Secrets => store::delete_secret(cx.db, &target.tenant, target.owner, &name).await?,
        Kind::Vars => store::delete_var(cx.db, &target.tenant, target.owner, &name).await?,
    };
    if existed {
        no_content().map(Ok)
    } else {
        Ok(stop(404, "not found"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn refused<T: std::fmt::Debug>(result: std::result::Result<T, Stop>) -> (u16, String) {
        let Stop(status, message, _) = result.unwrap_err();
        (status, message)
    }

    #[test]
    fn only_an_org_row_may_select() {
        for owner in [
            Owner::Repo(7),
            Owner::Environment {
                repo_id: 7,
                environment_id: 3,
            },
        ] {
            let (status, _) = refused(check_selection(owner, Some(&names(&["acme/app"]))));
            assert_eq!(status, 400);
        }
        let asked = names(&["acme/app"]);
        assert_eq!(
            check_selection(Owner::Org, Some(&asked)).unwrap(),
            Some(asked.as_slice())
        );
        // An empty list is a selection of nothing, not a refusal.
        assert_eq!(
            check_selection(Owner::Org, Some(&[])).unwrap(),
            Some(&[][..])
        );
    }

    #[test]
    fn no_list_means_all_repositories() {
        for owner in [Owner::Org, Owner::Repo(7)] {
            assert_eq!(check_selection(owner, None).unwrap(), None);
        }
    }

    #[test]
    fn more_than_the_limit_is_refused() {
        let at_limit: Vec<String> = (0..secrets::MAX_SELECTED_REPOS)
            .map(|n| format!("acme/r{n}"))
            .collect();
        assert!(check_selection(Owner::Org, Some(&at_limit)).is_ok());
        let mut over = at_limit;
        over.push("acme/extra".to_owned());
        let (status, message) = refused(check_selection(Owner::Org, Some(&over)));
        assert_eq!(
            (status, message.as_str()),
            (422, "Select at most 1000 repositories")
        );
    }

    #[test]
    fn unknown_names_are_listed_once_in_order() {
        let found = [("acme/app".to_owned(), 7)];
        let asked = names(&["zed/b", "acme/app", "acme/a", "ZED/B", "zed/b"]);
        let (status, message) = refused(resolve_selection(&asked, &found));
        assert_eq!(status, 422);
        assert_eq!(message, "Unknown repositories: zed/b, acme/a");
    }

    #[test]
    fn known_names_resolve_to_sorted_distinct_ids() {
        let found = [("acme/web".to_owned(), 9), ("acme/app".to_owned(), 7)];
        let asked = names(&["ACME/WEB", "acme/app", "acme/web"]);
        assert_eq!(resolve_selection(&asked, &found).unwrap(), [7, 9]);
    }

    #[test]
    fn an_empty_list_reaches_none() {
        assert_eq!(resolve_selection(&[], &[]).unwrap(), Vec::<u64>::new());
    }

    #[test]
    fn routes_parse() {
        let org = "acme";
        assert_eq!(
            settings_route("orgs/acme/billing", &Method::Get),
            Some(SettingsRoute::Billing { org })
        );
        assert_eq!(
            settings_route("orgs/acme/billing/cap", &Method::Put),
            Some(SettingsRoute::Cap { org })
        );
        let scope = Scope::Org(org);
        assert_eq!(
            settings_route("orgs/acme/secrets/TOKEN", &Method::Put),
            Some(SettingsRoute::Secret(scope, "TOKEN"))
        );
        assert_eq!(
            settings_route("orgs/acme/vars", &Method::Get),
            Some(SettingsRoute::Vars(scope))
        );
        assert_eq!(
            settings_route("orgs/acme/repositories", &Method::Get),
            Some(SettingsRoute::Repositories { org })
        );
        let repo = Scope::Repo {
            owner: "acme",
            repo: "api",
        };
        assert_eq!(
            settings_route("repos/acme/api/secrets/TOKEN", &Method::Delete),
            Some(SettingsRoute::Secret(repo, "TOKEN"))
        );
        assert_eq!(
            settings_route("repos/acme/api/vars", &Method::Get),
            Some(SettingsRoute::Vars(repo))
        );
    }

    #[test]
    fn environment_routes_parse() {
        assert_eq!(
            settings_route("repos/acme/api/environments", &Method::Get),
            Some(SettingsRoute::Environments {
                owner: "acme",
                repo: "api"
            })
        );
        for method in [Method::Put, Method::Delete] {
            assert_eq!(
                settings_route("repos/acme/api/environments/production", &method),
                Some(SettingsRoute::Environment {
                    owner: "acme",
                    repo: "api",
                    name: "production"
                })
            );
        }
        let scope = Scope::Environment {
            owner: "acme",
            repo: "api",
            environment: "production",
        };
        assert_eq!(
            settings_route(
                "repos/acme/api/environments/production/secrets",
                &Method::Get
            ),
            Some(SettingsRoute::Secrets(scope))
        );
        assert_eq!(
            settings_route(
                "repos/acme/api/environments/production/vars/X",
                &Method::Put
            ),
            Some(SettingsRoute::Var(scope, "X"))
        );
        assert_eq!(
            settings_route(
                "repos/acme/api/environments/production/secrets/X",
                &Method::Delete
            ),
            Some(SettingsRoute::Secret(scope, "X"))
        );
    }

    #[test]
    fn bad_environment_paths_have_no_route() {
        for (path, method) in [
            ("repos/acme/api/environments/prod%20eu", Method::Put),
            ("repos/acme/api/environments/", Method::Put),
            ("repos/acme/api/environments/production", Method::Get),
            ("repos/acme/api/environments", Method::Post),
            ("repos/acme/api/environments/production/other", Method::Get),
            ("repos/acme/api/environments/prod%20eu/secrets", Method::Get),
            (
                "repos/acme/api/environments/production/secrets/A/B",
                Method::Put,
            ),
        ] {
            assert_eq!(settings_route(path, &method), None, "{path}");
        }
    }

    #[test]
    fn changes_that_release_holds() {
        assert!(releases(false, false));
        assert!(releases(true, true));
        assert!(releases(false, true));
        assert!(!releases(true, false));
    }

    #[test]
    fn environment_bodies() {
        let parse = |body: &str| parse_environment_body(body.as_bytes(), 8 << 10);
        let ok = parse(r#"{"branches":["main"],"approval":true}"#).unwrap();
        assert_eq!((ok.branches, ok.approval), (vec!["main".to_owned()], true));
        for body in [
            "nope",
            r#"{"branches":["main"]}"#,
            r#"{"approval":false}"#,
            r#"{"branches":[],"approval":false,"x":1}"#,
        ] {
            let (status, message) = refused(parse(body));
            assert_eq!(status, 400);
            assert_eq!(
                message,
                "expected {\"branches\": [...], \"approval\": false}"
            );
        }
        let (status, message) = refused(parse(r#"{"branches":[""],"approval":false}"#));
        assert_eq!(
            (status, message.as_str()),
            (422, "Branch patterns cannot be empty")
        );
        let (status, _) = refused(parse_environment_body(&[b' '; 200], 100));
        assert_eq!(status, 413);
    }

    #[test]
    fn other_paths_and_methods_have_no_route() {
        for (path, method) in [
            ("orgs/acme/billing", Method::Post),
            ("orgs/acme/repositories", Method::Put),
            ("orgs/acme/billing/checkout", Method::Post),
            ("orgs/acme/billing/portal", Method::Post),
            ("orgs/acme/billing/cap", Method::Post),
            ("orgs/acme/billing/cap", Method::Delete),
            ("orgs/acme/billing/other", Method::Get),
            ("orgs/-acme/billing", Method::Get),
            ("orgs//billing", Method::Get),
            ("orgs/acme", Method::Get),
            ("orgs/acme/secrets/A/B", Method::Put),
            ("orgs/acme/secrets/TOKEN", Method::Get),
            ("repos/acme/secrets/TOKEN", Method::Put),
            ("runs/1", Method::Get),
        ] {
            assert_eq!(settings_route(path, &method), None, "{path}");
        }
        // The path matches, so the caller can answer 405.
        assert!(find("orgs/acme/billing/cap").is_some());
        assert!(find("orgs/acme/billing/checkout").is_none());
    }

    #[test]
    fn value_too_large() {
        let body = format!(r#"{{"value":"{}"}}"#, "a".repeat(49_153));
        let Err(Stop(status, message, _)) = parse_body(body.as_bytes(), 128 << 10) else {
            panic!("accepted");
        };
        assert_eq!(
            (status, message.as_str()),
            (413, "Values are at most 48 KB")
        );
        let body = format!(r#"{{"value":"{}"}}"#, "a".repeat(48 * 1024));
        assert!(parse_body(body.as_bytes(), 128 << 10).is_ok());
    }

    #[test]
    fn escaped_value_fits() {
        // Each quote is two bytes in JSON: about 96 KiB on the wire.
        let body = format!(r#"{{"value":"{}"}}"#, "\\\"".repeat(48 * 1024));
        assert!(body.len() > 96 * 1024 && body.len() <= 128 << 10);
        let parsed = parse_body(body.as_bytes(), 128 << 10).unwrap();
        assert_eq!(parsed.value.len(), 48 * 1024);
    }

    #[test]
    fn bad_bodies_are_refused_without_echoing_them() {
        for body in [
            &br#"{"value":""}"#[..],
            b"nope",
            br#"{"value":"x","extra":1}"#,
        ] {
            let Err(Stop(status, message, _)) = parse_body(body, 128 << 10) else {
                panic!("accepted");
            };
            assert_eq!(status, 400);
            assert!(message.starts_with("expected") || message == "Values cannot be empty");
        }
        let Err(Stop(status, ..)) = parse_body(&[b' '; 200], 100) else {
            panic!("accepted");
        };
        assert_eq!(status, 413);
    }

    #[test]
    fn bodies_with_repositories() {
        let parse = |body: &str| parse_body(body.as_bytes(), 128 << 10);
        assert_eq!(
            parse(r#"{"value":"x","repositories":["a/b"]}"#)
                .unwrap()
                .repositories,
            Some(vec!["a/b".to_owned()])
        );
        assert_eq!(
            parse(r#"{"value":"x","repositories":null}"#)
                .unwrap()
                .repositories,
            None
        );
        assert_eq!(parse(r#"{"value":"x"}"#).unwrap().repositories, None);
        let Err(Stop(status, message, _)) = parse(r#"{"value":"x","repositories":"a/b"}"#) else {
            panic!("accepted");
        };
        assert_eq!(status, 400);
        assert!(message.starts_with("expected"));
    }

    #[test]
    fn too_many_message_names_the_scope_and_limit() {
        assert_eq!(
            too_many_message("acme", Kind::Secrets, 1000),
            "acme has 1000 secrets, the most allowed"
        );
        assert_eq!(
            too_many_message("acme/api", Kind::Vars, 100),
            "acme/api has 100 vars, the most allowed"
        );
        assert_eq!(limit(true), 100);
        assert_eq!(limit(false), 1000);
    }

    #[test]
    fn names_are_uppercased_or_refused() {
        assert_eq!(item_name("token_1").unwrap(), "TOKEN_1");
        assert!(matches!(item_name("1TOKEN"), Err(Stop(400, ..))));
        assert!(matches!(item_name("GITHUB_X"), Err(Stop(400, ..))));
    }
}
