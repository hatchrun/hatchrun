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
//!   [`LintKind::UnverifiedCheckout`] is a checkout whose `ref` or
//!   `repository` is an expression that is not provably the base, so
//!   it may be the head; a repository that blocks head checkouts blocks
//!   it too.
//! - [`LintKind::ScriptInjection`]: a field the pull request's author
//!   writes (title, body, branch name) is interpolated with `${{ }}`
//!   straight into a `run:` script, where the shell reads it as code.
//!
//! Expressions are read from their parse, so `github.event.x`,
//! `github['event']['x']` and `GitHub.Event.X` are the same read. The
//! detectors still work on steps and not on what a script computes at
//! run time, so a workflow can be unsafe without a warning. A warning
//! never stops `validate` or `convert` from succeeding; the host decides
//! what a head checkout does (see the repository's
//! `pull_request_target_head_checkout` setting).

use crate::config::{CheckoutStep, RunStep, UsesStep};
use crate::expr::{Reads, reads};
use crate::plan::{Plan, PlannedStep};

/// What a lint found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintKind {
    /// The workflow checks out the pull request's head.
    HeadCheckout,
    /// The workflow checks out a ref or repository that is an
    /// expression not provably the base.
    UnverifiedCheckout,
    /// An untrusted event field is interpolated into a `run:` script.
    ScriptInjection,
}

impl LintKind {
    /// Whether a repository that blocks head checkouts blocks this.
    #[must_use]
    pub const fn is_head_checkout(self) -> bool {
        matches!(self, Self::HeadCheckout | Self::UnverifiedCheckout)
    }
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

/// The text of the unverified checkout finding.
const UNVERIFIED_CHECKOUT: &str = "checks out a ref or repository that is an expression not \
     provably the base branch, so in a pull_request_target workflow it may be the pull request \
     head, which runs the author's code with the base repository's secrets and write token; \
     use `github.sha`, `github.base_ref` or a literal";

/// Paths of an event that the author of an issue, comment or pull
/// request writes. A read of one of them, or of anything under it,
/// counts.
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
    "github.event.pages",
    "github.event.commits",
    "github.event.head_commit.message",
    "github.event.head_commit.author.email",
    "github.event.head_commit.author.name",
    "github.event.workflow_run.head_branch",
    "github.event.workflow_run.display_title",
    "github.event.workflow_run.head_commit.message",
    "github.head_ref",
];

/// Paths that name the pull request's head, or its merge commit, which
/// contains it.
const HEAD_PATHS: &[&str] = &[
    "github.event.pull_request.head",
    "github.event.pull_request.merge_commit_sha",
    "github.head_ref",
    "env.github_head_ref",
];

/// Paths a `ref` input may read and still be the base.
const BASE_REFS: &[&str] = &[
    "github.base_ref",
    "github.sha",
    "github.ref",
    "github.ref_name",
    "github.event.pull_request.base",
    "github.event.repository.default_branch",
];

/// Paths a `repository` input may read and still be the base.
const BASE_REPOSITORIES: &[&str] = &["github.repository", "github.event.pull_request.base"];

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
            let mut checkout = |kind: Option<LintKind>| match kind {
                Some(LintKind::HeadCheckout) => {
                    add(LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned());
                }
                Some(kind) => add(kind, UNVERIFIED_CHECKOUT.to_owned()),
                None => {}
            };
            match step {
                PlannedStep::Checkout(native) => checkout(native_checkout(native)),
                PlannedStep::Uses(uses) => {
                    checkout(action_checkout(uses));
                    if let Some(field) = script_input(uses).and_then(injected) {
                        add(LintKind::ScriptInjection, injection(&field));
                    }
                }
                PlannedStep::Run(run) => {
                    if checks_out_head_script(run) {
                        add(LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned());
                    }
                    if let Some(field) = injected(&run.run) {
                        add(LintKind::ScriptInjection, injection(&field));
                    }
                }
                _ => {}
            }
        }
    }
    found
}

