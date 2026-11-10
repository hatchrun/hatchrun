// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Warnings about workflows that run attacker-influenced input with the
//! base repository's secrets.
//!
//! A `pull_request_target` workflow runs trusted (secrets, a write
//! token) on an event that anyone can cause. Two mistakes turn that into
//! remote code execution, and both are found by reading the steps:
//!
//! - [`LintKind::HeadCheckout`]: the workflow checks out the pull
//!   request's head (the fork's code) and runs it.
//! - [`LintKind::ScriptInjection`]: a field the pull request's author
//!   writes (title, body, branch name) is interpolated with `${{ }}`
//!   straight into a `run:` script, where the shell reads it as code.
//!
//! The detectors are heuristics over the planned steps, so a workflow
//! can still be unsafe without a warning. A warning never stops
//! `validate` or `convert` from succeeding; the host decides what a
//! head checkout does (see the repository's `pull_request_target_head_checkout`
//! setting).

use crate::config::{CheckoutStep, RunStep, UsesStep};
use crate::plan::{Plan, PlannedStep};
use crate::{Config, Diagnostic, compile};

/// What a lint found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintKind {
    /// The workflow checks out the pull request's head.
    HeadCheckout,
    /// An untrusted event field is interpolated into a `run:` script.
    ScriptInjection,
}

/// One finding, with where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lint {
    /// What was found.
    pub kind: LintKind,
    /// The workflow's name.
    pub workflow: String,
    /// The planned job's id.
    pub job: String,
    /// The step's position in the job, from 1.
    pub step: usize,
    /// What is wrong and how to fix it, in one sentence or two.
    pub message: String,
}

impl Lint {
    /// The finding as one line that names its place.
    #[must_use]
    pub fn text(&self) -> String {
        format!(
            "workflow `{}`, job `{}`, step {}: {}",
            self.workflow, self.job, self.step, self.message
        )
    }
}

/// The text of the head checkout finding. The head checkout is what the
/// repository setting `pull_request_target_head_checkout` blocks.
const HEAD_CHECKOUT: &str = "checks out the pull request head in a pull_request_target workflow, \
     so the code of the pull request runs with the base repository's secrets and write token; \
     check out the base and run the pull request's code in a pull_request workflow instead";

/// Fields of an event that the author of an issue, comment or pull
/// request writes. Matched without regard to case and spaces.
const UNTRUSTED: &[&str] = &[
    "github.event.issue.title",
    "github.event.issue.body",
    "github.event.pull_request.title",
    "github.event.pull_request.body",
    "github.event.pull_request.head.ref",
    "github.event.pull_request.head.label",
    "github.event.pull_request.head.repo.default_branch",
    "github.event.comment.body",
    "github.event.review.body",
    "github.event.review_comment.body",
    "github.event.pages.",
    "github.event.commits.",
    "github.event.head_commit.message",
    "github.event.head_commit.author.email",
    "github.event.head_commit.author.name",
    "github.event.workflow_run.head_branch",
    "github.event.workflow_run.display_title",
    "github.event.workflow_run.head_commit.message",
    "github.head_ref",
];

/// What in a `ref` or `repository` input names the pull request's head
/// (or its merge commit, which contains it).
const HEAD_REFS: &[&str] = &[
    "github.event.pull_request.head",
    "github.event.pull_request.merge_commit_sha",
    "github.head_ref",
    "GITHUB_HEAD_REF",
    "refs/pull/",
];

/// The git commands whose arguments can name the head.
const GIT_FETCHERS: &[&str] = &[
    "fetch",
    "checkout",
    "switch",
    "pull",
    "merge",
    "clone",
    "reset",
    "cherry-pick",
    "worktree",
    "restore",
];

/// Whether `on` names an event this module warns about: the workflows
/// that run attacker-influenced events with the base repository's
/// secrets.
#[must_use]
pub fn guarded(on: &crate::Triggers) -> bool {
    on.pull_request_target.is_some()
}

