// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Picks the workflows a GitHub event starts.

use std::collections::BTreeSet;

use hatchrun_workflow::{
    CommandOptions, Config, Diagnostic, Event, Library, Plan, compile_with, config_errors, parse,
    triggered,
};

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
    select_with(source, event, compile_ref, &Library::default())
}

/// [`select`], with the workflow files of other repositories the
/// workflows call (see [`crate::remote`]).
#[must_use]
pub fn select_with(source: &str, event: &Event, compile_ref: &str, library: &Library) -> Selected {
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
        match compile_with(&config, name, compile_ref, library) {
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

/// The workflows of `source` that `event` starts and that call a file of
/// another repository, directly or through another workflow, so that
/// file is worth reading before they are planned. Empty when the config
/// does not parse.
#[must_use]
pub fn remote_workflows(source: &str, event: &Event) -> Vec<String> {
    let Ok(config) = parse(source) else {
        return Vec::new();
    };
    config
        .workflows
        .iter()
        .filter(|(_, wf)| triggered(&wf.on, event))
        .filter(|(name, _)| {
            !hatchrun_workflow::remote_calls_from(&config, &[name.as_str()], &Library::default())
                .is_empty()
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// Whether a pull request `action` against `base_ref` starts a workflow
/// that filters on changed paths, so the files are worth fetching.
/// False when the config does not parse (it is reported without them).
#[must_use]
pub fn needs_pull_request_paths(source: &str, action: &str, base_ref: &str) -> bool {
    needs_paths(
        source,
        &Event::PullRequest {
            action: action.to_owned(),
            base_ref: base_ref.to_owned(),
            changed: Vec::new(),
        },
    )
}

/// As [`needs_pull_request_paths`], for the workflows of
/// `on: pull_request_target`, which `source` is the base branch's config
/// of.
#[must_use]
pub fn needs_pull_request_target_paths(source: &str, action: &str, base_ref: &str) -> bool {
    needs_paths(
        source,
        &Event::PullRequestTarget {
            action: action.to_owned(),
            base_ref: base_ref.to_owned(),
            changed: Vec::new(),
        },
    )
}

/// Whether `event`, a pull request event, starts a workflow that filters
/// on changed paths. Other events filter on none here.
fn needs_paths(source: &str, event: &Event) -> bool {
    let Ok(config) = parse(source) else {
        return false;
    };
    config.workflows.values().any(|wf| {
        let on = match event {
            Event::PullRequest { .. } => &wf.on.pull_request,
            Event::PullRequestTarget { .. } => &wf.on.pull_request_target,
            _ => return false,
        };
        triggered(&wf.on, event)
            && on
                .as_ref()
                .is_some_and(|pr| pr.paths.is_some() || pr.paths_ignore.is_some())
    })
}

/// Whether any workflow of `source` listens to `pull_request_target`,
/// so the base branch's config is worth planning for a pull request. A
/// config that does not parse has none: the push that made it was told.
#[must_use]
pub fn has_pull_request_target(source: &str) -> bool {
    parse(source).is_ok_and(|config| {
        config
            .workflows
            .values()
            .any(|wf| wf.on.pull_request_target.is_some())
    })
}

/// Whether any workflow of `config` listens to `event`, so the event is
/// worth the calls that plan it.
#[must_use]
pub fn listens(config: &Config, event: &Event) -> bool {
    config.workflows.values().any(|wf| triggered(&wf.on, event))
}

/// The pull request actions `/approve-run` makes a workflow again as, in
/// the order it tries them.
pub const REMAKE_ACTIONS: [&str; 3] = ["synchronize", "opened", "reopened"];

/// The workflows of `config` (those in `among`, all when `None`) that
/// `/approve-run` makes again, grouped by the action they are made as.
/// The comment is no pull request delivery, so a workflow is made as the
/// first of [`REMAKE_ACTIONS`] that it listens to, which is the one that
/// starts it when its pull request gets a commit, is opened or is
/// reopened. A workflow that listens to none of them (one that only runs
/// on `labeled`, say) is not made again. `event` builds the trigger for
/// an action.
#[must_use]
pub fn remake_batches(
    config: &Config,
    event: impl Fn(&str) -> Event,
    among: Option<&[String]>,
) -> Vec<(&'static str, Vec<String>)> {
    let mut left: Vec<&String> = config
        .workflows
        .keys()
        .filter(|name| among.is_none_or(|among| among.contains(name)))
        .collect();
    let mut batches = Vec::new();
    for action in REMAKE_ACTIONS {
        let trigger = event(action);
        let (hit, rest): (Vec<&String>, Vec<&String>) = left
            .into_iter()
            .partition(|name| triggered(&config.workflows[*name].on, &trigger));
        left = rest;
        if !hit.is_empty() {
            batches.push((action, hit.into_iter().cloned().collect()));
        }
    }
    batches
}

/// The names of the chat commands any workflow of `config` defines, for
/// the comment parser to look for.
#[must_use]
pub fn command_names(config: &Config) -> BTreeSet<String> {
    config
        .workflows
        .values()
        .filter_map(|wf| wf.on.command.as_ref())
        .flat_map(|commands| commands.iter().map(|(name, _)| name.to_owned()))
        .collect()
}

/// A workflow that defines a chat command, with the command's options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandWorkflow {
    /// The workflow's name.
    pub workflow: String,
    /// The command's permission, places, sha rule and reaction.
    pub options: CommandOptions,
}

/// The workflows of `config` that define the command `name` for a
/// comment on a pull request (`on_pull_request`) or on a plain issue, in
/// workflow name order. Each decides alone who may run it, so the caller
/// checks each one's options.
#[must_use]
pub fn command_workflows(
    config: &Config,
    name: &str,
    on_pull_request: bool,
) -> Vec<CommandWorkflow> {
    let event = Event::Command {
        name: name.to_owned(),
        on_pull_request,
    };
    config
        .workflows
        .iter()
        .filter(|(_, wf)| triggered(&wf.on, &event))
        .filter_map(|(workflow, wf)| {
            Some(CommandWorkflow {
                workflow: workflow.clone(),
                options: wf.on.command_options(name)?.clone(),
            })
        })
        .collect()
}

/// What the planner finds in each of `plans`, workflows of `source`:
/// the findings of [`hatchrun_workflow::workflow_lints`] for the
/// workflow's own triggers, one line each.
#[must_use]
pub fn workflow_findings(source: &str, plans: &[Plan]) -> Vec<Vec<String>> {
    let config = parse(source).ok();
    plans
        .iter()
        .map(|plan| {
            config
                .as_ref()
                .and_then(|config| config.workflows.get(&plan.workflow))
                .map(|wf| {
                    hatchrun_workflow::workflow_lints(plan, &wf.on)
                        .iter()
                        .map(hatchrun_workflow::Lint::text)
                        .collect()
                })
                .unwrap_or_default()
        })
        .collect()
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
    fn a_pull_request_target_listener_is_found_in_the_config() {
        let config = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  a: { on: { pull_request: {} }, jobs: [t] }
  b: { on: { pull_request_target: {} }, jobs: [t] }
";
        assert!(has_pull_request_target(config));
        assert!(!has_pull_request_target(
            &config.replace("pull_request_target", "push")
        ));
        // A config that cannot be read has none to run.
        assert!(!has_pull_request_target("not: [valid"));
    }

    const CHAT: &str = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  retest:
    on:
      command:
        retest: {}
    jobs: [t]
  deploy:
    on:
      command:
        deploy: { permission: maintain, in: [pull_request, issue] }
        retest: { permission: admin }
    jobs: [t]
  triage:
    on: { issue_comment: { types: [created] } }
    jobs: [t]
";

    #[test]
    fn the_commands_of_every_workflow_are_known() {
        let names: Vec<String> = command_names(&parse(CHAT).unwrap()).into_iter().collect();
        assert_eq!(names, ["deploy", "retest"]);
        assert!(command_names(&parse(CONFIG).unwrap()).is_empty());
    }

    #[test]
    fn a_command_finds_the_workflows_that_define_it_for_the_place() {
        let on_pr = command_workflows(&parse(CHAT).unwrap(), "retest", true);
        let names: Vec<&str> = on_pr.iter().map(|c| c.workflow.as_str()).collect();
        assert_eq!(names, ["deploy", "retest"]);
        // Each workflow keeps its own options.
        assert_eq!(
            on_pr[0].options.permission,
            hatchrun_workflow::CommandPermission::Admin
        );
        assert_eq!(
            on_pr[1].options.permission,
            hatchrun_workflow::CommandPermission::Write
        );
        // `deploy` allows issues, `retest` does not.
        assert_eq!(
            command_workflows(&parse(CHAT).unwrap(), "deploy", false).len(),
            1
        );
        assert_eq!(
            command_workflows(&parse(CHAT).unwrap(), "retest", false).len(),
            0
        );
        assert_eq!(
            command_workflows(&parse(CHAT).unwrap(), "nope", true).len(),
            0
        );
    }

    #[test]
    fn a_remade_workflow_runs_as_the_first_action_it_listens_to() {
        let config = parse(
            "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  a: { on: { pull_request: { types: [opened, reopened] } }, jobs: [t] }
  b: { on: { pull_request: {} }, jobs: [t] }
  c: { on: { pull_request: { types: [reopened] } }, jobs: [t] }
  d: { on: { pull_request: { types: [labeled] } }, jobs: [t] }
  e: { on: { push: {} }, jobs: [t] }
",
        )
        .unwrap();
        let event = |action: &str| Event::PullRequest {
            action: action.into(),
            base_ref: "main".into(),
            changed: Vec::new(),
        };
        // `b` has the default types, which include `synchronize`; `d`
        // and `e` listen to none of the three.
        assert_eq!(
            remake_batches(&config, event, None),
            [
                ("synchronize", vec!["b".to_owned()]),
                ("opened", vec!["a".to_owned()]),
                ("reopened", vec!["c".to_owned()]),
            ]
        );
        let among = ["a".to_owned(), "d".to_owned()];
        assert_eq!(
            remake_batches(&config, event, Some(&among)),
            [("opened", vec!["a".to_owned()])]
        );
    }

    #[test]
    fn an_event_is_listened_to_by_a_workflow_that_names_it() {
        let comment = |action: &str| Event::IssueComment {
            action: action.into(),
        };
        let chat = parse(CHAT).unwrap();
        assert!(listens(&chat, &comment("created")));
        assert!(!listens(&chat, &comment("edited")));
        assert!(!listens(
            &chat,
            &Event::Issues {
                action: "opened".into()
            }
        ));
    }

    #[test]
    fn a_workflows_findings_use_its_own_triggers() {
        let config = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [{ run: 'echo ${{ github.event.comment.body }}' }] }
workflows:
  talk:
    on: { issue_comment: {} }
    jobs: [t]
  quiet:
    on: { push: {} }
    jobs: [t]
";
        let event = Event::IssueComment {
            action: "created".into(),
        };
        let Selected::Plans { plans, .. } = select(config, &event, "refs/heads/main") else {
            panic!("config is unusable");
        };
        assert_eq!(plans.len(), 1);
        let found = workflow_findings(config, &plans);
        assert!(!found[0].is_empty(), "{found:?}");
        assert!(
            found[0].iter().any(|line| line.contains("comment")),
            "{found:?}"
        );
        assert_eq!(
            workflow_findings("not: [valid", &plans),
            [Vec::<String>::new()]
        );
    }

    #[test]
    fn pull_request_target_paths_are_needed_by_its_own_workflows_only() {
        let config = "
jobs:
  t: { runs-on: ubuntu-latest, steps: [] }
workflows:
  plain:
    on: { pull_request_target: { types: [labeled] } }
    jobs: [t]
  docs:
    on: { pull_request_target: { types: [opened], paths: ['docs/**'] } }
    jobs: [t]
  other:
    on: { pull_request: { types: [labeled], paths: ['src/**'] } }
    jobs: [t]
";
        assert!(needs_pull_request_target_paths(config, "opened", "main"));
        assert!(!needs_pull_request_target_paths(config, "labeled", "main"));
        // The pull_request workflow's filter is not the target's.
        assert!(needs_pull_request_paths(config, "labeled", "main"));
        assert!(!needs_pull_request_paths(config, "opened", "main"));
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

    #[test]
    fn remote_files_are_read_only_for_a_triggered_workflow_that_reaches_them() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: { branches: [main] } }
    jobs: [{ up: { call: shared } }]
  shared:
    on: { call: {} }
    jobs: [{ far: { call: 'acme/shared/.github/workflows/x.yml@v1' } }]
  other:
    on: { push: { branches: [dev] } }
    jobs: [{ far: { call: 'acme/other/.github/workflows/y.yml@v1' } }]
  plain:
    on: { schedule: [{ cron: '0 3 * * *' }] }
    jobs: [t]
";
        assert_eq!(remote_workflows(source, &push("refs/heads/main")), ["ci"]);
        assert_eq!(remote_workflows(source, &push("refs/heads/dev")), ["other"]);
        assert_eq!(remote_workflows(CONFIG, &push("refs/heads/main")).len(), 0);
        assert_eq!(
            remote_workflows("jobs: [", &push("refs/heads/main")).len(),
            0
        );
    }
}
