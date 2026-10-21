// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! An organization's secrets, variables and environments (see
//! [`crate::secrets`]). Only owners reach them, as every tenant route,
//! and changes carry the CSRF token like every other write.
//!
//! `?repo=` names a repository and `?environment=` an environment, of
//! that repository or, without `repo`, of every repository; with
//! neither, a route acts on the organization's own.
//!
//! - `GET /api/tenants/<org>/secrets` lists a scope's secrets: names,
//!   who changed each and when, never a value.
//! - `PUT /api/tenants/<org>/secrets/<name>` with `{"value": "..."}`
//!   sets one; `DELETE` removes it.
//! - `GET /api/tenants/<org>/variables` lists a scope's variables with
//!   their values; `PUT` and `DELETE` on `variables/<name>` as above.
//! - `GET /api/tenants/<org>/environments` lists the environments;
//!   `POST` with `{"name", "repo"?, "branches"?, "requireApproval"?,
//!   "reviewers"?}` adds one.
//! - `PUT /api/tenants/<org>/environments/<name>?repo=` replaces its
//!   rules; `DELETE` removes it with its secrets and variables.
//! - `GET /api/tenants/<org>/repositories` lists the repositories the
//!   App's installation reaches, for the scope pickers.
//! - `GET /api/tenants/<org>/secrets/events` lists the changes, newest
//!   first, `?before=<id>` for the next page.
//!
//! Changes answer with the scope's new listing. Every one adds a
//! `secret_events` row.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use worker::{Method, Request, Response};

use super::tenants::{TenantRow, authorize};
use super::{Answer, Ctx, Failure};
use crate::fetch::Counted;
use crate::github::{self, InstalledRepository, REPOSITORIES_PER_PAGE};
use crate::secrets::store::{self, Change, EnvironmentRow, Kind};
use crate::secrets::{self, Keys, NONCE_BYTES, Rules, Scope, WebCrypto};
use crate::tokens::{App, Kv, Tokens};
use crate::{now_ms, webcrypto};

/// Events in a page, unless asked for fewer.
const EVENTS_PAGE: u32 = 50;
/// Most events in a page.
const EVENTS_PAGE_MAX: u32 = 100;
/// Pages of repositories read, so at most 1,000 repositories.
const REPOSITORY_PAGES_MAX: u32 = 10;

/// Answers the routes above; `rest` is the path after the
/// organization. The owner check comes first, so nothing under these
/// paths answers anyone else, not even with a 404.
pub(crate) async fn route(
    ctx: &Ctx<'_>,
    org: &str,
    method: Method,
    rest: &[&str],
    req: &mut Request,
) -> Answer {
    let row = authorize(ctx, org).await?;
    match (method, rest) {
        (Method::Get, ["secrets"]) => {
            listing(ctx, &row, Kind::Secret, &place(ctx, &row, req).await?).await
        }
        (Method::Get, ["secrets", "events"]) => events(ctx, &row, req).await,
        (Method::Put, ["secrets", name]) => {
            let scope = place(ctx, &row, req).await?;
            set_secret(ctx, &row, &scope, name, value(req).await?).await
        }
        (Method::Get, ["variables"]) => {
            listing(ctx, &row, Kind::Variable, &place(ctx, &row, req).await?).await
        }
        (Method::Put, ["variables", name]) => {
            let scope = place(ctx, &row, req).await?;
            set_variable(ctx, &row, &scope, name, value(req).await?).await
        }
        (Method::Delete, [kind @ ("secrets" | "variables"), name]) => {
            let kind = if *kind == "secrets" {
                Kind::Secret
            } else {
                Kind::Variable
            };
            let scope = place(ctx, &row, req).await?;
            delete(ctx, &row, kind, &scope, name).await
        }
        (Method::Get, ["environments"]) => environments(ctx, &row).await,
        (Method::Post, ["environments"]) => create_environment(ctx, &row, body(req).await?).await,
        (Method::Put, ["environments", name]) => {
            let repo = repo(&row, &query::<Place>(req)?)?;
            update_environment(ctx, &row, repo.as_deref(), name, body(req).await?).await
        }
        (Method::Delete, ["environments", name]) => {
            let repo = repo(&row, &query::<Place>(req)?)?;
            delete_environment(ctx, &row, repo.as_deref(), name).await
        }
        (Method::Get, ["repositories"]) => repositories(ctx, &row).await,
        _ => Err(Failure::new(404, "not found")),
    }
}

