// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Secrets, variables and environments in their tables, and the
//! `secret_events` row every change adds in the same statement.
//!
//! Listing secrets reads names and who changed them, never the sealed
//! columns. A write that adds a name checks the scope's limit after
//! locking the tenant's row, so two writes at once cannot both take the
//! last place.

use serde::Deserialize;
use serde::de::IgnoredAny;
use worker::Result;

use super::{Rules, Scope, Sealed};
use crate::db::{Db, Param, params};

/// Secrets or variables: the same keys, different values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Secret,
    Variable,
}

impl Kind {
    fn table(self) -> &'static str {
        match self {
            Kind::Secret => "secrets",
            Kind::Variable => "variables",
        }
    }

    /// The `kind` column of its events.
    fn event(self) -> &'static str {
        match self {
            Kind::Secret => "secret",
            Kind::Variable => "variable",
        }
    }

    /// The columns that hold the value.
    fn value_columns(self) -> &'static [&'static str] {
        match self {
            Kind::Secret => &["ciphertext", "nonce", "key_id"],
            Kind::Variable => &["value"],
        }
    }
}

/// The rows of one scope. Binds the tenant, the scope, the repository
/// and the environment id, the last two NULL when unset.
const IN_SCOPE: &str = "tenant_id = $1 AND scope = $2 \
    AND repo IS NOT DISTINCT FROM $3::text AND environment_id IS NOT DISTINCT FROM $4::bigint";

/// Locks the tenant's row while a write counts its scope.
const LOCK_TENANT: &str = "SELECT id FROM tenants WHERE id = $1 FOR UPDATE";

/// The four [`IN_SCOPE`] values.
fn scope_params(tenant_id: &str, scope: &Scope) -> Vec<Param> {
    params![
        tenant_id,
        scope.kind(),
        scope.repo_column(),
        scope.environment_id()
    ]
}

/// A secret as listings show it: no value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct SecretRow {
    pub name: String,
    pub updated_at: u64,
    pub updated_by: String,
}

/// A variable, with its value.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct VariableRow {
    pub name: String,
    pub value: String,
    pub updated_at: u64,
    pub updated_by: String,
}

fn list_query(kind: Kind) -> String {
    let value = match kind {
        Kind::Secret => "",
        Kind::Variable => "value, ",
    };
    format!(
        "SELECT name, {value}updated_at, updated_by FROM {} WHERE {IN_SCOPE} \
         ORDER BY lower(name)",
        kind.table()
    )
}

/// The secrets of a scope, by name.
pub(crate) async fn secrets(db: &Db, tenant_id: &str, scope: &Scope) -> Result<Vec<SecretRow>> {
    db.all(&list_query(Kind::Secret), &scope_params(tenant_id, scope))
        .await
}

/// The variables of a scope, by name.
pub(crate) async fn variables(db: &Db, tenant_id: &str, scope: &Scope) -> Result<Vec<VariableRow>> {
    db.all(&list_query(Kind::Variable), &scope_params(tenant_id, scope))
        .await
}

