// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The settings routes of `/api/ci/`: an organization's billing, and
//! the secrets and variables of an organization or a repository.
//!
//! | Route | Needs | Answer |
//! | --- | --- | --- |
//! | `GET /api/ci/orgs/{org}/billing` | org admin | [`BillingView`](crate::billing::BillingView) |
//! | `POST /api/ci/orgs/{org}/billing/checkout` | org admin, CSRF | `{"url": ...}` |
//! | `POST /api/ci/orgs/{org}/billing/portal` | org admin, CSRF | `{"url": ...}` |
//! | `PUT /api/ci/orgs/{org}/billing/cap` | org admin, CSRF | `204` |
//! | `GET /api/ci/orgs/{org}/secrets` | org admin | `[{"name", "updatedAt"}]` |
//! | `PUT, DELETE /api/ci/orgs/{org}/secrets/{name}` | org admin, CSRF | `204` |
//! | `GET /api/ci/orgs/{org}/vars` | org admin | `[{"name", "value", "updatedAt"}]` |
//! | `PUT, DELETE /api/ci/orgs/{org}/vars/{name}` | org admin, CSRF | `204` |
//!
//! The same four under `/api/ci/repos/{owner}/{repo}/` need admin
//! access to the repository. No route answers a secret's value.
//!
//! An organization with no tenant, and a user who is not an active
//! admin of it, both get `404`. `api::serve` runs the Origin, CSRF and
//! body checks of every mutating method before it calls [`handle`].

use hatchrun_cloud::ci_wake;
use hatchrun_cloud::fetch::send;
use hatchrun_cloud::polar;
use hatchrun_cloud::tenant::valid_login;
use serde::Deserialize;
use serde_json::json;
use worker::{Method, Request, Result, console_error};

use crate::access::{self, Access, Failure};
use crate::api::{Cx, Served, Stop, body_cap, name, no_content, now_ms, reply, stop};
use crate::billing::{self, Hold};
use crate::launches;
use crate::secrets::{self, Aad, Keyring};
use crate::store::{self, CiBilling, Put, TenantRow};

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
}

/// Where a settings path goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsRoute<'a> {
    /// What the organization has used and may spend.
    Billing {
        /// The organization.
        org: &'a str,
    },
    /// Start a checkout for the CI product.
    Checkout {
        /// The organization.
        org: &'a str,
    },
    /// Open the customer portal.
    Portal {
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
}

