// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Picks the workflows a GitHub event starts.

use hatchrun_workflow::{Diagnostic, Event, Plan, compile, config_errors, parse, triggered};

/// A triggered workflow that did not compile.
#[derive(Debug, PartialEq, Eq)]
pub struct Failed {
    /// The workflow's name.
    pub workflow: String,
    /// What is wrong with it.
    pub diagnostics: Vec<Diagnostic>,
}

/// The workflows an event starts, or why the config cannot be used.
#[derive(Debug, PartialEq, Eq)]
pub enum Selected {
    /// Plans of the triggered workflows that compiled, in workflow name
    /// order, and those that did not.
    Plans {
        /// The workflows to run.
        plans: Vec<Plan>,
        /// The workflows that fail on their own, such as one that
        /// names no runner template.
        failed: Vec<Failed>,
    },
    /// The config does not parse, or `config_errors` found something.
    Invalid(Vec<Diagnostic>),
}

/// Parses `source`, keeps the workflows whose `on:` `event` triggers
/// (in name order) and compiles each against `compile_ref`. Plans with
/// no jobs are dropped. A config that does not parse, or has a problem
/// of its own (see [`config_errors`]), makes the whole result `Invalid`.
/// A workflow that does not compile fails alone, and the others still
/// run.
#[must_use]
pub fn select(source: &str, event: &Event, compile_ref: &str) -> Selected {
    let config = match parse(source) {
        Ok(config) => config,
        Err(diagnostics) => return Selected::Invalid(diagnostics),
    };
    let errors = config_errors(&config);
    if !errors.is_empty() {
        return Selected::Invalid(errors);
    }
    let mut plans = Vec::new();
    let mut failed = Vec::new();
    for (name, workflow) in &config.workflows {
        if !triggered(&workflow.on, event) {
            continue;
        }
        match compile(&config, name, compile_ref) {
            Ok(plan) if plan.jobs.is_empty() => {}
            Ok(plan) => plans.push(plan),
            Err(diagnostics) => failed.push(Failed {
                workflow: name.clone(),
                diagnostics,
            }),
        }
    }
    Selected::Plans { plans, failed }
}

/// Whether a pull request `action` against `base_ref` starts a workflow
/// that filters on changed paths, so the files are worth fetching.
/// False when the config does not parse (it is reported without them).
#[must_use]
pub fn needs_pull_request_paths(source: &str, action: &str, base_ref: &str) -> bool {
    let Ok(config) = parse(source) else {
        return false;
    };
    let event = Event::PullRequest {
        action: action.to_owned(),
        base_ref: base_ref.to_owned(),
        changed: Vec::new(),
    };
    config.workflows.values().any(|wf| {
        triggered(&wf.on, &event)
            && wf
                .on
                .pull_request
                .as_ref()
                .is_some_and(|pr| pr.paths.is_some() || pr.paths_ignore.is_some())
    })
}

/// Every `(workflow, cron)` pair of `on: schedule`, in workflow name
/// order. Empty when the config does not parse.
#[must_use]
pub fn schedules(source: &str) -> Vec<(String, String)> {
    let Ok(config) = parse(source) else {
        return Vec::new();
    };
    config
        .workflows
        .iter()
        .flat_map(|(name, wf)| {
            wf.on
                .schedule
                .iter()
                .map(move |s| (name.clone(), s.cron.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  ci:
    on: { push: { branches: [main] } }
    jobs: [t]
  nightly:
    on: { schedule: [{ cron: '0 3 * * *' }] }
    jobs: [t]
";

    fn push(r: &str) -> Event {
        Event::Push {
            r#ref: r.into(),
            changed: Vec::new(),
        }
    }

    #[test]
    fn pull_request_paths_are_needed_only_for_a_listening_path_filter() {
        let config = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  plain:
    on: { pull_request: { branches: [main] } }
    jobs: [t]
  docs:
    on: { pull_request: { types: [labeled], paths: ['docs/**'] } }
    jobs: [t]
";
        assert!(needs_pull_request_paths(config, "labeled", "main"));
        assert!(!needs_pull_request_paths(config, "opened", "main"));
        assert!(!needs_pull_request_paths("not: [valid", "labeled", "main"));
    }

    #[test]
    fn selects_triggered_workflows_only() {
        let Selected::Plans { plans, .. } =
            select(CONFIG, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].workflow, "ci");
        let Selected::Plans { plans: none, .. } =
            select(CONFIG, &push("refs/heads/dev"), "refs/heads/dev")
        else {
            panic!("invalid");
        };
        assert_eq!(none, Vec::<hatchrun_workflow::Plan>::new());
    }

    #[test]
    fn a_workflow_that_does_not_compile_fails_alone() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs: [{ t: { requires: [nope] } }]
  fine:
    on: { push: {} }
    jobs: [t]
";
        let Selected::Plans { plans, failed } =
            select(source, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].workflow, "fine");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].workflow, "ci");
        assert_eq!(failed[0].diagnostics.len(), 1);
    }

    #[test]
    fn a_config_that_does_not_parse_is_invalid() {
        assert!(matches!(
            select("jobs: [", &push("refs/heads/main"), "refs/heads/main"),
            Selected::Invalid(d) if !d.is_empty()
        ));
    }

    #[test]
    fn config_wide_error_is_still_invalid() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci: { on: { push: {}, schedule: [{ cron: 'not a cron' }] }, jobs: [t] }
