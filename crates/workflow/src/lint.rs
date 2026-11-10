// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Warnings about workflows that run attacker-influenced input with the
//! base repository's secrets.
//!
//! A `pull_request_target` workflow runs trusted (secrets, a write
//! token) on an event that anyone can cause. Two mistakes turn that into
//! remote code execution, and both are found by reading the plan:
//!
//! - [`LintKind::HeadCheckout`]: the workflow gets the pull request's
//!   head (the fork's code) onto the runner and runs it. A job is
//!   *tainted* when anything in it reads the head: a path of the
//!   `github` context at or above `github.event.pull_request.head`, the
//!   head ref, a `refs/pull/<n>/head` ref, `GITHUB_HEAD_REF`. The read can
//!   be anywhere in the job (its env, a step's env, `with:`, `if:`, a
//!   script), so passing the value through `env:` to a script does not
//!   hide it. A tainted job that checks out code or runs a command that
//!   fetches it is a head checkout. [`LintKind::UnverifiedCheckout`] is a
//!   checkout whose `ref` or `repository` is an expression that is not
//!   provably the base. A repository that blocks head checkouts blocks
//!   both.
//! - [`LintKind::ScriptInjection`]: a field the pull request's author
//!   writes (title, body, branch name) is interpolated with `${{ }}`
//!   straight into a `run:` script, where the shell reads it as code.
//!
//! Expressions are read from their parse, so `github.event.x`,
//! `github['event']['x']` and `GitHub.Event.X` are the same read, a
//! computed index (`github.event.pull_request[format(...)]`) can be any
//! name, and reading a parent (`toJSON(github.event.pull_request)`) reads
//! what is under it. The detectors still work on the plan and not on what
//! a script computes at run time, so a workflow can be unsafe without a
//! warning. A warning never stops `validate` or `convert` from
//! succeeding; the host decides what a head checkout does (see the
//! repository's `pull_request_target_head_checkout` setting).

use serde_json::Value;

use crate::config::{CheckoutStep, UsesStep};
use crate::expr::{reads, reads_expression};
use crate::plan::{Plan, PlannedJob, PlannedStep};

/// What a lint found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintKind {
    /// The workflow gets the pull request's head onto the runner.
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
/// request writes.
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

/// The git commands that fetch code or move a checkout to it.
const GIT_FETCHERS: &[&str] = &[
    "fetch",
    "checkout",
    "clone",
    "pull",
    "switch",
    "worktree",
    "merge",
    "reset",
    "cherry-pick",
    "restore",
];

/// Whether `on` names an event this module warns about: the workflows
/// that run attacker-influenced events with the base repository's
/// secrets.
#[must_use]
pub fn guarded(on: &crate::Triggers) -> bool {
    on.pull_request_target.is_some()
}

