// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Workflow runs, as the app lists jobs by them.
//!
//! The first hatchrun job of a run records the run from what
//! `workflow_job` carries: the workflow's name, the branch, the commit
//! and the attempt. The run's number, event, title and actor are only
//! on the run itself, so the Worker fetches it from GitHub once, after
//! answering the webhook; the reconciler fetches the runs that fetch
//! missed.
//!
//! Nothing subscribes to `workflow_run`. It fires for every run of
//! every repository the App is on, most of them without a hatchrun job,
//! and its state covers jobs on other runners that hatchrun does not
//! record. A run's state in the app rolls up from its recorded jobs.

use serde::Deserialize;
use wasm_bindgen::JsValue;
use worker::{D1Database, Env, console_warn};

use crate::fetch::Counted;
use crate::http::Response;
use crate::tokens::{App, Kv, Tokens};
use crate::{DB, github, int, now_ms};

/// What the app shows of a run beyond its jobs.
#[derive(Debug, Default, PartialEq, Eq, Deserialize)]
pub struct Details {
    /// The run's number in its workflow, as GitHub shows `#412`.
    pub run_number: Option<u64>,
    /// What started it: `push`, `pull_request`, `schedule`, ...
    pub event: Option<String>,
    /// The commit message or pull request title.
    pub display_title: Option<String>,
    /// Who started it.
    #[serde(default, deserialize_with = "login")]
    pub actor: Option<String>,
    /// The workflow file.
    pub path: Option<String>,
}

fn login<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    struct Actor {
        login: String,
    }
    Ok(Option::<Actor>::deserialize(de)?.map(|actor| actor.login))
}

/// Reads the run out of GitHub's answer. A run GitHub will not show,
/// because it is gone or out of the installation's reach, has no
/// details and is not asked about again.
///
/// # Errors
///
/// A server error, an expired token, a rate limit or an answer that is
/// not a run; asking again may work.
pub fn parse(response: &Response) -> Result<Details, String> {
    // GitHub answers a rate limit with 403 too, saying so in the body.
    let limited = response.status == 403 && response.text().contains("rate limit");
    if (400..500).contains(&response.status) && !matches!(response.status, 401 | 429) && !limited {
        return Ok(Details::default());
    }
    if !response.ok() {
        return Err(format!(
            "run: GitHub answered {}: {}",
            response.status,
            response.text()
        ));
    }
    serde_json::from_slice(&response.body).map_err(|err| format!("run: {err}"))
}

/// Records run `?1`'s details at `?7`, once.
pub(crate) const DETAILS: &str = "UPDATE runs SET run_number = ?2, event = ?3, \
     display_title = ?4, actor = ?5, path = ?6, details_at = ?7, updated_at = ?7 \
     WHERE id = ?1 AND details_at IS NULL";

/// Records `details` of run `id`.
pub(crate) async fn record(db: &D1Database, id: u64, details: &Details) -> worker::Result<()> {
    let text = |value: &Option<String>| value.as_deref().map_or(JsValue::NULL, Into::into);
    db.prepare(DETAILS)
        .bind(&[
            int(id)?,
            details.run_number.map_or(Ok(JsValue::NULL), int)?,
            text(&details.event),
            text(&details.display_title),
            text(&details.actor),
            text(&details.path),
            int(now_ms())?,
        ])?
        .run()
        .await?;
    Ok(())
}

/// Fetches run `run_id` of `repository` and records its details. Errors
/// end up in the log; the reconciler asks again.
pub async fn fetch(env: Env, installation_id: u64, repository: String, run_id: u64) {
    let fetched = async {
        let app = App::read(&env)?;
        let api = app.api.clone();
        let calls = Counted::default();
        let tokens = Tokens::new(Kv::new(&env), app, &calls, now_ms());
        let response = tokens
            .send(installation_id, |token| {
                github::workflow_run(&api, token, &repository, run_id)
            })
            .await
            .map_err(|err| err.to_string())?;
        let details = parse(&response)?;
        let db = env.d1(DB).map_err(|err| err.to_string())?;
        record(&db, run_id, &details)
            .await
            .map_err(|err| err.to_string())
    };
    if let Err(err) = fetched.await {
        console_warn!("run {run_id}: {err}");
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::testdb::{Db, params};

    fn answer(status: u16, body: &str) -> Response {
        Response {
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn reads_a_run() {
        let run = parse(&answer(
            200,
            r#"{"id":9,"name":"Smoke test","run_number":412,"event":"push",
                "display_title":"fix(agent): retry the cache upload","path":".github/workflows/smoke.yml",
                "actor":{"login":"octocat","id":1},"head_branch":"main"}"#,
        ))
        .unwrap();
        assert_eq!(
            run,
            Details {
                run_number: Some(412),
                event: Some("push".to_owned()),
                display_title: Some("fix(agent): retry the cache upload".to_owned()),
                actor: Some("octocat".to_owned()),
                path: Some(".github/workflows/smoke.yml".to_owned()),
            }
        );
        assert_eq!(
            parse(&answer(200, r#"{"id":9,"actor":null}"#)).unwrap(),
            Details::default()
        );
    }

    #[test]
    fn a_run_github_will_not_show_is_final() {
        assert_eq!(parse(&answer(404, "{}")).unwrap(), Details::default());
        assert_eq!(parse(&answer(403, "{}")).unwrap(), Details::default());
        assert!(parse(&answer(502, "bad gateway")).is_err());
        assert!(parse(&answer(401, "{}")).is_err());
        assert!(parse(&answer(429, "{}")).is_err());
        for message in [
            "API rate limit exceeded for installation ID 42.",
            "You have exceeded a secondary rate limit.",
        ] {
            let body = format!(r#"{{"message":"{message}"}}"#);
            assert!(parse(&answer(403, &body)).is_err(), "{message}");
        }
        assert!(parse(&answer(200, "[]")).is_err());
    }

    #[test]
    fn details_are_recorded_once() {
        let db = Db::new();
        db.run(
            "INSERT INTO runs (id, installation_id, repository, head_sha, attempt, queued_at, \
             created_at, updated_at) VALUES (9, 1, 'acme/app', 'ab', 1, 5, 5, 5)",
            &[],
        );
        let set = |number: u64| {
            db.run(
                DETAILS,
                &params![
                    9u64,
                    number,
                    "push",
                    "title",
                    "octocat",
                    None::<&str>,
                    10u64
                ],
            )
        };
        assert_eq!(set(412), 1);
        assert_eq!(set(413), 0);
        let number: Option<u64> = db.column("SELECT run_number FROM runs", &[], "run_number");
        assert_eq!(number, Some(412));
    }
}