fn injection(field: &str) -> String {
    format!(
        "interpolates `{field}` into a script, which lets whoever wrote it run commands; \
         pass it through `env:` and read it as a shell variable"
    )
}

/// Whether `path` is `under` or something below it.
fn under(path: &[String], under: &str) -> bool {
    let want: Vec<&str> = under.split('.').collect();
    path.len() >= want.len() && path.iter().zip(&want).all(|(a, b)| a == b)
}

fn reads_any(read: &Reads, list: &[&str]) -> bool {
    read.paths
        .iter()
        .any(|path| list.iter().any(|entry| under(path, entry)))
}

/// What the text of a `with.ref` reads cannot be parsed as: the text
/// with case, spaces and quotes removed, for a substring match.
fn squashed(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|c| !c.is_whitespace()) {
        let c = if matches!(c, '\'' | '"' | '[' | ']') {
            '.'
        } else {
            c.to_ascii_lowercase()
        };
        if c != '.' || !out.ends_with('.') {
            out.push(c);
        }
    }
    out
}

/// Whether `text` names the pull request head: a path of a `${{ }}` in
/// it, a ref of the form `refs/pull/<n>/...` or `pull/<n>/head|merge`
/// (in a string of an expression or outside one), or `GITHUB_HEAD_REF`.
fn names_head(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("refs/pull/") || lower.contains("github_head_ref") || pull_ref(&lower) {
        return true;
    }
    let Ok(list) = reads(text) else {
        // An expression that does not parse is read as text.
        let flat = squashed(text);
        return HEAD_PATHS.iter().any(|p| flat.contains(p));
    };
    list.iter().any(|read| {
        reads_any(read, HEAD_PATHS)
            || read
                .strings
                .iter()
                .any(|s| s.to_ascii_lowercase().contains("refs/pull/"))
    })
}

/// Whether the text of a `ref` or `repository` input is provably the
/// base: no expression, or only expressions that are one read of a path
/// in `allowed` (or one literal).
fn provably_base(text: &str, allowed: &[&str]) -> bool {
    let Ok(list) = reads(text) else {
        return false;
    };
    list.iter().all(|read| {
        read.plain
            && read
                .paths
                .iter()
                .all(|path| allowed.iter().any(|entry| under(path, entry)))
    })
}

/// How a checkout's `ref` and `repository` rate: the head, an
/// expression that may be, or fine.
fn rate_checkout(inputs: &[(&str, &[&str])]) -> Option<LintKind> {
    if inputs.iter().any(|(text, _)| names_head(text)) {
        return Some(LintKind::HeadCheckout);
    }
    inputs
        .iter()
        .any(|(text, allowed)| !provably_base(text, allowed))
        .then_some(LintKind::UnverifiedCheckout)
}

fn native_checkout(step: &CheckoutStep) -> Option<LintKind> {
    let text = step.r#ref.as_deref()?;
    rate_checkout(&[(text, BASE_REFS)])
}