fn refused(message: impl Into<String>) -> Failure {
    Failure::new(422, message)
}

/// `?repo=&environment=`; empty is unset.
#[derive(Debug, Default, Deserialize)]
struct Place {
    repo: Option<String>,
    environment: Option<String>,
}

impl Place {
    fn repo(&self) -> Option<&str> {
        self.repo.as_deref().filter(|repo| !repo.is_empty())
    }

    fn environment(&self) -> Option<&str> {
        self.environment.as_deref().filter(|name| !name.is_empty())
    }
}

fn query<T: DeserializeOwned>(req: &Request) -> Result<T, Failure> {
    req.query()
        .map_err(|err| Failure::new(400, format!("query: {err}")))
}

/// The repository `?repo=` names, checked.
fn repo(row: &TenantRow, place: &Place) -> Result<Option<String>, Failure> {
    place
        .repo()
        .map(|given| secrets::repo_name(&row.org_login, given))
        .transpose()
        .map_err(refused)
}

/// The scope the query names. An environment must exist.
async fn place(ctx: &Ctx<'_>, row: &TenantRow, req: &Request) -> Result<Scope, Failure> {
    let place = query::<Place>(req)?;
    let repo = repo(row, &place)?;
    let Some(name) = place.environment() else {
        return Ok(repo.map_or(Scope::Org, Scope::Repo));
    };
    secrets::check_environment_name(name).map_err(refused)?;
    let found = store::environment(ctx.db, &row.id, repo.as_deref(), name).await?;
    found.map(|environment| environment.scope()).ok_or_else(|| {
        Failure::new(
            404,
            match &repo {
                Some(repo) => format!("no environment {name} for repository {repo}"),
                None => format!("no environment {name} for every repository"),
            },
        )
    })
}

/// A JSON body, or 400 saying what is wrong with it.
async fn body<T: DeserializeOwned>(req: &mut Request) -> Result<T, Failure> {
    let text = req.text().await?;
    serde_json::from_str(&text).map_err(|err| Failure::new(400, format!("body: {err}")))
}

/// `{"value": "..."}`, the body of a `PUT`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Value {
    value: String,
}

impl std::fmt::Debug for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Value").finish_non_exhaustive()
    }
}

/// The value in a `PUT`'s body. The error never quotes the body, which
/// may hold a secret.
async fn value(req: &mut Request) -> Result<Value, Failure> {
    parse_value(&req.text().await?)
}

fn parse_value(text: &str) -> Result<Value, Failure> {
    serde_json::from_str(text)
        .map_err(|_| Failure::new(400, r#"send {"value": "..."} with a string value"#))
}

/// A secret as the app lists it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretView {
    name: String,
    updated_at: u64,
    updated_by: String,
}

/// A variable as the app lists it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VariableView {
    name: String,
    value: String,
    updated_at: u64,
    updated_by: String,
}

/// The scope a listing is of.
fn scope_json(scope: &Scope) -> serde_json::Value {
    serde_json::json!({
        "scope": scope.kind(),
        "repo": scope.repo(),
        "environment": scope.environment(),
        "max": scope.limit(),
    })
}

/// A scope's secrets or variables, with the scope.
async fn listing(ctx: &Ctx<'_>, row: &TenantRow, kind: Kind, scope: &Scope) -> Answer {
    let (key, list) = match kind {
        Kind::Secret => (
            "secrets",
            serde_json::to_value(secret_views(store::secrets(ctx.db, &row.id, scope).await?)),
        ),
        Kind::Variable => (
            "variables",
            serde_json::to_value(variable_views(
                store::variables(ctx.db, &row.id, scope).await?,
            )),
        ),
    };
    let mut answer = scope_json(scope);
    answer[key] = list.map_err(|err| Failure::new(500, err.to_string()))?;
    Ok(Response::from_json(&answer)?)
}