/// The findings in the steps of `plan`, in job and step order. The
/// caller decides whether the plan's event is one to warn about (see
/// [`guarded`]). Commands are already expanded in a plan, so a
/// checkout inside a command is found too.
#[must_use]
pub fn plan_lints(plan: &Plan) -> Vec<Lint> {
    let mut found = Vec::new();
    for job in &plan.jobs {
        for (i, step) in job.steps.iter().enumerate() {
            let mut add = |kind, message: String| {
                found.push(Lint {
                    kind,
                    workflow: plan.workflow.clone(),
                    job: job.id.clone(),
                    step: i + 1,
                    message,
                });
            };
            match step {
                PlannedStep::Checkout(checkout) if checks_out_head_native(checkout) => {
                    add(LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned());
                }
                PlannedStep::Uses(uses) => {
                    if checks_out_head_action(uses) {
                        add(LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned());
                    }
                    if let Some(field) = script_input(uses).and_then(injected) {
                        add(LintKind::ScriptInjection, injection(field));
                    }
                }
                PlannedStep::Run(run) => {
                    if checks_out_head_script(run) {
                        add(LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned());
                    }
                    if let Some(field) = injected(&run.run) {
                        add(LintKind::ScriptInjection, injection(field));
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// The findings of every workflow of `config` that runs on a guarded
/// event, as warnings. Each workflow is planned for `refs/heads/main`;
/// one that does not compile has its own errors (see
/// [`validate`](crate::validate)) and no findings here. A call of a file
/// of another repository plans nothing, so its steps are not read.
#[must_use]
pub fn lints(config: &Config) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for (name, wf) in &config.workflows {
        if !guarded(&wf.on) {
            continue;
        }
        let Ok(plan) = compile(config, name, "refs/heads/main") else {
            continue;
        };
        out.extend(
            plan_lints(&plan)
                .iter()
                .map(|lint| Diagnostic::new(lint.text())),
        );
    }
    out
}

fn injection(field: &str) -> String {
    format!(
        "interpolates `{field}` into a script, which lets whoever wrote it run commands; \
         pass it through `env:` and read it as a shell variable"
    )
}

/// Whether `text` names the pull request head.
fn names_head(text: &str) -> bool {
    HEAD_REFS.iter().any(|marker| text.contains(marker))
}

fn checks_out_head_native(step: &CheckoutStep) -> bool {
    step.r#ref.as_deref().is_some_and(names_head)
}

fn checks_out_head_action(step: &UsesStep) -> bool {
    let action = step.uses.to_ascii_lowercase();
    if action.split('@').next() != Some("actions/checkout") {
        return false;
    }
    ["ref", "repository"].iter().any(|input| {
        step.with
            .get(*input)
            .and_then(serde_json::Value::as_str)
            .is_some_and(names_head)
    })
}

/// Whether a script checks out the head: `gh pr checkout`, or a git
/// command on a line that names the head or a `pull/<n>/head` ref.
fn checks_out_head_script(step: &RunStep) -> bool {
    step.run.lines().any(|line| {
        let line = line.trim();
        if has_word_run(line, &["gh", "pr", "checkout"]) {
            return true;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let git = words
            .windows(2)
            .any(|pair| pair[0] == "git" && GIT_FETCHERS.contains(&pair[1]));
        git && (names_head(line) || pull_ref(line))
    })
}

/// Whether `words` appear in `line` one after another.
fn has_word_run(line: &str, words: &[&str]) -> bool {
    let all: Vec<&str> = line.split_whitespace().collect();
    all.windows(words.len()).any(|w| w == words)
}

/// Whether `line` has a ref of the form `pull/<n>/head` or
/// `pull/<n>/merge`, with or without the `refs/` before it.
fn pull_ref(line: &str) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find("pull/") {
        let after = &rest[at + "pull/".len()..];
        if let Some((_, tail)) = after.split_once('/') {
            for end in ["head", "merge"] {
                if let Some(more) = tail.strip_prefix(end)
                    && !more.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                {
                    return true;
                }
            }
        }
        rest = after;
    }
    false
}

/// The script of an `actions/github-script` step, which runs the text
/// as JavaScript.
fn script_input(step: &UsesStep) -> Option<&str> {
    let action = step.uses.to_ascii_lowercase();
    if action.split('@').next() != Some("actions/github-script") {
        return None;
    }
    step.with.get("script")?.as_str()
}

/// The first untrusted field that `script` interpolates with `${{ }}`.
fn injected(script: &str) -> Option<&'static str> {
    let mut rest = script;
    while let Some(start) = rest.find("${{") {
        let after = &rest[start + 3..];
        let end = after.find("}}")?;
        let body: String = after[..end]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        // A field read through an index (`github.event['issue']`) is
        // the same field; the quotes and brackets are dropped.
        let mut dotted = String::with_capacity(body.len());
        for c in body.chars() {
            let c = if matches!(c, '\'' | '"' | '[' | ']') {
                '.'
            } else {
                c
            };
            if c != '.' || !dotted.ends_with('.') {
                dotted.push(c);
            }
        }
        let body = dotted;
        if let Some(field) = UNTRUSTED.iter().find(|field| body.contains(*field)) {
            return Some(field.trim_end_matches('.'));
        }
        rest = &after[end + 2..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn config(on: &str, steps: &str) -> Config {
        let yaml = format!(
            "jobs:\n  t:\n    runs-on: ubuntu-latest\n    steps:\n{steps}\nworkflows:\n  w:\n    on: {on}\n    jobs: [t]\n"
        );
        parse(&yaml).unwrap()
    }

    fn found(steps: &str) -> Vec<LintKind> {
        let config = config("{ pull_request_target: {} }", steps);
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        plan_lints(&plan).iter().map(|l| l.kind).collect()
    }

    #[test]
    fn a_checkout_of_the_head_is_found() {
        for with in [
            "ref: ${{ github.event.pull_request.head.sha }}",
            "ref: ${{ github.event.pull_request.head.ref }}",
            "ref: ${{ github.head_ref }}",
            "ref: refs/pull/${{ github.event.number }}/merge",
            "ref: ${{ github.event.pull_request.merge_commit_sha }}",
            "repository: ${{ github.event.pull_request.head.repo.full_name }}",
        ] {
            let steps =
                format!("      - uses: actions/checkout@v4\n        with:\n          {with}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{with}");
        }
    }

    #[test]
    fn a_checkout_of_the_base_is_not_found() {
        assert_eq!(
            found("      - uses: actions/checkout@v4"),
            Vec::<LintKind>::new()
        );
        let steps = "      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.event.pull_request.base.sha }}";
        assert_eq!(found(steps), Vec::<LintKind>::new());
        assert_eq!(found("      - checkout"), Vec::<LintKind>::new());
    }

    #[test]
    fn the_native_checkout_step_is_read_too() {
        let steps = "      - checkout:\n          ref: ${{ github.event.pull_request.head.sha }}";
        assert_eq!(found(steps), [LintKind::HeadCheckout]);
    }

    #[test]
    fn scripts_that_fetch_or_check_out_the_head_are_found() {
        for script in [
            "gh pr checkout ${{ github.event.number }}",
            "git fetch origin refs/pull/${{ github.event.number }}/head",
            "git fetch origin pull/12/merge",
            "git checkout $GITHUB_HEAD_REF && git fetch origin ${{ github.head_ref }}",
            "git checkout ${{ github.event.pull_request.head.sha }}",
        ] {
            let steps = format!("      - run: |\n          {script}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{script}");
        }
        for script in [
            "git fetch origin main",
            "git log --oneline",
            "echo refs/pull/1/head",
            "git fetch origin pull/12/header",
        ] {
            let steps = format!("      - run: |\n          {script}");
            assert!(!found(&steps).contains(&LintKind::HeadCheckout), "{script}");
        }
    }

    #[test]
    fn untrusted_fields_in_a_script_are_found() {
        for field in [
            "github.event.pull_request.title",
            "github.event.pull_request.body",
            "github.event.pull_request.head.ref",
            "github.event.issue.title",
            "github.event.comment.body",
            "github.head_ref",
            "github.event.commits[0].message",
            "  github.event.pull_request.title  ",
            "contains(github.event.pull_request.title, 'x')",
            "github.event['pull_request']['title']",
        ] {
            let steps = format!("      - run: echo \"${{{{ {field} }}}}\"");
            assert_eq!(found(&steps), [LintKind::ScriptInjection], "{field}");
        }
    }

    #[test]
    fn safe_interpolations_and_env_passing_are_not_found() {
        for steps in [
            "      - run: echo ${{ github.event.pull_request.number }}",
            "      - run: echo ${{ github.sha }} ${{ github.event.pull_request.base.ref }}",
            "      - run: echo \"$TITLE\"\n        env:\n          TITLE: ${{ github.event.pull_request.title }}",
        ] {
            assert!(found(steps).is_empty(), "{steps}");
        }
    }

    #[test]
    fn a_github_script_with_a_title_in_its_script_is_found() {
        let steps = "      - uses: actions/github-script@v7\n        with:\n          script: |\n            core.info('${{ github.event.pull_request.title }}')";
        assert_eq!(found(steps), [LintKind::ScriptInjection]);
    }

    #[test]
    fn a_finding_names_the_workflow_job_and_step() {
        let config = config(
            "{ pull_request_target: {} }",
            "      - run: echo hi\n      - run: echo ${{ github.event.issue.title }}",
        );
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        let lints = plan_lints(&plan);
        assert_eq!(lints.len(), 1);
        assert!(
            lints[0]
                .text()
                .starts_with("workflow `w`, job `t`, step 2: ")
        );
    }

    #[test]
    fn only_a_pull_request_target_workflow_is_linted_in_a_config() {
        let step = "      - run: echo ${{ github.event.pull_request.title }}";
        assert_eq!(lints(&config("{ pull_request_target: {} }", step)).len(), 1);
        assert_eq!(lints(&config("{ pull_request: {} }", step)).len(), 0);
        assert_eq!(lints(&config("{ push: {} }", step)).len(), 0);
    }

    #[test]
    fn a_command_is_expanded_before_it_is_read() {
        let yaml = "
commands:
  grab:
    steps:
      - uses: actions/checkout@v4
        with:
          ref: ${{ github.event.pull_request.head.sha }}
jobs:
  t:
    runs-on: ubuntu-latest
    steps:
      - grab
workflows:
  w:
    on: { pull_request_target: {} }
    jobs: [t]
";
        let config = parse(yaml).unwrap();
        assert_eq!(lints(&config).len(), 1);
    }
}
