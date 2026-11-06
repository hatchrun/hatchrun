// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Concurrency groups on the control plane.
//!
//! The run's object evaluates the groups when it queues a job and
//! records them ([`claims`], [`store::register`]). The launch consumer
//! puts the run, then the job, in line ([`gate`]): the one that runs
//! goes on, the one that waits is held for `group` with a message that
//! names who runs, and a superseded one is cancelled. A run or job that
//! ends gives up its place ([`release`]), which lets the one that waits
//! run and wakes its held jobs.
//!
//! The cancel path is the run object's: a whole run through
//! `POST /cancel?attempt=N`, one job through `POST /cancel?job=ID&attempt=N`, both
//! with a message that the check run shows.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use hatchrun_cloud::db::Db;
use hatchrun_workflow::hatchrun_context;
use serde_json::{Value, json};
use worker::{Env, Method, RequestInit, console_error};

use crate::env::run_stub;
use crate::graph::Node;
use crate::launches::send_wakes;
use crate::run::RunInfo;
use crate::store::{self, NewSlot, Slot, Victim};

/// The word `ci_jobs.held` stores for a job that waits for a group.
pub const HOLD: &str = "group";

/// The context a workflow's group is evaluated against: what the
/// workflow level reads (`github`, `inputs`, `vars`).
#[must_use]
pub fn context(github_ctx: &Value, vars: &BTreeMap<String, String>) -> Value {
    let inputs = github_ctx
        .get("event")
        .and_then(|event| event.get("inputs"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    json!({
        "hatchrun": hatchrun_context(github_ctx),
        "github": github_ctx,
        "vars": vars,
        "inputs": inputs,
    })
}

/// What to record for the queued job `node` of the run: its workflow's
/// group, evaluated against `workflow_ctx` ([`context`]), and its own,
/// evaluated against `job_ctx` ([`crate::graph::Graph::job_context`]),
/// each evaluated or, when the evaluation failed, the reason.
#[must_use]
pub fn claims(
    info: &RunInfo,
    node: &Node,
    workflow_ctx: &Value,
    job_ctx: &Value,
    now: u64,
) -> Vec<NewSlot> {
    let mut out = Vec::new();
    if let Some(c) = &info.concurrency {
        out.push(NewSlot {
            repo_id: info.repo.id,
            run_id: info.run_id,
            job_id: None,
            attempt: info.run_attempt,
            group: c
                .resolve(workflow_ctx)
                .map(|r| (r.group, r.cancel_in_progress)),
            now,
        });
    }
    if let Some(c) = &node.job.concurrency {
        out.push(NewSlot {
            repo_id: info.repo.id,
            run_id: info.run_id,
            job_id: Some(node.id),
            attempt: node.attempt,
            group: c.resolve(job_ctx).map(|r| (r.group, r.cancel_in_progress)),
            now,
        });
    }
    out
}

/// What the launch consumer does with a job after looking at its groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// Every group it names lets it run.
    Pass,
    /// It waits for a group; the text is for the check run.
    Hold(String),
    /// A group could not be evaluated; the text says why.
    Fail(String),
    /// A newer arrival took its place; the cancel is on its way.
    Cancelled,
}

/// Who holds a group, as the waiting message names them.
fn who(blocker: Option<&store::Blocker>) -> String {
    match blocker {
        Some(b) => match &b.job_name {
            Some(job) => format!("job `{job}` of run #{}", b.run_number),
            None => format!("run #{} of `{}`", b.run_number, b.workflow),
        },
        None => "another run".to_owned(),
    }
}

/// The check run text of a job that waits for a group.
#[must_use]
pub fn waiting_message(slot: &Slot, blocker: Option<&store::Blocker>) -> String {
    let what = if slot.job_id.is_some() { "job" } else { "run" };
    format!(
        "Waiting for concurrency group `{}`: {} is running. A newer {what} takes this one's place while it waits.",
        slot.group_name,
        who(blocker)
    )
}

/// The check run text of a run or job that is cancelled for a group.
#[must_use]
pub fn cancel_message(victim: &Victim) -> String {
    let what = if victim.job_id.is_some() {
        "job"
    } else {
        "run"
    };
    if victim.state == "superseded" {
        format!(
            "Cancelled: a newer {what} replaced it while it waited for concurrency group `{}`.",
            victim.group_name
        )
    } else {
        format!(
            "Cancelled: a newer {what} in concurrency group `{}` set cancel-in-progress.",
            victim.group_name
        )
    }
}

/// Looks at the groups of queued job `job_id` of run `run_id`, the
/// workflow's first, and puts the run or job in line where it is not
/// yet. Stops at the first group that does not let the job run.
///
/// # Errors
///
/// Postgres refused a statement; nothing was decided.
pub async fn gate(
    env: &Env,
    db: &Db,
    run_id: u64,
    job_id: u64,
    attempt: u32,
) -> Result<Gate, String> {
    let db_err = |e: worker::Error| e.to_string();
    for slot in store::slots(db, run_id, job_id).await.map_err(db_err)? {
        // A row of another attempt of the job is left over.
        if slot.job_id.is_some() && slot.attempt != attempt {
            continue;
        }
        let slot = if slot.state == "new" {
            let Some(entered) = store::enter(db, &slot).await.map_err(db_err)? else {
                continue;
            };
            // Entering sweeps the rows of ended holders and may promote
            // a waiting row that `release` never saw. Wake what that row
            // holds; this job's own turn is the one running.
            wake_promoted(env, db, &entered, job_id).await;
            entered
        } else {
            slot
        };
        match slot.state.as_str() {
            "invalid" => return Ok(Gate::Fail(slot.group_name)),
            "running" => {}
            "pending" => {
                enforce(env, db, &slot).await;
                let blocker = store::blocker(db, slot.repo_id, &slot.group_key)
                    .await
                    .map_err(db_err)?;
                return Ok(Gate::Hold(waiting_message(&slot, blocker.as_ref())));
            }
            "superseded" => {
                enforce(env, db, &slot).await;
                return Ok(Gate::Cancelled);
            }
            other => return Err(format!("concurrency row {} is {other}", slot.id)),
        }
    }
    Ok(Gate::Pass)
}

/// Wakes the held jobs of the group of `slot`, other than `job_id`.
async fn wake_promoted(env: &Env, db: &Db, slot: &Slot, job_id: u64) {
    match store::wake_targets(db, slot.repo_id, &slot.group_key).await {
        Ok(woken) => {
            let woken: Vec<_> = woken.into_iter().filter(|w| w.job_id != job_id).collect();
            if let Err(err) = send_wakes(env, &woken).await {
                console_error!("group {}: wake the promoted: {err}", slot.group_name);
            }
        }
        Err(err) => console_error!("group {}: find the promoted: {err}", slot.group_name),
    }
}

/// Cancels the rows of the group of `slot` that a newer arrival
/// replaced or marked. A cancel that fails is made again at the next
/// recheck of whoever waits.
async fn enforce(env: &Env, db: &Db, slot: &Slot) {
    let victims = match store::victims(db, slot.repo_id, &slot.group_key).await {
        Ok(victims) => victims,
        Err(err) => {
            console_error!("group {}: find what to cancel: {err}", slot.group_name);
            return;
        }
    };
    for victim in &victims {
        if let Err(err) = post_cancel(env, victim).await {
            console_error!(
                "group {}: cancel run {}: {err}",
                victim.group_name,
                victim.run_id
            );
        }
    }
}

/// Asks the object of the victim's run to cancel the run, or one job of
/// it, with the reason. A run that never started (`superseded`) ends
/// whatever its jobs' `if:`.
async fn post_cancel(env: &Env, victim: &Victim) -> Result<(), String> {
    let body = json!({
        "message": cancel_message(victim),
        "outright": victim.state == "superseded",
    });
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_body(Some(body.to_string().into()));
    let mut url = format!(
        "https://run/cancel?run={}&tenant={}&days={}",
        victim.run_id, victim.tenant_id, victim.retention_days
    );
    // The attempt of the job, or of the run when no job is named, so a
    // cancel queued before a rerun cannot end the new attempt.
    if let Some(job) = victim.job_id {
        let _ = write!(url, "&job={job}");
    }
    let _ = write!(url, "&attempt={}", victim.attempt);
    let request = worker::Request::new_with_init(&url, &init).map_err(|e| e.to_string())?;
    let response = run_stub(env, victim.run_id)
        .map_err(|e| e.to_string())?
        .fetch_with_request(request)
        .await
        .map_err(|e| e.to_string())?;
    match response.status_code() {
        200..=299 | 404 | 409 => Ok(()),
        other => Err(format!("the run's object answered {other}")),
    }
}

/// Sends the cancel again for superseded rows whose run or job is
/// still open, which happens when the first cancel failed and the one
/// that waited was promoted before anything looked at the group again.
/// Takes `budget` for each call and stops when it is spent. Returns how
/// many cancels failed to send.
///
/// # Errors
///
/// Postgres refused the query; nothing was sent.
pub async fn resend_cancels(
    env: &Env,
    db: &Db,
    limit: u32,
    mut take: impl FnMut() -> bool,
) -> worker::Result<u32> {
    let mut failed = 0;
    for victim in store::unfinished_superseded(db, limit).await? {
        if !take() {
            break;
        }
        if let Err(err) = post_cancel(env, &victim).await {
            failed += 1;
            console_error!(
                "group {}: cancel run {} again: {err}",
                victim.group_name,
                victim.run_id
            );
        }
    }
    Ok(failed)
}

/// A run (`job_id` is `None`) or a job ended: gives up its place in its
/// group, lets the one that waits run and wakes that one's held jobs. A
/// failure to wake is logged: the reconciler's backstop wakes held jobs
/// that nobody looked at.
///
/// # Errors
///
/// Postgres refused a statement while the place was given up; the caller
/// tries again.
pub async fn release(env: &Env, db: &Db, run_id: u64, job_id: Option<u64>) -> worker::Result<()> {
    let Some((repo_id, group_key)) = store::leave(db, run_id, job_id).await? else {
        return Ok(());
    };
    match store::wake_targets(db, repo_id, &group_key).await {
        Ok(woken) => {
            if let Err(err) = send_wakes(env, &woken).await {
                console_error!("group {group_key}: wake the next: {err}");
            }
        }
        Err(err) => console_error!("group {group_key}: find the next: {err}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use hatchrun_workflow::{Cancel, Concurrency, compile, parse};

    use super::*;
    use crate::graph::Graph;
    use crate::intake::Repo;

    fn info() -> RunInfo {
        RunInfo {
            run_id: 3,
            run_number: 12,
            run_attempt: 1,
            tenant_id: "t".into(),
            retention_days: 30,
            installation_id: 1,
            repo: Repo {
                id: 7,
                full_name: "acme/app".into(),
                default_branch: "main".into(),
                private: false,
            },
            workflow: "ci".into(),
            event_name: "push".into(),
            git_ref: "refs/heads/dev".into(),
            sha: "abc".into(),
            actor: "octocat".into(),
            head_ref: String::new(),
            base_ref: String::new(),
            concurrency: None,
        }
    }

    fn node_of(yaml: &str) -> (Graph, Option<Concurrency>) {
        let config = parse(yaml).unwrap_or_else(|e| panic!("{e:?}"));
        let plan = compile(&config, "ci", "refs/heads/main").unwrap_or_else(|e| panic!("{e:?}"));
        let workflow = plan.concurrency.clone();
        let graph = Graph::new(plan.jobs.into_iter().map(|j| (7, j)).collect());
        (graph, workflow)
    }

    fn slots_of(
        run: &RunInfo,
        graph: &Graph,
        gh: &Value,
        vars: &BTreeMap<String, String>,
    ) -> Vec<NewSlot> {
        let job_ctx = graph
            .job_context(7, gh, vars)
            .expect("job 7")
            .expect("inputs");
        claims(run, &graph.jobs[0], &context(gh, vars), &job_ctx, 5)
    }

    const WORKFLOW: &str = "
jobs:
  deploy:
    runs-on: ubuntu-latest
    concurrency: { group: 'deploy-${{ matrix.env }}-${{ vars.REGION }}', cancel-in-progress: true }
    steps: []
workflows:
  ci:
    on: { push: {} }
    concurrency:
      group: '${{ github.workflow }}-${{ inputs.target }}'
      cancel-in-progress: \"${{ github.ref != 'refs/heads/main' }}\"
    jobs:
      - deploy: { matrix: { env: [prod] } }
";

    #[test]
    fn both_levels_are_evaluated_with_their_own_contexts() {
        let (graph, workflow) = node_of(WORKFLOW);
        let mut run = info();
        run.concurrency = workflow;
        let gh = json!({
            "workflow": "ci",
            "ref": "refs/heads/dev",
            "event": { "inputs": { "target": "eu" } },
        });
        let vars = BTreeMap::from([("REGION".to_owned(), "eu-1".to_owned())]);
        let slots = slots_of(&run, &graph, &gh, &vars);
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].job_id, None);
        assert_eq!(slots[0].group, Ok(("ci-eu".to_owned(), true)));
        assert_eq!(slots[1].job_id, Some(7));
        assert_eq!(slots[1].group, Ok(("deploy-prod-eu-1".to_owned(), true)));
    }

    #[test]
    fn a_job_group_can_read_the_strategy_context() {
        let yaml = "
jobs:
  deploy:
    runs-on: ubuntu-latest
    concurrency: { group: 'lane-${{ strategy.job-index }}-of-${{ strategy.job-total }}' }
    steps: []
workflows:
  ci:
    on: { push: {} }
    jobs:
      - deploy: { matrix: { env: [a, b] } }
";
        let (graph, _) = node_of(yaml);
        let slots = slots_of(&info(), &graph, &json!({}), &BTreeMap::new());
        assert_eq!(slots[0].group, Ok(("lane-0-of-2".to_owned(), false)));
    }

    #[test]
    fn a_group_that_cannot_be_evaluated_says_why() {
        let (graph, _) = node_of(WORKFLOW);
        let mut run = info();
        run.concurrency = Some(Concurrency {
            group: "${{ github.nope.deeper }}-${{ fromJSON('x') }}".into(),
            cancel_in_progress: Cancel::No,
        });
        let slots = slots_of(&run, &graph, &json!({}), &BTreeMap::new());
        let why = slots[0].group.clone().unwrap_err();
        assert!(why.contains("concurrency group"), "{why}");
    }

    #[test]
    fn messages_name_who_runs_and_why_a_run_was_cancelled() {
        let slot = Slot {
            id: 1,
            repo_id: 2,
            run_id: 3,
            job_id: None,
            attempt: 1,
            group_key: "g".into(),
            group_name: "G".into(),
            state: "pending".into(),
            cancel_others: false,
            cancel_me: false,
        };
        let blocker = store::Blocker {
            run_number: 12,
            workflow: "ci".into(),
            job_name: None,
        };
        assert_eq!(
            waiting_message(&slot, Some(&blocker)),
            "Waiting for concurrency group `G`: run #12 of `ci` is running. A newer run takes this one's place while it waits."
        );
        let job = store::Blocker {
            job_name: Some("deploy".into()),
            ..blocker
        };
        let job_slot = Slot {
            job_id: Some(9),
            ..slot
        };
        assert!(waiting_message(&job_slot, Some(&job)).contains("job `deploy` of run #12"));
        assert!(waiting_message(&job_slot, None).contains("another run"));
        let victim = Victim {
            run_id: 3,
            job_id: None,
            attempt: 1,
            state: "superseded".into(),
            group_name: "G".into(),
            tenant_id: "t".into(),
            retention_days: 30,
        };
        assert!(cancel_message(&victim).contains("replaced it while it waited"));
        let running = Victim {
            state: "running".into(),
            ..victim
        };
        assert!(cancel_message(&running).contains("cancel-in-progress"));
    }
}