fn secret_views(rows: Vec<store::SecretRow>) -> Vec<SecretView> {
    rows.into_iter()
        .map(|row| SecretView {
            name: row.name,
            updated_at: row.updated_at,
            updated_by: row.updated_by,
        })
        .collect()
}

fn variable_views(rows: Vec<store::VariableRow>) -> Vec<VariableView> {
    rows.into_iter()
        .map(|row| VariableView {
            name: row.name,
            value: row.value,
            updated_at: row.updated_at,
            updated_by: row.updated_by,
        })
        .collect()
}

fn change<'a>(ctx: &'a Ctx<'_>) -> Change<'a> {
    Change {
        actor: &ctx.session.user.login,
        at: now_ms(),
    }
}

fn full(kind: &str, scope: &Scope) -> Failure {
    refused(format!(
        "At most {} {kind} {}",
        scope.limit(),
        scope.describe()
    ))
}

/// `PUT /api/tenants/<org>/secrets/<name>`
async fn set_secret(
    ctx: &Ctx<'_>,
    row: &TenantRow,
    scope: &Scope,
    name: &str,
    body: Value,
) -> Answer {
    secrets::check_name(name).map_err(refused)?;
    secrets::check_value(&body.value).map_err(refused)?;
    let keys = Keys::read(ctx.env)
        .await
        .map_err(|err| Failure::new(500, format!("secrets key: {err}")))?;
    let nonce: [u8; NONCE_BYTES] = webcrypto::random_bytes(NONCE_BYTES)?
        .try_into()
        .map_err(|_| Failure::new(500, "short nonce"))?;
    let aad = secrets::additional_data(&row.id, scope, name);
    let sealed = secrets::seal(&WebCrypto, &keys.current, &nonce, &aad, &body.value)
        .await
        .map_err(|err| Failure::new(500, format!("seal: {err}")))?;
    drop(body);
    if !store::set_secret(ctx.db, &row.id, scope, name, &sealed, change(ctx)).await? {
        return Err(full("secrets", scope));
    }
    listing(ctx, row, Kind::Secret, scope).await
}

/// `PUT /api/tenants/<org>/variables/<name>`
async fn set_variable(
    ctx: &Ctx<'_>,
    row: &TenantRow,
    scope: &Scope,
    name: &str,
    body: Value,
) -> Answer {
    secrets::check_name(name).map_err(refused)?;
    secrets::check_value(&body.value).map_err(refused)?;
    if !store::set_variable(ctx.db, &row.id, scope, name, &body.value, change(ctx)).await? {
        return Err(full("variables", scope));
    }
    listing(ctx, row, Kind::Variable, scope).await
}

/// `DELETE /api/tenants/<org>/{secrets,variables}/<name>`
async fn delete(ctx: &Ctx<'_>, row: &TenantRow, kind: Kind, scope: &Scope, name: &str) -> Answer {
    secrets::check_name(name).map_err(refused)?;
    if !store::delete(ctx.db, kind, &row.id, scope, name, change(ctx)).await? {
        let what = match kind {
            Kind::Secret => "secret",
            Kind::Variable => "variable",
        };
        return Err(Failure::new(
            404,
            format!("no {what} {name} {}", scope.describe()),
        ));
    }
    listing(ctx, row, kind, scope).await
}

/// An environment as the app lists it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentView {
    name: String,
    /// `None` for every repository.
    repo: Option<String>,
    branches: Vec<String>,
    require_approval: bool,
    reviewers: Vec<String>,
    created_at: u64,
    updated_at: u64,
}