/// Adds a value or replaces the one with the same name, ignoring case,
/// unless that would take the scope past `$8` names, and records the
/// change. Binds the [`IN_SCOPE`] values, the name, the owner's login,
/// the time in milliseconds, the limit, the repository and environment
/// name for the event, then the value columns. Returns the name stored.
fn set_query(kind: Kind) -> String {
    let table = kind.table();
    let columns = kind.value_columns();
    let names = columns.join(", ");
    let values = (0..columns.len())
        .map(|i| format!("${}::text", 11 + i))
        .collect::<Vec<_>>()
        .join(", ");
    let updates = columns
        .iter()
        .map(|column| format!("{column} = excluded.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "WITH saved AS ( \
           INSERT INTO {table} \
             (tenant_id, scope, repo, environment_id, name, updated_by, updated_at, {names}) \
           SELECT $1, $2, $3::text, $4::bigint, $5, $6, $7, {values} \
           WHERE EXISTS (SELECT 1 FROM {table} WHERE {IN_SCOPE} AND lower(name) = lower($5)) \
             OR (SELECT count(*) FROM {table} WHERE {IN_SCOPE}) < $8 \
           ON CONFLICT (tenant_id, scope, repo, environment_id, lower(name)) DO UPDATE SET \
             name = excluded.name, updated_by = excluded.updated_by, \
             updated_at = excluded.updated_at, {updates} \
           RETURNING name), \
         event AS ( \
           INSERT INTO secret_events \
             (tenant_id, actor, action, kind, scope, repo, environment, name, at) \
           SELECT $1, $6, 'set', '{}', $2, $9::text, $10::text, name, $7 FROM saved) \
         SELECT name FROM saved",
        kind.event()
    )
}

/// Removes a value by name, ignoring case, and records it. Binds the
/// [`IN_SCOPE`] values, the name, the owner's login, the time in
/// milliseconds, and the repository and environment name for the
/// event. Returns the name removed.
fn delete_query(kind: Kind) -> String {
    format!(
        "WITH gone AS ( \
           DELETE FROM {} WHERE {IN_SCOPE} AND lower(name) = lower($5) RETURNING name), \
         event AS ( \
           INSERT INTO secret_events \
             (tenant_id, actor, action, kind, scope, repo, environment, name, at) \
           SELECT $1, $6, 'delete', '{}', $2, $8::text, $9::text, name, $7 FROM gone) \
         SELECT name FROM gone",
        kind.table(),
        kind.event()
    )
}

/// Who changes something, and when.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Change<'a> {
    /// The owner's GitHub login.
    pub actor: &'a str,
    /// Unix milliseconds.
    pub at: u64,
}

async fn set(
    db: &Db,
    kind: Kind,
    tenant_id: &str,
    scope: &Scope,
    name: &str,
    values: Vec<Param>,
    change: Change<'_>,
) -> Result<bool> {
    let mut bound = scope_params(tenant_id, scope);
    bound.extend(params![
        name,
        change.actor,
        change.at,
        scope.limit(),
        scope.repo(),
        scope.environment()
    ]);
    bound.extend(values);
    let saved: Option<IgnoredAny> = db
        .first_locked((LOCK_TENANT, &params![tenant_id]), &set_query(kind), &bound)
        .await?;
    Ok(saved.is_some())
}

/// Stores a sealed secret. `false` when the scope is full.
pub(crate) async fn set_secret(
    db: &Db,
    tenant_id: &str,
    scope: &Scope,
    name: &str,
    sealed: &Sealed,
    change: Change<'_>,
) -> Result<bool> {
    let values = params![
        sealed.ciphertext.as_str(),
        sealed.nonce.as_str(),
        sealed.key_id.as_str()
    ];
    set(db, Kind::Secret, tenant_id, scope, name, values, change).await
}

/// Stores a variable. `false` when the scope is full.
pub(crate) async fn set_variable(
    db: &Db,
    tenant_id: &str,
    scope: &Scope,
    name: &str,
    value: &str,
    change: Change<'_>,
) -> Result<bool> {
    set(
        db,
        Kind::Variable,
        tenant_id,
        scope,
        name,
        params![value],
        change,
    )
    .await
}

/// Removes a secret or variable. `false` when there was none.
pub(crate) async fn delete(
    db: &Db,
    kind: Kind,
    tenant_id: &str,
    scope: &Scope,
    name: &str,
    change: Change<'_>,
) -> Result<bool> {
    let mut bound = scope_params(tenant_id, scope);
    bound.extend(params![
        name,
        change.actor,
        change.at,
        scope.repo(),
        scope.environment()
    ]);
    let gone: Option<IgnoredAny> = db.first(&delete_query(kind), &bound).await?;
    Ok(gone.is_some())
}

/// An `environments` row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct EnvironmentRow {
    pub id: u64,
    pub repo: Option<String>,
    pub name: String,
    /// JSON array of patterns.
    pub branches: String,
    pub require_approval: bool,
    /// JSON array of logins.
    pub reviewers: String,
    pub created_at: u64,
    pub updated_at: u64,
}