/// The findings in the jobs of `plan`, in job and step order. The
/// caller decides whether the plan's event is one to warn about (see
/// [`guarded`]). Commands and called workflows are already expanded in a
/// plan, so their steps are read too.
#[must_use]
pub fn plan_lints(plan: &Plan) -> Vec<Lint> {
    let mut found = Vec::new();
    for job in &plan.jobs {
        let taint = job_taint(job);
        let tainted = |what: &str| {
            taint.as_ref().map(|reason| {
                format!(
                    "{what} in a job that reads the pull request head ({reason}) in a \
                     pull_request_target workflow, so the code of the pull request can run with \
                     the base repository's secrets and write token; keep the head out of this job \
                     and run the pull request's code in a pull_request workflow instead"
                )
            })
        };
        for (i, step) in job.steps.iter().enumerate() {
            let (kind, message) = match step {
                PlannedStep::Checkout(native) => {
                    match checkout_finding(native_checkout(native), tainted("checks out code")) {
                        Some(found) => found,
                        None => continue,
                    }
                }
                PlannedStep::Uses(uses) => {
                    if let Some(field) = script_input(uses).and_then(injected) {
                        found.push(lint(
                            plan,
                            job,
                            i,
                            LintKind::ScriptInjection,
                            injection(&field),
                        ));
                    }
                    if is_action(uses, "actions/checkout") {
                        match checkout_finding(action_checkout(uses), tainted("checks out code")) {
                            Some(found) => found,
                            None => continue,
                        }
                    } else {
                        continue;
                    }
                }
                PlannedStep::Run(run) => {
                    if let Some(field) = injected(&run.run) {
                        found.push(lint(
                            plan,
                            job,
                            i,
                            LintKind::ScriptInjection,
                            injection(&field),
                        ));
                    }
                    if has_word_run(&run.run, &["gh", "pr", "checkout"]) {
                        (LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned())
                    } else if fetches_code(&run.run)
                        && let Some(message) = tainted("runs a command that fetches code")
                    {
                        (LintKind::HeadCheckout, message)
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };
            found.push(lint(plan, job, i, kind, message));
        }
    }
    found
}

fn lint(plan: &Plan, job: &PlannedJob, step: usize, kind: LintKind, message: String) -> Lint {
    Lint {
        kind,
        workflow: plan.workflow.clone(),
        job: job.id.clone(),
        step: step + 1,
        message,
    }
}

/// What a checkout step is found to be: what its own inputs rate, else
/// a head checkout when its job is tainted.
fn checkout_finding(
    rated: Option<LintKind>,
    tainted: Option<String>,
) -> Option<(LintKind, String)> {
    match rated {
        Some(LintKind::HeadCheckout) => Some((LintKind::HeadCheckout, HEAD_CHECKOUT.to_owned())),
        Some(kind) => Some((kind, UNVERIFIED_CHECKOUT.to_owned())),
        None => tainted.map(|message| (LintKind::HeadCheckout, message)),
    }
}

fn injection(field: &str) -> String {
    format!(
        "interpolates `{field}` into a script, which lets whoever wrote it run commands; \
         pass it through `env:` and read it as a shell variable"
    )
}

/// Whether `path` reads `entry`: the same place, a place below it, or a
/// parent of it (`github.event` reads everything under it). A step of
/// `*` (a computed index) can be any name.
fn touches(path: &[String], entry: &str) -> bool {
    path.iter()
        .zip(entry.split('.'))
        .all(|(step, want)| step == "*" || step == want)
}

/// Whether `path` is `under` the dotted `entry`, or the entry itself: a
/// stricter match than [`touches`], for what must be provably the base.
fn under(path: &[String], entry: &str) -> bool {
    let want: Vec<&str> = entry.split('.').collect();
    path.len() >= want.len() && path.iter().zip(&want).all(|(a, b)| a == b)
}

/// The text with case, spaces and quotes removed, for a substring match
/// on an expression that does not parse.
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

/// What reading `text` says about the pull request head, `None` when it
/// says nothing: the path it reads, or the ref it names. `bare` text is
/// one expression without `${{ }}`, as an `if:` is. Text with an
/// expression that does not parse counts as reading it.
fn head_read(text: &str, bare: bool) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("github_head_ref") {
        return Some("GITHUB_HEAD_REF".to_owned());
    }
    if pull_ref(&lower) {
        return Some("a refs/pull ref".to_owned());
    }
    let parsed = if bare && !text.contains("${{") {
        reads_expression(text).map(|read| vec![read])
    } else {
        reads(text)
    };
    let Ok(list) = parsed else {
        let flat = squashed(text);
        return HEAD_PATHS
            .iter()
            .any(|entry| flat.contains(entry))
            .then(|| "an expression that cannot be read".to_owned());
    };
    for read in &list {
        for path in &read.paths {
            if HEAD_PATHS.iter().any(|entry| touches(path, entry)) {
                return Some(path.join("."));
            }
        }
        if read
            .strings
            .iter()
            .any(|s| pull_ref(&s.to_ascii_lowercase()))
        {
            return Some("a refs/pull ref".to_owned());
        }
    }
    None
}

/// Whether `text` names the pull request head.
fn names_head(text: &str) -> bool {
    head_read(text, false).is_some()
}

/// Why the job is tainted: the first read of the pull request head found
/// anywhere in it (its env, a step's env, `with:`, `if:`, a script, its
/// container and services), else `None`.
fn job_taint(job: &PlannedJob) -> Option<String> {
    let value = serde_json::to_value(job).ok()?;
    taint_in(&value, false)
}

fn taint_in(value: &Value, bare: bool) -> Option<String> {
    match value {
        Value::String(text) => head_read(text, bare),
        Value::Array(items) => items.iter().find_map(|item| taint_in(item, false)),
        Value::Object(fields) => fields.iter().find_map(|(key, item)| {
            taint_in(
                item,
                matches!(key.as_str(), "if" | "if_" | "continue_on_error"),
            )
        }),
        _ => None,
    }
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

fn is_action(step: &UsesStep, name: &str) -> bool {
    step.uses.to_ascii_lowercase().split('@').next() == Some(name)
}

fn action_checkout(step: &UsesStep) -> Option<LintKind> {
    let mut inputs: Vec<(&str, &[&str])> = Vec::new();
    for (name, allowed) in [("ref", BASE_REFS), ("repository", BASE_REPOSITORIES)] {
        if let Some(text) = step.with.get(name).and_then(Value::as_str) {
            inputs.push((text, allowed));
        }
    }
    rate_checkout(&inputs)
}

/// Whether a script runs a command that fetches code or moves a
/// checkout to it: a git command such as `fetch` or `checkout` (within
/// a few words of `git`, which covers `git -C dir fetch`), `gh pr
/// checkout` or `gh repo clone`.
fn fetches_code(script: &str) -> bool {
    let flat: String = script
        .chars()
        .map(|c| {
            if matches!(c, ';' | '&' | '|' | '(' | ')') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    words.iter().enumerate().any(|(i, word)| match *word {
        "git" => words[i + 1..]
            .iter()
            .take(4)
            .any(|w| GIT_FETCHERS.contains(w)),
        "gh" => matches!(
            words.get(i + 1..i + 3),
            Some(["pr", "checkout"] | ["repo", "clone"])
        ),
        _ => false,
    })
}

/// Whether `words` appear in `text` one after another.
fn has_word_run(text: &str, words: &[&str]) -> bool {
    let all: Vec<&str> = text.split_whitespace().collect();
    all.windows(words.len()).any(|w| w == words)
}

/// Whether `c` can be in a git ref name.
fn is_ref_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')
}

/// The text after a pull request number at the start of `text`: digits,
/// a `${{ }}`, a `${VAR}`, a `$VAR` or a `{0}` placeholder.
fn after_number(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix("${{") {
        return rest.split_once("}}").map(|(_, rest)| rest);
    }
    if let Some(rest) = text.strip_prefix("${") {
        return rest.split_once('}').map(|(_, rest)| rest);
    }
    if let Some(rest) = text.strip_prefix('{') {
        return rest.split_once('}').map(|(_, rest)| rest);
    }
    let name = if let Some(rest) = text.strip_prefix('$') {
        rest.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_')
    } else {
        text.trim_start_matches(|c: char| c.is_ascii_digit())
    };
    (name.len() < text.len()).then_some(name)
}

/// Whether `text` (lowercase) has a ref of the form
/// `refs/pull/<n>/head` or `refs/pull/<n>/merge`, with or without the
/// `refs/` before it. It starts where a ref starts and ends where it
/// ends, so `release/pull/2/head-fix` and `pull/2/header` are other
/// branches.
fn pull_ref(text: &str) -> bool {
    let mut from = 0;
    while let Some(found) = text[from..].find("pull/") {
        let at = from + found;
        from = at + "pull/".len();
        let before = &text[..at];
        let before = before.strip_suffix("refs/").unwrap_or(before);
        if before.chars().next_back().is_some_and(is_ref_char) {
            continue;
        }
        let Some(rest) = after_number(&text[from..]).and_then(|rest| rest.strip_prefix('/')) else {
            continue;
        };
        for end in ["head", "merge"] {
            if let Some(more) = rest.strip_prefix(end)
                && !more.chars().next().is_some_and(is_ref_char)
            {
                return true;
            }
        }
    }
    false
}

/// The script of an `actions/github-script` step, which runs the text
/// as JavaScript.
fn script_input(step: &UsesStep) -> Option<&str> {
    if !is_action(step, "actions/github-script") {
        return None;
    }
    step.with.get("script")?.as_str()
}

/// The first untrusted field that `script` interpolates with `${{ }}`,
/// as a dotted path. Reading a parent of one (`toJSON(github.event)`)
/// reads it.
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
                .find(|entry| touches(path, entry))
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
        found_in("", steps)
    }

    /// The findings of a job with `job` (more keys of the job) and `steps`.
    fn found_in(job: &str, steps: &str) -> Vec<LintKind> {
        let yaml = format!(
            "jobs:\n  t:\n    runs-on: ubuntu-latest\n{job}    steps:\n{steps}\n\
             workflows:\n  w:\n    on: {{ pull_request_target: {{}} }}\n    jobs: [t]\n"
        );
        let config = parse(&yaml).unwrap();
        let plan = compile(&config, "w", "refs/heads/main").unwrap();
        plan_lints(&plan).iter().map(|l| l.kind).collect()
    }

    /// Whether `block` would refuse the job.
    fn blocked(job: &str, steps: &str) -> bool {
        found_in(job, steps).iter().any(|k| k.is_head_checkout())
    }

    #[test]
    fn the_head_passed_through_env_to_a_fetch_is_blocked() {
        let head = "${{ github.event.pull_request.head.sha }}";
        // A step's env.
        let steps = format!(
            "      - run: git fetch origin $PR_SHA && git checkout $PR_SHA\n        env:\n          PR_SHA: {head}"
        );
        assert!(blocked("", &steps));
        // The job's env.
        let job = format!("    env:\n      PR_SHA: {head}\n");
        assert!(blocked(&job, "      - run: git fetch origin $PR_SHA"));
        // A checkout that has nothing to do with the head in its inputs,
        // in a job that reads the head elsewhere.
        assert!(blocked(
            &job,
            "      - uses: actions/checkout@v4\n        with:\n          ref: main"
        ));
        assert!(blocked(&job, "      - checkout"));
        // Read by an `if:`, a `with:` of another action or a later step.
        let steps = "      - run: echo hi\n        if: github.event.pull_request.head.repo.fork\n      - run: git clone https://example.com/x\n";
        assert!(blocked("", steps));
        let steps = format!(
            "      - uses: some/action@v1\n        with:\n          sha: {head}\n      - run: gh repo clone o/r"
        );
        assert!(blocked("", &steps));
    }

    #[test]
    fn computed_indexes_and_parents_of_the_head_are_blocked() {
        for expr in [
            "github.event.pull_request[format('h{0}', 'ead')].sha",
            "github.event.pull_request[inputs.field].sha",
            "github.event[matrix.k].head.sha",
            "toJSON(github.event.pull_request)",
            "toJSON(github.event)",
            "toJSON(github)",
            "github.event.pull_request",
            "format('{0}', github.event.pull_request)",
            "fromJSON(toJSON(github.event.pull_request)).head.sha",
        ] {
            let steps = format!("      - run: git fetch origin \"${{{{ {expr} }}}}\"");
            assert!(blocked("", &steps), "{expr}");
            let steps = format!(
                "      - uses: actions/checkout@v4\n        with:\n          ref: ${{{{ {expr} }}}}"
            );
            assert!(blocked("", &steps), "{expr}");
        }
    }

    #[test]
    fn a_job_that_never_reads_the_head_is_not_blocked_whatever_it_fetches() {
        for steps in [
            "      - run: git fetch origin main && git checkout release",
            "      - run: git clone https://example.com/x && make",
            "      - run: git fetch origin release/pull/2/head-fix",
            "      - run: git fetch origin refs/heads/pull/1/head",
            "      - run: git fetch origin pull/2/header",
            "      - run: git fetch origin pull/head",
            "      - uses: actions/checkout@v4\n        with:\n          ref: ${{ github.sha }}",
            "      - run: git fetch origin ${{ github.event.pull_request.base.sha }}",
            "      - run: echo ${{ github.event.pull_request.number }} ${{ github.event.pull_request.labels.*.name }}",
        ] {
            assert!(!blocked("", steps), "{steps}");
        }
        // The head read in a job that fetches nothing is only a warning
        // for the job that checks out.
        assert!(!blocked(
            "",
            "      - run: echo ${{ github.event.pull_request.head.sha }}"
        ));
    }

    #[test]
    fn a_pull_ref_is_a_whole_ref_and_nothing_longer() {
        for yes in [
            "pull/12/head",
            "refs/pull/12/merge",
            "+refs/pull/5/head:refs/remotes/pr/5",
            "git fetch origin pull/7/merge",
            "refs/pull/${{ github.event.number }}/head",
            "refs/pull/$PR/head",
            "refs/pull/${pr}/merge",
            "refs/pull/{0}/head",
            "\"refs/pull/3/head\"",
        ] {
            assert!(pull_ref(&yes.to_ascii_lowercase()), "{yes}");
        }
        for no in [
            "release/pull/2/head-fix",
            "refs/heads/pull/1/head",
            "origin/pull/2/head",
            "pull/2/header",
            "pull/2/head_old",
            "pull/head",
            "refs/pull/x/head",
            "pull/2/heads",
            "pull/2/merged",
        ] {
            assert!(!pull_ref(no), "{no}");
        }
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
