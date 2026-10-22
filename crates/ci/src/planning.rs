// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Picks the workflows a GitHub event starts.

use hatchrun_workflow::{Diagnostic, Event, Plan, Template, compile, parse, triggered};

/// The workflows an event starts, or why the config cannot be used.
#[derive(Debug, PartialEq, Eq)]
pub enum Selected {
    /// A plan for each started workflow, in workflow name order.
    Plans(Vec<Plan>),
    /// The config does not parse or compile.
    Invalid(Vec<Diagnostic>),
}

/// Parses `source`, keeps the workflows whose `on:` `event` triggers
/// (in name order) and compiles each against `compile_ref`. Plans with
/// no jobs are dropped. Any parse or compile error makes the whole
/// result `Invalid`.
#[must_use]
pub fn select(source: &str, event: &Event, compile_ref: &str) -> Selected {
    let config = match parse(source) {
        Ok(config) => config,
        Err(diagnostics) => return Selected::Invalid(diagnostics),
    };
    let mut plans = Vec::new();
    let mut errors: Vec<Diagnostic> = Vec::new();
    for (name, workflow) in &config.workflows {
        if !triggered(&workflow.on, event) {
            continue;
        }
        match compile(&config, name, compile_ref) {
            Ok(plan) if plan.jobs.is_empty() => {}
            Ok(plan) => plans.push(plan),
            Err(diagnostics) => {
                // Config-wide problems are reported by every compile.
                for d in diagnostics {
                    if !errors.contains(&d) {
                        errors.push(d);
                    }
                }
            }
        }
    }
    if errors.is_empty() {
        Selected::Plans(plans)
    } else {
        Selected::Invalid(errors)
    }
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

/// Whether jobs on `template` can launch yet. Only `ubuntu-latest` can.
///
/// # Errors
///
/// Returns the message to fail the job with.
pub fn launchable(template: &Template) -> Result<(), String> {
    let label = template.labels[0];
    if label == "ubuntu-latest" {
        Ok(())
    } else {
        Err(format!(
            "`{label}` runners come in a later release; use ubuntu-latest or ubuntu-26.04"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatchrun_workflow::template;

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
    fn selects_triggered_workflows_only() {
        let Selected::Plans(plans) = select(CONFIG, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].workflow, "ci");
        let Selected::Plans(none) = select(CONFIG, &push("refs/heads/dev"), "refs/heads/dev")
        else {
            panic!("invalid");
        };
        assert_eq!(none, Vec::<hatchrun_workflow::Plan>::new());
    }

    #[test]
    fn compile_errors_make_the_result_invalid() {
        let source = "
jobs: { t: { runs-on: ubuntu-latest, steps: [] } }
workflows:
  ci:
    on: { push: {} }
    jobs: [{ t: { requires: [nope] } }]
";
        assert!(matches!(
            select(source, &push("refs/heads/main"), "refs/heads/main"),
            Selected::Invalid(d) if !d.is_empty()
        ));
        assert!(matches!(
            select("jobs: [", &push("refs/heads/main"), "refs/heads/main"),
            Selected::Invalid(d) if !d.is_empty()
        ));
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
        let Selected::Plans(plans) = select(source, &push("refs/heads/main"), "refs/heads/main")
        else {
            panic!("invalid");
        };
        assert_eq!(plans, Vec::<hatchrun_workflow::Plan>::new());
    }

    #[test]
    fn other_templates_are_not_launchable() {
        assert!(launchable(template("ubuntu-latest").unwrap()).is_ok());
        assert!(launchable(template("ubuntu-26.04").unwrap()).is_ok());
        let err = launchable(template("ubuntu-26.04-arm").unwrap()).unwrap_err();
        assert!(
            err.contains("`ubuntu-26.04-arm` runners come in a later release"),
            "{err}"
        );
        assert!(err.ends_with("use ubuntu-latest or ubuntu-26.04"), "{err}");
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