impl EnvironmentRow {
    /// The rules its columns hold.
    pub(crate) fn rules(&self) -> Rules {
        Rules {
            branches: serde_json::from_str(&self.branches).unwrap_or_default(),
            require_approval: self.require_approval,
            reviewers: serde_json::from_str(&self.reviewers).unwrap_or_default(),
        }
    }

    /// The scope of its secrets and variables.
    pub(crate) fn scope(&self) -> Scope {
        Scope::Environment {
            id: self.id,
            name: self.name.clone(),
            repo: self.repo.clone(),
        }
    }
}

const ENVIRONMENT_COLUMNS: &str =
    "id, repo, name, branches, require_approval, reviewers, created_at, updated_at";

/// The `scope` column of an environment's own events: `repo` names it
/// in the statement.
const ENVIRONMENT_SCOPE: &str = "CASE WHEN repo IS NULL THEN 'org' ELSE 'repo' END";

/// A tenant's environments, those for every repository first.
fn environments_query() -> String {
    format!(
        "SELECT {ENVIRONMENT_COLUMNS} FROM environments WHERE tenant_id = $1 \
         ORDER BY repo NULLS FIRST, lower(name)"
    )
}

/// An environment by repository, NULL for every repository, and name,
/// ignoring case.
fn environment_query() -> String {
    format!(
        "SELECT {ENVIRONMENT_COLUMNS} FROM environments WHERE tenant_id = $1 \
         AND repo IS NOT DISTINCT FROM $2::text AND lower(name) = lower($3)"
    )
}

/// Adds an environment unless the name is taken. Binds the tenant, the
/// repository or NULL, the name, the branches and reviewers as JSON,
/// whether it needs approval, the time in milliseconds and the owner's
/// login.
fn create_environment_query() -> String {
    format!(
        "WITH saved AS ( \
           INSERT INTO environments (tenant_id, repo, name, branches, reviewers, \
             require_approval, created_at, updated_at) \
           VALUES ($1, $2::text, $3, $4, $5, $6, $7, $7) \
           ON CONFLICT DO NOTHING RETURNING name, repo), \
         event AS ( \
           INSERT INTO secret_events (tenant_id, actor, action, kind, scope, repo, name, at) \
           SELECT $1, $8, 'set', 'environment', {ENVIRONMENT_SCOPE}, repo, name, $7 FROM saved) \
         SELECT name FROM saved"
    )
}

/// Replaces an environment's rules. Binds as
/// [`create_environment_query`].
fn update_environment_query() -> String {
    format!(
        "WITH saved AS ( \
           UPDATE environments SET branches = $4, reviewers = $5, require_approval = $6, \
             updated_at = $7 \
           WHERE tenant_id = $1 AND repo IS NOT DISTINCT FROM $2::text \
             AND lower(name) = lower($3) \
           RETURNING name, repo), \
         event AS ( \
           INSERT INTO secret_events (tenant_id, actor, action, kind, scope, repo, name, at) \
           SELECT $1, $8, 'set', 'environment', {ENVIRONMENT_SCOPE}, repo, name, $7 FROM saved) \
         SELECT name FROM saved"
    )
}

/// Removes an environment, and with it its secrets and variables.
/// Binds the tenant, the repository or NULL, the name, the time in
/// milliseconds and the owner's login.
fn delete_environment_query() -> String {
    format!(
        "WITH gone AS ( \
           DELETE FROM environments WHERE tenant_id = $1 \
             AND repo IS NOT DISTINCT FROM $2::text AND lower(name) = lower($3) \
           RETURNING name, repo), \
         event AS ( \
           INSERT INTO secret_events (tenant_id, actor, action, kind, scope, repo, name, at) \
           SELECT $1, $5, 'delete', 'environment', {ENVIRONMENT_SCOPE}, repo, name, $4 \
           FROM gone) \
         SELECT name FROM gone"
    )
}

/// A tenant's environments.
pub(crate) async fn environments(db: &Db, tenant_id: &str) -> Result<Vec<EnvironmentRow>> {
    db.all(&environments_query(), &params![tenant_id]).await
}