fn action_checkout(step: &UsesStep) -> Option<LintKind> {
    let action = step.uses.to_ascii_lowercase();
    if action.split('@').next() != Some("actions/checkout") {
        return None;
    }
    let mut inputs: Vec<(&str, &[&str])> = Vec::new();
    for (name, allowed) in [("ref", BASE_REFS), ("repository", BASE_REPOSITORIES)] {
        if let Some(text) = step.with.get(name).and_then(serde_json::Value::as_str) {
            inputs.push((text, allowed));
        }
    }
    rate_checkout(&inputs)
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

/// The first untrusted field that `script` interpolates with `${{ }}`,
/// as a dotted path.
fn injected(script: &str) -> Option<String> {
    let Ok(list) = reads(script) else {
        // An expression that does not parse is read as text.
        let flat = squashed(script);
        return UNTRUSTED
            .iter()
            .find(|entry| flat.contains(*entry))
            .map(|entry| (*entry).to_owned());
    };
    list.iter().find_map(|read| {
        read.paths.iter().find_map(|path| {
            UNTRUSTED
                .iter()
                .find(|entry| under(path, entry))
                .map(|entry| (*entry).to_owned())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, compile, parse, validate_with_warnings};

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
    fn every_spelling_of_the_head_is_the_head() {
        for expr in [
            "github.event.pull_request.head.sha",
            "github['event']['pull_request']['head']['sha']",
            "github.event.pull_request['head'].sha",
            "GitHub.Event.Pull_Request.Head.SHA",
            "github[\"event\"].pull_request.head.ref",
            "github.event.pull_request.MERGE_COMMIT_SHA",
            "github.HEAD_REF",
            "github['head_ref']",
            "format('{0}', github.event.pull_request.head.sha)",
            "github.head_ref || 'main'",
            "format('refs/pull/{0}/head', github.event.number)",
            "toJSON(github.event.pull_request.head)",
        ] {
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: ${{{{ {expr} }}}}"
            );
            assert_eq!(found(&steps), [LintKind::HeadCheckout], "{expr}");
            let steps = format!("      - run: git checkout ${{{{ {expr} }}}}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{expr}");
        }
        for text in ["$GITHUB_HEAD_REF", "$github_head_ref", "${GITHUB_HEAD_REF}"] {
            let steps = format!("      - run: git fetch origin {text}");
            assert!(found(&steps).contains(&LintKind::HeadCheckout), "{text}");
        }
    }

    #[test]
    fn a_ref_that_is_not_provably_the_base_is_unverified() {
        for expr in [
            "matrix.ref",
            "inputs.ref",
            "env.REF",
            "steps.x.outputs.ref",
            "fromJSON('{\"a\":1}').a",
            "github.event.inputs.ref",
            "github.event.pull_request.base.ref == 'x' && matrix.r",
        ] {
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: ${{{{ {expr} }}}}"
            );
            assert_eq!(found(&steps), [LintKind::UnverifiedCheckout], "{expr}");
        }
        let steps = "      - uses: actions/checkout@v4\n        with:\n          repository: ${{ matrix.repo }}";
        assert_eq!(found(steps), [LintKind::UnverifiedCheckout]);
        let steps = "      - checkout:\n          ref: ${{ matrix.ref }}";
        assert_eq!(found(steps), [LintKind::UnverifiedCheckout]);
        assert!(LintKind::UnverifiedCheckout.is_head_checkout());
        assert!(!LintKind::ScriptInjection.is_head_checkout());
    }

    #[test]
    fn provably_base_refs_are_clean() {
        for ref_ in [
            "main",
            "refs/heads/release",
            "${{ github.base_ref }}",
            "${{ github.sha }}",
            "${{ github.ref }}",
            "${{ GitHub['Base_Ref'] }}",
            "${{ github.event.pull_request.base.sha }}",
            "${{ github.event.pull_request['base']['ref'] }}",
            "${{ 'main' }}",
        ] {
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: \"{ref_}\""
            );
            assert_eq!(found(&steps), Vec::<LintKind>::new(), "{ref_}");
        }
        let steps = "      - uses: actions/checkout@v4\n        with:\n          repository: ${{ github.repository }}";
        assert_eq!(found(steps), Vec::<LintKind>::new());
    }

    #[test]
    fn injection_is_found_in_every_spelling() {
        for expr in [
            "GitHub.Event.Pull_Request.Title",
            "github['event']['pull_request']['title']",
            "github.event.pull_request['body']",
            "github['head_ref']",
            "github.event.commits[0].message",
            "github.event.commits.*.message",
        ] {
            let steps = format!("      - run: echo \"${{{{ {expr} }}}}\"");
            assert_eq!(found(&steps), [LintKind::ScriptInjection], "{expr}");
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
        assert_eq!(
            validate_with_warnings(&config("{ pull_request_target: {} }", step))
                .1
                .len(),
            1
        );
        assert_eq!(
            validate_with_warnings(&config("{ pull_request: {} }", step))
                .1
                .len(),
            0
        );
        assert_eq!(
            validate_with_warnings(&config("{ push: {} }", step))
                .1
                .len(),
            0
        );
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
        assert_eq!(validate_with_warnings(&config).1.len(), 1);
    }
}