impl SettingsRoute<'_> {
    /// Whether the route answers to `method`.
    #[must_use]
    pub fn allows(&self, method: &Method) -> bool {
        match self {
            SettingsRoute::Billing { .. } | SettingsRoute::Secrets(_) | SettingsRoute::Vars(_) => {
                *method == Method::Get
            }
            SettingsRoute::Checkout { .. } | SettingsRoute::Portal { .. } => {
                *method == Method::Post
            }
            SettingsRoute::Cap { .. } => *method == Method::Put,
            SettingsRoute::Secret(..) | SettingsRoute::Var(..) => {
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
        ["orgs", org, "billing", "checkout"] => Some(SettingsRoute::Checkout { org: login(org)? }),
        ["orgs", org, "billing", "portal"] => Some(SettingsRoute::Portal { org: login(org)? }),
        ["orgs", org, "billing", "cap"] => Some(SettingsRoute::Cap { org: login(org)? }),
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
        _ => None,
    }
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
        SettingsRoute::Checkout { org } => checkout(cx, org).await,
        SettingsRoute::Portal { org } => portal(cx, org).await,
        SettingsRoute::Cap { org } => cap(cx, org, req, body_cap(&route)).await,
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

/// Polar's token and the CI product, or the `503` that says billing is
/// not set up. No checkout starts while the product id is empty.
fn polar_ready(cx: &Cx<'_>) -> Served<()> {
    if cx.settings.polar_token.is_empty() || cx.settings.polar_ci_product_id.is_empty() {
        return stop(503, "billing is not set up yet");
    }
    Ok(())
}

/// Polar's answer as an error the caller sees, with the detail kept in
/// the log.
fn polar_failed(what: &str, err: &str) -> Stop {
    console_error!("settings: {what}: {err}");
    Stop(
        502,
        "Polar did not answer; try again shortly".to_owned(),
        None,
    )
}

async fn checkout(cx: &Cx<'_>, org: &str) -> Result<Served> {
    let tenant = match admin_tenant(cx, org).await? {
        Ok(tenant) => tenant,
        Err(stop) => return Ok(Err(stop)),
    };
    if let Err(stop) = polar_ready(cx) {
        return Ok(Err(stop));
    }
    if tenant.ci_billing == CiBilling::Active {
        return Ok(stop(
            409,
            format!("{} already has a card", tenant.org_login),
        ));
    }
    let settings = cx.settings;
    let page = format!("{}/o/{}/ci", settings.app_url, tenant.org_login);
    let request = polar::checkout(
        &settings.polar_api,
        &settings.polar_token,
        std::slice::from_ref(&settings.polar_ci_product_id),
        &tenant.id,
        &tenant.org_login,
        &page,
        &page,
    );
    let url = match send(&request).await {
        Ok(response) => polar::checkout_url(&response),
        Err(err) => Err(err),
    };
    match url {
        Ok(url) => reply(200, &json!({ "url": url })).map(Ok),
        Err(err) => Ok(Err(polar_failed("checkout", &err))),
    }
}

async fn portal(cx: &Cx<'_>, org: &str) -> Result<Served> {
    let tenant = match admin_tenant(cx, org).await? {
        Ok(tenant) => tenant,
        Err(stop) => return Ok(Err(stop)),
    };
    if cx.settings.polar_token.is_empty() {
        return Ok(stop(503, "billing is not set up yet"));
    }
    if tenant.ci_billing == CiBilling::Never {
        return Ok(stop(
            409,
            format!("{} has no CI subscription", tenant.org_login),
        ));
    }
    let settings = cx.settings;
    let page = format!("{}/o/{}/ci", settings.app_url, tenant.org_login);
    let request = polar::customer_session(
        &settings.polar_api,
        &settings.polar_token,
        &tenant.id,
        &page,
    );
    let url = match send(&request).await {
        Ok(response) => polar::portal_url(&response),
        Err(err) => Err(err),
    };
    match url {
        Ok(url) => reply(200, &json!({ "url": url })).map(Ok),
        Err(err) => Ok(Err(polar_failed("portal", &err))),
    }
}

/// The body of `PUT .../billing/cap`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CapBody {
    cap_dollars: u64,
}

/// How many spend-cap holds a cap change wakes at once.
const WAKE_HELD: u32 = 5;

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
    // The jobs waiting on the old cap re-check it now. A failure here
    // leaves them to the reconciler's backstop: the cap is saved.
    match ci_wake::held(cx.db, &tenant.id, Hold::SpendCap.as_str(), WAKE_HELD).await {
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
    /// `None` for the organization's own rows.
    repo_id: Option<u64>,
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
            repo_id: None,
            label: tenant.org_login,
        })),
        Scope::Repo { owner, repo } => {
            Ok(cx
                .repo(owner, repo, Access::Admin)
                .await?
                .map(|repo| Target {
                    tenant: repo.tenant_id,
                    repo_id: Some(repo.id),
                    label: repo.full_name,
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
}

/// The value in a `PUT` body of at most `max_body` bytes. Nothing of
/// the body goes into an error.
fn parse_value(body: &[u8], max_body: usize) -> std::result::Result<String, Stop> {
    if body.len() > max_body {
        return stop(413, "the request is too large");
    }
    let Ok(ValueBody { value }) = serde_json::from_slice(body) else {
        return stop(400, "expected {\"value\": \"...\"}");
    };
    if value.is_empty() {
        return stop(400, "Values cannot be empty");
    }
    if value.len() > secrets::MAX_VALUE_BYTES {
        return stop(413, "Values are at most 48 KB");
    }
    Ok(value)
}

/// The uppercased name, or the `400` that says what is wrong with it.
fn item_name(item: &str) -> std::result::Result<String, Stop> {
    secrets::valid_name(item).or_else(|message| stop(400, message))
}

async fn list_secrets(cx: &Cx<'_>, scope: Scope<'_>) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let rows = store::secret_names(cx.db, &target.tenant, target.repo_id).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(name, updated)| json!({ "name": name, "updatedAt": updated }))
        .collect();
    reply(200, &json!(list)).map(Ok)
}

async fn list_vars(cx: &Cx<'_>, scope: Scope<'_>) -> Result<Served> {
    let target = match target(cx, scope).await? {
        Ok(target) => target,
        Err(stop) => return Ok(Err(stop)),
    };
    let rows = store::vars(cx.db, &target.tenant, target.repo_id).await?;
    let list: Vec<_> = rows
        .into_iter()
        .map(|(name, value, updated)| json!({ "name": name, "value": value, "updatedAt": updated }))
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
    let value = match parse_value(&req.bytes().await?, max_body) {
        Ok(value) => value,
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
        repo_id: target.repo_id,
        name: &name,
    };
    let sealed = secrets::seal(&keyring.current, &aad, nonce, &value);
    let max = limit(target.repo_id.is_some());
    let put = store::put_secret(
        cx.db,
        &target.tenant,
        target.repo_id,
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
    let value = match parse_value(&req.bytes().await?, max_body) {
        Ok(value) => value,
        Err(stop) => return Ok(Err(stop)),
    };
    let max = limit(target.repo_id.is_some());
    let put = store::put_var(
        cx.db,
        &target.tenant,
        target.repo_id,
        &name,
        &value,
        max,
        now_ms(),
    )
    .await?;
    saved(put, &target, Kind::Vars, max)
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
        Kind::Secrets => store::delete_secret(cx.db, &target.tenant, target.repo_id, &name).await?,
        Kind::Vars => store::delete_var(cx.db, &target.tenant, target.repo_id, &name).await?,
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

    #[test]
    fn routes_parse() {
        let org = "acme";
        assert_eq!(
            settings_route("orgs/acme/billing", &Method::Get),
            Some(SettingsRoute::Billing { org })
        );
        assert_eq!(
            settings_route("orgs/acme/billing/checkout", &Method::Post),
            Some(SettingsRoute::Checkout { org })
        );
        assert_eq!(
            settings_route("orgs/acme/billing/portal", &Method::Post),
            Some(SettingsRoute::Portal { org })
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
    fn other_paths_and_methods_have_no_route() {
        for (path, method) in [
            ("orgs/acme/billing", Method::Post),
            ("orgs/acme/billing/checkout", Method::Get),
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
    }

    #[test]
    fn value_too_large() {
        let body = format!(r#"{{"value":"{}"}}"#, "a".repeat(49_153));
        let Err(Stop(status, message, _)) = parse_value(body.as_bytes(), 128 << 10) else {
            panic!("accepted");
        };
        assert_eq!(
            (status, message.as_str()),
            (413, "Values are at most 48 KB")
        );
        let body = format!(r#"{{"value":"{}"}}"#, "a".repeat(48 * 1024));
        assert!(parse_value(body.as_bytes(), 128 << 10).is_ok());
    }

    #[test]
    fn escaped_value_fits() {
        // Each quote is two bytes in JSON: about 96 KiB on the wire.
        let body = format!(r#"{{"value":"{}"}}"#, "\\\"".repeat(48 * 1024));
        assert!(body.len() > 96 * 1024 && body.len() <= 128 << 10);
        let value = parse_value(body.as_bytes(), 128 << 10).unwrap();
        assert_eq!(value.len(), 48 * 1024);
    }

    #[test]
    fn bad_bodies_are_refused_without_echoing_them() {
        for body in [
            &br#"{"value":""}"#[..],
            b"nope",
            br#"{"value":"x","extra":1}"#,
        ] {
            let Err(Stop(status, message, _)) = parse_value(body, 128 << 10) else {
                panic!("accepted");
            };
            assert_eq!(status, 400);
            assert!(message.starts_with("expected") || message == "Values cannot be empty");
        }
        let Err(Stop(status, ..)) = parse_value(&[b' '; 200], 100) else {
            panic!("accepted");
        };
        assert_eq!(status, 413);
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