/// The environment of `repo`, `None` for one that covers every
/// repository, named `name`.
pub(crate) async fn environment(
    db: &Db,
    tenant_id: &str,
    repo: Option<&str>,
    name: &str,
) -> Result<Option<EnvironmentRow>> {
    db.first(&environment_query(), &params![tenant_id, repo, name])
        .await
}

fn rules_params(
    tenant_id: &str,
    repo: Option<&str>,
    name: &str,
    rules: &Rules,
    change: Change<'_>,
) -> Result<Vec<Param>> {
    let json = |list: &Vec<String>| {
        serde_json::to_string(list).map_err(|err| worker::Error::RustError(err.to_string()))
    };
    Ok(params![
        tenant_id,
        repo,
        name,
        json(&rules.branches)?,
        json(&rules.reviewers)?,
        rules.require_approval,
        change.at,
        change.actor
    ])
}

/// Adds an environment. `false` when one of that repository has the
/// name.
pub(crate) async fn create_environment(
    db: &Db,
    tenant_id: &str,
    repo: Option<&str>,
    name: &str,
    rules: &Rules,
    change: Change<'_>,
) -> Result<bool> {
    let bound = rules_params(tenant_id, repo, name, rules, change)?;
    let saved: Option<IgnoredAny> = db.first(&create_environment_query(), &bound).await?;
    Ok(saved.is_some())
}

/// Replaces an environment's rules. `false` when there is none.
pub(crate) async fn update_environment(
    db: &Db,
    tenant_id: &str,
    repo: Option<&str>,
    name: &str,
    rules: &Rules,
    change: Change<'_>,
) -> Result<bool> {
    let bound = rules_params(tenant_id, repo, name, rules, change)?;
    let saved: Option<IgnoredAny> = db.first(&update_environment_query(), &bound).await?;
    Ok(saved.is_some())
}

/// Removes an environment with its secrets and variables. `false` when
/// there is none.
pub(crate) async fn delete_environment(
    db: &Db,
    tenant_id: &str,
    repo: Option<&str>,
    name: &str,
    change: Change<'_>,
) -> Result<bool> {
    let gone: Option<IgnoredAny> = db
        .first(
            &delete_environment_query(),
            &params![tenant_id, repo, name, change.at, change.actor],
        )
        .await?;
    Ok(gone.is_some())
}

/// A `secret_events` row.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct EventRow {
    pub id: u64,
    pub actor: String,
    pub action: String,
    pub kind: String,
    pub scope: String,
    pub repo: Option<String>,
    pub environment: Option<String>,
    pub name: String,
    pub at: u64,
}

/// A tenant's events, newest first, before event id `$2` when set, at
/// most `$3`.
const EVENTS: &str = "SELECT id, actor, action, kind, scope, repo, environment, name, at \
    FROM secret_events WHERE tenant_id = $1 AND ($2::bigint IS NULL OR id < $2::bigint) \
    ORDER BY id DESC LIMIT $3";

