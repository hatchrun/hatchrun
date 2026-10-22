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

use crate::access::{self, Failure};
use crate::api::{Cx, Served, Stop, body_cap, name, no_content, now_ms, reply, stop};
use crate::billing::{self, Hold};
use crate::launches;
use crate::store::{self, CiBilling, TenantRow};

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
        // The secrets and variables answer once their store does.
        SettingsRoute::Secrets(_)
        | SettingsRoute::Secret(..)
        | SettingsRoute::Vars(_)
        | SettingsRoute::Var(..) => Ok(stop(404, "not found")),
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
}