impl From<EnvironmentRow> for EnvironmentView {
    fn from(row: EnvironmentRow) -> Self {
        let rules = row.rules();
        Self {
            name: row.name,
            repo: row.repo,
            branches: rules.branches,
            require_approval: rules.require_approval,
            reviewers: rules.reviewers,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

/// `GET /api/tenants/<org>/environments`
async fn environments(ctx: &Ctx<'_>, row: &TenantRow) -> Answer {
    let list: Vec<EnvironmentView> = store::environments(ctx.db, &row.id)
        .await?
        .into_iter()
        .map(EnvironmentView::from)
        .collect();
    Ok(Response::from_json(
        &serde_json::json!({ "environments": list }),
    )?)
}

/// An environment's rules as the app sends them.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RulesBody {
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    require_approval: bool,
    #[serde(default)]
    reviewers: Vec<String>,
}

impl RulesBody {
    fn checked(self) -> Result<Rules, Failure> {
        Rules {
            branches: self.branches,
            require_approval: self.require_approval,
            reviewers: self.reviewers,
        }
        .validate()
        .map_err(refused)
    }
}

/// A new environment as the app sends it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NewEnvironment {
    name: String,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    branches: Vec<String>,
    #[serde(default)]
    require_approval: bool,
    #[serde(default)]
    reviewers: Vec<String>,
}

/// `POST /api/tenants/<org>/environments`
async fn create_environment(ctx: &Ctx<'_>, row: &TenantRow, body: NewEnvironment) -> Answer {
    secrets::check_environment_name(&body.name).map_err(refused)?;
    let repo = repo(
        row,
        &Place {
            repo: body.repo,
            environment: None,
        },
    )?;
    let rules = RulesBody {
        branches: body.branches,
        require_approval: body.require_approval,
        reviewers: body.reviewers,
    }
    .checked()?;
    if !store::create_environment(
        ctx.db,
        &row.id,
        repo.as_deref(),
        &body.name,
        &rules,
        change(ctx),
    )
    .await?
    {
        return Err(Failure::new(409, format!("{} exists already", body.name)));
    }
    environments(ctx, row).await
}

/// `PUT /api/tenants/<org>/environments/<name>`
async fn update_environment(
    ctx: &Ctx<'_>,
    row: &TenantRow,
    repo: Option<&str>,
    name: &str,
    body: RulesBody,
) -> Answer {
    secrets::check_environment_name(name).map_err(refused)?;
    let rules = body.checked()?;
    if !store::update_environment(ctx.db, &row.id, repo, name, &rules, change(ctx)).await? {
        return Err(Failure::new(404, format!("no environment {name}")));
    }
    environments(ctx, row).await
}

/// `DELETE /api/tenants/<org>/environments/<name>`
async fn delete_environment(
    ctx: &Ctx<'_>,
    row: &TenantRow,
    repo: Option<&str>,
    name: &str,
) -> Answer {
    secrets::check_environment_name(name).map_err(refused)?;
    if !store::delete_environment(ctx.db, &row.id, repo, name, change(ctx)).await? {
        return Err(Failure::new(404, format!("no environment {name}")));
    }
    environments(ctx, row).await
}

/// `?before=&limit=` of the activity list.
#[derive(Debug, Default, Deserialize)]
struct EventsPage {
    before: Option<u64>,
    limit: Option<u32>,
}

/// An event as the app lists it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventView {
    id: u64,
    actor: String,
    action: String,
    kind: String,
    scope: String,
    repo: Option<String>,
    environment: Option<String>,
    name: String,
    at: u64,
}

/// `GET /api/tenants/<org>/secrets/events`
async fn events(ctx: &Ctx<'_>, row: &TenantRow, req: &Request) -> Answer {
    let page = query::<EventsPage>(req)?;
    let limit = page.limit.unwrap_or(EVENTS_PAGE).clamp(1, EVENTS_PAGE_MAX);
    let rows = store::events(ctx.db, &row.id, page.before, limit).await?;
    let next = (rows.len() == limit as usize)
        .then(|| rows.last().map(|event| event.id))
        .flatten();
    let list: Vec<EventView> = rows
        .into_iter()
        .map(|event| EventView {
            id: event.id,
            actor: event.actor,
            action: event.action,
            kind: event.kind,
            scope: event.scope,
            repo: event.repo,
            environment: event.environment,
            name: event.name,
            at: event.at,
        })
        .collect();
    Ok(Response::from_json(
        &serde_json::json!({ "events": list, "next": next }),
    )?)
}