/// Up to `limit` events of a tenant, newest first, older than event
/// `before` when given.
pub(crate) async fn events(
    db: &Db,
    tenant_id: &str,
    before: Option<u64>,
    limit: u32,
) -> Result<Vec<EventRow>> {
    db.all(EVENTS, &params![tenant_id, before, limit]).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::tests::{Native, key};
    use crate::secrets::{additional_data, open, seal};
    use crate::testdb::{Db, block_on};

    const NOW: u64 = 1_792_140_000_000;
    const TENANT: &str = "aaaaaaaaaaaaaaaa";
    const OWNER: Change<'static> = Change {
        actor: "octo",
        at: NOW,
    };

    fn tenant(db: &Db, id: &str, org_id: u64) {
        db.run(
            "INSERT INTO tenants (id, org_id, org_login, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $4)",
            &params![id, org_id, format!("org{org_id}"), NOW],
        );
    }

    fn setup() -> Db {
        let db = Db::new();
        tenant(&db, TENANT, 1);
        db
    }

    fn repo(name: &str) -> Scope {
        Scope::Repo(name.to_owned())
    }

    fn put(db: &Db, scope: &Scope, name: &str, value: &str) -> bool {
        db.wait(set_variable(db.db(), TENANT, scope, name, value, OWNER))
            .unwrap()
    }

    fn put_secret(db: &Db, scope: &Scope, name: &str, value: &str) -> bool {
        let aad = additional_data(TENANT, scope, name);
        let sealed = block_on(seal(&Native, &key("k1", 1), &[3; 12], &aad, value)).unwrap();
        db.wait(set_secret(db.db(), TENANT, scope, name, &sealed, OWNER))
            .unwrap()
    }

    fn names<T>(rows: &[T], name: impl Fn(&T) -> &str) -> Vec<String> {
        rows.iter().map(|row| name(row).to_owned()).collect()
    }

    fn all_events(db: &Db) -> Vec<EventRow> {
        db.wait(events(db.db(), TENANT, None, 100)).unwrap()
    }

    fn create(db: &Db, repo: Option<&str>, name: &str) -> bool {
        db.wait(create_environment(
            db.db(),
            TENANT,
            repo,
            name,
            &Rules::default(),
            OWNER,
        ))
        .unwrap()
    }

    fn find(db: &Db, repo: Option<&str>, name: &str) -> Option<EnvironmentRow> {
        db.wait(environment(db.db(), TENANT, repo, name)).unwrap()
    }

    #[test]
    fn a_listing_holds_no_secret_values() {
        let db = setup();
        assert!(put_secret(&db, &Scope::Org, "NPM_TOKEN", "hunter2"));
        let listed = db.wait(secrets(db.db(), TENANT, &Scope::Org)).unwrap();
        assert_eq!(
            listed,
            [SecretRow {
                name: "NPM_TOKEN".into(),
                updated_at: NOW,
                updated_by: "octo".into()
            }]
        );
        // The statement itself reads no sealed column.
        let sql = list_query(Kind::Secret);
        for column in ["ciphertext", "nonce", "key_id", "value"] {
            assert!(!sql.contains(column), "{sql}");
        }
        let stored: String = db
            .column("SELECT ciphertext FROM secrets", &[], "ciphertext")
            .unwrap();
        assert!(!stored.contains("hunter2"));
    }

    #[test]
    fn a_stored_secret_opens_only_in_its_place() {
        let db = setup();
        let scope = repo("web");
        assert!(put_secret(&db, &scope, "TOKEN", "hunter2"));
        let row = db
            .rows("SELECT ciphertext, nonce, key_id FROM secrets", &[])
            .unwrap()
            .remove(0);
        let sealed = Sealed {
            ciphertext: row["ciphertext"].as_str().unwrap().to_owned(),
            nonce: row["nonce"].as_str().unwrap().to_owned(),
            key_id: row["key_id"].as_str().unwrap().to_owned(),
        };
        let keys = crate::secrets::Keys {
            current: key("k1", 1),
            previous: None,
        };
        let here = additional_data(TENANT, &scope, "TOKEN");
        assert_eq!(
            block_on(open(&Native, &keys, &sealed, &here)).unwrap(),
            "hunter2"
        );
        let there = additional_data(TENANT, &repo("api"), "TOKEN");
        assert!(block_on(open(&Native, &keys, &sealed, &there)).is_err());
    }

    #[test]
    fn names_ignore_case_within_a_scope() {
        let db = setup();
        assert!(put(&db, &Scope::Org, "REGISTRY", "a"));
        assert!(put(&db, &Scope::Org, "registry", "b"));
        let listed = db.wait(variables(db.db(), TENANT, &Scope::Org)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].name.as_str(), listed[0].value.as_str()),
            ("registry", "b")
        );
    }

    #[test]
    fn scopes_keep_their_own_values() {
        let db = setup();
        assert!(create(&db, Some("web"), "prod"));
        let prod = find(&db, Some("web"), "PROD").unwrap().scope();
        for (scope, value) in [
            (Scope::Org, "org"),
            (repo("web"), "web"),
            (repo("api"), "api"),
            (prod.clone(), "prod"),
        ] {
            assert!(put(&db, &scope, "REGION", value));
        }
        for (scope, value) in [
            (Scope::Org, "org"),
            (repo("web"), "web"),
            (repo("api"), "api"),
            (prod, "prod"),
        ] {
            let listed = db.wait(variables(db.db(), TENANT, &scope)).unwrap();
            assert_eq!(names(&listed, |row| &row.value), [value], "{scope:?}");
        }
        // Another tenant sees none of them.
        tenant(&db, "bbbbbbbbbbbbbbbb", 2);
        let other = db
            .wait(variables(db.db(), "bbbbbbbbbbbbbbbb", &Scope::Org))
            .unwrap();
        assert_eq!(other, []);
    }

    #[test]
    fn a_full_scope_takes_no_new_names() {
        let db = setup();
        let scope = repo("web");
        for i in 0..crate::secrets::REPO_MAX {
            assert!(put(&db, &scope, &format!("V{i}"), "x"), "V{i}");
        }
        assert!(!put(&db, &scope, "ONE_MORE", "x"));
        // Replacing one that exists still works, and other scopes have
        // their own room.
        assert!(put(&db, &scope, "v0", "y"));
        assert!(put(&db, &repo("api"), "ONE_MORE", "x"));
        assert!(put_secret(&db, &scope, "ONE_MORE", "x"));
        let count: i64 = db
            .column(
                "SELECT count(*) AS n FROM variables WHERE repo = 'web'",
                &[],
                "n",
            )
            .unwrap();
        assert_eq!(count, 100);
        // The refused write left no event.
        assert!(!all_events(&db).iter().any(|event| {
            event.kind == "variable"
                && event.name == "ONE_MORE"
                && event.repo.as_deref() == Some("web")
        }));
    }

    #[test]
    fn every_change_is_recorded_without_its_value() {
        let db = setup();
        assert!(put_secret(&db, &repo("web"), "TOKEN", "hunter2"));
        assert!(put(&db, &Scope::Org, "REGION", "eu-west-1"));
        assert!(
            db.wait(delete(
                db.db(),
                Kind::Secret,
                TENANT,
                &repo("web"),
                "token",
                Change {
                    actor: "hubot",
                    at: NOW + 1
                },
            ))
            .unwrap()
        );
        assert!(
            !db.wait(delete(
                db.db(),
                Kind::Variable,
                TENANT,
                &Scope::Org,
                "MISSING",
                OWNER
            ))
            .unwrap()
        );
        let listed = all_events(&db);
        let seen: Vec<_> = listed
            .iter()
            .map(|event| {
                (
                    event.actor.as_str(),
                    event.action.as_str(),
                    event.kind.as_str(),
                    event.scope.as_str(),
                    event.repo.as_deref(),
                    event.name.as_str(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("hubot", "delete", "secret", "repo", Some("web"), "TOKEN"),
                ("octo", "set", "variable", "org", None, "REGION"),
                ("octo", "set", "secret", "repo", Some("web"), "TOKEN"),
            ]
        );
        assert_eq!(listed[0].at, NOW + 1);
        let everything = db
            .rows("SELECT * FROM secret_events", &[])
            .unwrap()
            .iter()
            .map(|row| serde_json::to_string(row).unwrap())
            .collect::<String>();
        assert!(!everything.contains("hunter2") && !everything.contains("eu-west-1"));
        assert_eq!(db.wait(secrets(db.db(), TENANT, &repo("web"))).unwrap(), []);
    }

    #[test]
    fn environments_are_named_once_per_repository() {
        let db = setup();
        assert!(create(&db, None, "prod"));
        assert!(!create(&db, None, "PROD"));
        assert!(create(&db, Some("web"), "prod"));
        assert!(!create(&db, Some("web"), "Prod"));
        assert!(create(&db, Some("api"), "staging"));
        let listed = db.wait(environments(db.db(), TENANT)).unwrap();
        let seen: Vec<_> = listed
            .iter()
            .map(|row| (row.repo.as_deref(), row.name.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                (None, "prod"),
                (Some("api"), "staging"),
                (Some("web"), "prod")
            ]
        );
        assert!(find(&db, Some("api"), "prod").is_none());
    }

    #[test]
    fn environment_rules_round_trip_and_are_recorded() {
        let db = setup();
        assert!(create(&db, Some("web"), "prod"));
        let rules = Rules {
            branches: vec!["main".into(), "release/*".into()],
            require_approval: true,
            reviewers: vec!["octo".into()],
        };
        assert!(
            db.wait(update_environment(
                db.db(),
                TENANT,
                Some("web"),
                "PROD",
                &rules,
                OWNER
            ))
            .unwrap()
        );
        assert!(
            !db.wait(update_environment(
                db.db(),
                TENANT,
                None,
                "prod",
                &rules,
                OWNER
            ))
            .unwrap()
        );
        let row = find(&db, Some("web"), "prod").unwrap();
        assert_eq!(row.rules(), rules);
        let seen: Vec<_> = all_events(&db)
            .iter()
            .map(|event| {
                (
                    event.action.clone(),
                    event.kind.clone(),
                    event.scope.clone(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("set".into(), "environment".into(), "repo".into()),
                ("set".into(), "environment".into(), "repo".into())
            ]
        );
    }

    #[test]
    fn deleting_an_environment_takes_its_values() {
        let db = setup();
        assert!(create(&db, None, "prod"));
        let prod = find(&db, None, "prod").unwrap().scope();
        assert!(put_secret(&db, &prod, "TOKEN", "hunter2"));
        assert!(put(&db, &prod, "REGION", "eu"));
        assert!(
            db.wait(delete_environment(db.db(), TENANT, None, "Prod", OWNER))
                .unwrap()
        );
        assert!(
            !db.wait(delete_environment(db.db(), TENANT, None, "prod", OWNER))
                .unwrap()
        );
        let left: i64 = db
            .column(
                "SELECT (SELECT count(*) FROM secrets) + (SELECT count(*) FROM variables) AS n",
                &[],
                "n",
            )
            .unwrap();
        assert_eq!(left, 0);
        let last = &all_events(&db)[0];
        assert_eq!(
            (
                last.action.as_str(),
                last.kind.as_str(),
                last.scope.as_str(),
                last.name.as_str()
            ),
            ("delete", "environment", "org", "prod")
        );
    }

    #[test]
    fn events_page_newest_first() {
        let db = setup();
        for i in 0..5 {
            assert!(put(&db, &Scope::Org, &format!("V{i}"), "x"));
        }
        let first = db.wait(events(db.db(), TENANT, None, 2)).unwrap();
        assert_eq!(names(&first, |row| &row.name), ["V4", "V3"]);
        let next = db
            .wait(events(db.db(), TENANT, Some(first[1].id), 2))
            .unwrap();
        assert_eq!(names(&next, |row| &row.name), ["V2", "V1"]);
        let plan = db.plan(EVENTS, &params![TENANT, None::<u64>, 2u32]);
        assert!(plan.contains("secret_events_tenant"), "{plan}");
    }

    #[test]
    fn rows_must_match_their_scope() {
        let db = setup();
        let insert = "INSERT INTO variables \
            (tenant_id, scope, repo, environment_id, name, value, updated_at, updated_by) \
            VALUES ($1, $2, $3::text, $4::bigint, 'X', 'v', 1, 'octo')";
        for (scope, repo, environment) in [
            ("org", Some("web"), None),
            ("repo", None, None),
            ("env", None, None),
            ("env", Some("web"), Some(1u64)),
            ("team", None, None),
        ] {
            assert!(
                db.try_run(insert, &params![TENANT, scope, repo, environment])
                    .is_err(),
                "{scope} {repo:?} {environment:?}"
            );
        }
        db.run(insert, &params![TENANT, "org", None::<&str>, None::<u64>]);
        // NULLs collide like any other value.
        assert!(
            db.try_run(insert, &params![TENANT, "org", None::<&str>, None::<u64>])
                .is_err()
        );
    }
}