";
        assert!(matches!(
            select(source, &push("refs/heads/main"), "refs/heads/main"),
            Selected::Invalid(d) if d.iter().any(|d| d.message.contains("invalid cron"))
        ));
    }

    const TEMPLATE_LIST: &str = "ubuntu-latest, ubuntu-26.04, ubuntu-26.04-arm, \
        ubuntu-26.04-4core, ubuntu-26.04-4core-arm, ubuntu-26.04-8core, \
        ubuntu-26.04-8core-arm, ubuntu-26.04-16core, ubuntu-26.04-16core-arm";

    #[test]
    fn bad_label_fails_only_its_workflow() {
        let source = "
jobs:
  good: { runs-on: ubuntu-latest, steps: [] }
  bad: { runs-on: ubuntu-24.04, steps: [] }
workflows:
  ci: { on: { push: {} }, jobs: [good] }
  bad: { on: { push: {} }, jobs: [bad] }
";
        let Selected::Plans { plans, failed } =
            select(source, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        let names: Vec<_> = plans.iter().map(|plan| plan.workflow.as_str()).collect();
        assert_eq!(names, ["ci"]);
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].workflow, "bad");
        let message = &failed[0].diagnostics[0].message;
        assert!(
            message.contains(&format!(
                "names no runner template; use one of {TEMPLATE_LIST}"
            )),
            "{message}"
        );
    }

    #[test]
    fn bad_executor_fails_only_the_workflows_that_use_it() {
        let source = "
executors: { old: { runs-on: ubuntu-24.04 } }
jobs:
  good: { runs-on: ubuntu-latest, steps: [] }
  legacy: { executor: old, steps: [] }
workflows:
  ci: { on: { push: {} }, jobs: [good] }
  legacy: { on: { push: {} }, jobs: [legacy] }
";
        let Selected::Plans { plans, failed } =
            select(source, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].workflow, "ci");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].workflow, "legacy");
        assert!(failed[0].diagnostics[0].message.contains("executor `old`"));
    }

    #[test]
    fn plan_with_every_job_filtered_out_is_dropped() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs:
      - t: { branches: [release] }
";
        let Selected::Plans { plans, failed } =
            select(source, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans, Vec::<hatchrun_workflow::Plan>::new());
        assert_eq!(failed.len(), 0);
    }

    #[test]
    fn schedules_lists_every_cron() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  b: { on: { schedule: [{ cron: '0 3 * * *' }, { cron: '5 4 * * 1' }] }, jobs: [t] }
  a: { on: { push: {} }, jobs: [t] }
  c: { on: { schedule: [{ cron: '*/15 * * * *' }] }, jobs: [t] }
";
        assert_eq!(
            schedules(source),
            [
                ("b".to_owned(), "0 3 * * *".to_owned()),
                ("b".to_owned(), "5 4 * * 1".to_owned()),
                ("c".to_owned(), "*/15 * * * *".to_owned()),
            ]
        );
        assert_eq!(schedules("jobs: ["), Vec::<(String, String)>::new());
    }
}