/// `GET /api/tenants/<org>/repositories`
async fn repositories(ctx: &Ctx<'_>, row: &TenantRow) -> Answer {
    let Some(installation_id) = row.installation_id else {
        return Err(Failure::new(
            409,
            format!("Install the App on {} first", row.org_login),
        ));
    };
    let app = App::read(ctx.env).map_err(|err| Failure::new(500, err))?;
    let api = app.api.clone();
    let calls = Counted::default();
    let tokens = Tokens::new(Kv::new(ctx.env), app, &calls, now_ms());
    let mut list: Vec<InstalledRepository> = Vec::new();
    for page in 1..=REPOSITORY_PAGES_MAX {
        let answer = tokens
            .send(installation_id, |token| {
                github::installation_repositories(&api, token, page)
            })
            .await
            .map_err(|err| Failure::from(err.to_string()))?;
        let found = github::parse_installation_repositories(&answer)?;
        let done = found.repositories.len() < REPOSITORIES_PER_PAGE as usize;
        list.extend(found.repositories);
        if done || list.len() as u64 >= found.total_count {
            break;
        }
    }
    list.sort_by_key(|repository| repository.name.to_ascii_lowercase());
    let list: Vec<serde_json::Value> = list
        .into_iter()
        .map(|repository| {
            serde_json::json!({
                "name": repository.name.to_ascii_lowercase(),
                "fullName": repository.full_name,
                "private": repository.private,
                "archived": repository.archived,
            })
        })
        .collect();
    Ok(Response::from_json(
        &serde_json::json!({ "repositories": list }),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_body_is_never_quoted_back() {
        assert_eq!(
            parse_value(r#"{"value":"hunter2"}"#).unwrap().value,
            "hunter2"
        );
        for bad in [
            r#"{"value":"hunter2","extra":1}"#,
            r#"{"value":7}"#,
            r#"{"secret":"hunter2"}"#,
            "hunter2",
        ] {
            let err = parse_value(bad).unwrap_err();
            assert_eq!(err.status, 400);
            assert!(!err.message.contains("hunter2"), "{}", err.message);
        }
        let value = parse_value(r#"{"value":"hunter2"}"#).unwrap();
        assert!(!format!("{value:?}").contains("hunter2"));
    }

    #[test]
    fn secret_listings_have_no_value_field() {
        let views = secret_views(vec![store::SecretRow {
            name: "TOKEN".into(),
            updated_at: 5,
            updated_by: "octo".into(),
        }]);
        let json = serde_json::to_value(&views).unwrap();
        assert_eq!(
            json,
            serde_json::json!([{ "name": "TOKEN", "updatedAt": 5, "updatedBy": "octo" }])
        );
        let variables = variable_views(vec![store::VariableRow {
            name: "REGION".into(),
            value: "eu-west-1".into(),
            updated_at: 5,
            updated_by: "octo".into(),
        }]);
        assert_eq!(
            serde_json::to_value(&variables).unwrap()[0]["value"],
            "eu-west-1"
        );
    }

    #[test]
    fn an_empty_query_value_is_unset() {
        let place: Place =
            serde_json::from_value(serde_json::json!({"repo": "", "environment": ""})).unwrap();
        assert_eq!((place.repo(), place.environment()), (None, None));
        let place: Place =
            serde_json::from_value(serde_json::json!({"repo": "web", "environment": "prod"}))
                .unwrap();
        assert_eq!(
            (place.repo(), place.environment()),
            (Some("web"), Some("prod"))
        );
    }

    #[test]
    fn listings_name_their_scope() {
        assert_eq!(
            scope_json(&Scope::Repo("web".into())),
            serde_json::json!({"scope": "repo", "repo": "web", "environment": null, "max": 100})
        );
        assert_eq!(
            full("secrets", &Scope::Org).message,
            "At most 1000 secrets for the organization"
        );
    }

    #[test]
    fn environment_bodies_are_checked() {
        let body: RulesBody = serde_json::from_value(serde_json::json!({
            "branches": ["main"], "requireApproval": true, "reviewers": ["octo"]
        }))
        .unwrap();
        let rules = body.checked().unwrap();
        assert!(rules.require_approval);
        assert!(serde_json::from_value::<RulesBody>(serde_json::json!({"name": "x"})).is_err());
        let bad: RulesBody =
            serde_json::from_value(serde_json::json!({"reviewers": ["no such/login"]})).unwrap();
        assert_eq!(bad.checked().unwrap_err().status, 422);
    }
}
