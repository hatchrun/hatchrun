// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `uses:` steps that run JavaScript actions: inputs, the `pre`, main
//! and `post` scripts and the Node binary they run on.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use hatchrun_workflow::{Conclusion, PlannedStep, UsesStep};
use serde_json::Value;

use super::action::{self, Action, ActionRef, Runs};
use super::context::ActionScope;
use super::files::StepFiles;
use super::sink::Sink;
use super::{Common, Job, Launch, Line, Options, Ran, base_env, interp};

/// Where the runner image keeps its Node builds.
const EXTERNALS: &str = "/home/runner/externals";

/// The Node binary for an action's `runs.using` value.
///
/// `node12`, `node16` and `node20` run on `node20`, everything else on
/// `node24`. The binary is `--node` when given, else the image's
/// `externals/<runtime>/bin/node`, else `node` on the agent's `PATH`.
///
/// # Errors
///
/// When no Node binary can be found.
pub fn node_binary(opts: &Options, version: &str) -> Result<PathBuf> {
    locate(
        opts.node.as_deref(),
        Path::new(EXTERNALS),
        std::env::var_os("PATH"),
        version,
    )
}

fn locate(
    explicit: Option<&Path>,
    externals: &Path,
    path: Option<OsString>,
    version: &str,
) -> Result<PathBuf> {
    if let Some(node) = explicit {
        return Ok(node.to_owned());
    }
    // An image that lacks the preferred build still has the other one.
    let runtimes = match version {
        "node12" | "node16" | "node20" => ["node20", "node24"],
        _ => ["node24", "node20"],
    };
    for runtime in runtimes {
        let candidate = externals.join(runtime).join("bin/node");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // The step's own PATH may not hold node, so resolve it here.
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("node");
            if dir.is_absolute() && candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    bail!("no Node binary found for `{version}`: pass --node or install one in {EXTERNALS}")
}

/// A `post` script that is due after the job's steps.
pub struct Post {
    /// The step's index, for its `STATE_*`.
    index: usize,
    /// Which file command files it uses.
    slot: usize,
    /// The `uses:` value, for the header.
    uses: String,
    action_name: String,
    node: PathBuf,
    script: PathBuf,
    condition: Option<String>,
    /// The step's `env`, its inputs and the action variables.
    extra: BTreeMap<String, String>,
    continue_on_error: bool,
}

/// The inputs and environment a `uses:` step gives its action.
pub(super) struct Evaluated {
    /// The step's `env`, `INPUT_*` and the `GITHUB_ACTION_*` variables.
    pub(super) env: BTreeMap<String, String>,
    /// The step's `env` alone.
    pub(super) step_env: BTreeMap<String, String>,
    /// The action's scope, with every input and its default.
    pub(super) scope: ActionScope,
    pub(super) warnings: Vec<String>,
}

/// The `GITHUB_ACTION` of a `uses:` step: its id, else a generated name.
pub(super) fn action_name(step: &UsesStep, r: &ActionRef) -> String {
    match (&step.id, r) {
        (Some(id), _) => id.clone(),
        (None, ActionRef::Remote { owner, repo, .. }) => format!("__{owner}_{repo}"),
        (None, ActionRef::Local(_)) => "__run".to_owned(),
    }
}

impl Job<'_> {
    /// Fetches every action the steps use and runs their `pre` scripts,
    /// as GitHub does before the first step. A local action that is not
    /// in the workspace yet, say because a checkout step brings it, is
    /// fetched when its step runs and has no `pre`.
    pub(super) async fn pre_phase(&mut self, sink: &mut dyn Sink) {
        let doc = self.doc;
        for (index, step) in doc.job.steps.iter().enumerate() {
            let PlannedStep::Uses(uses) = step else {
                continue;
            };
            let Ok(r) = action::parse_ref(&uses.uses) else {
                continue;
            };
            let found = self.fetch_action(&r).await;
            if matches!((&r, &found), (ActionRef::Local(_), Err(_))) {
                continue;
            }
            self.fetched.insert(index, found);
        }
        for (index, step) in doc.job.steps.iter().enumerate() {
            let PlannedStep::Uses(uses) = step else {
                continue;
            };
            if self.aborted {
                break;
            }
            let Some(Ok(action)) = self.fetched.get(&index).cloned() else {
                continue;
            };
            let Ok(r) = action::parse_ref(&uses.uses) else {
                continue;
            };
            if let Runs::Node {
                version,
                pre: Some(pre),
                pre_if,
                ..
            } = &action.meta.runs
            {
                let hook = Hook {
                    index,
                    slot: doc.job.steps.len() + index,
                    version,
                    script: pre,
                    condition: pre_if.as_deref(),
                };
                self.run_pre(&hook, uses, &action, &r, sink).await;
            }
        }
    }

    /// Fetches an action, giving up when the job is stopped. The first
    /// stop request ends a download at once, since nothing that is
    /// waiting for it can run any more; once the grace period of a
    /// stopped job is running, its end and a second request do.
    pub(super) async fn fetch_action(&mut self, r: &ActionRef) -> Result<Action, String> {
        let limits = action::Limits {
            owner: self.owner,
            ..action::LIMITS
        };
        let first = self.grace_until.is_none();
        let deadline = self.grace_until;
        let mut stop = super::wait_for(self.cancel_requests.clone(), if first { 1 } else { 2 });
        let fetch = action::fetch_with(r, &self.doc.github, &self.layout, &self.actions, limits);
        let fetched = tokio::select! {
            found = fetch => Some(found),
            () = &mut stop => None,
            () = async {
                match deadline {
                    Some(end) => tokio::time::sleep_until(end.into()).await,
                    None => std::future::pending().await,
                }
            } => None,
        };
        if let Some(found) = fetched {
            return found.map_err(|e| format!("{e:#}"));
        }
        self.note_cancel();
        Err("The operation was canceled while downloading the action.".to_owned())
    }

    async fn run_pre(
        &mut self,
        hook: &Hook<'_>,
        uses: &UsesStep,
        action: &Action,
        r: &ActionRef,
        sink: &mut dyn Sink,
    ) {
        match self.hook_runs(Some(hook.condition.unwrap_or("always()"))) {
            Ok(true) => {}
            Ok(false) => return,
            Err(err) => {
                self.announce(sink, hook.index, "Pre", &uses.uses);
                self.say(sink, Line::Error(&format!("pre-if: {err}")));
                self.settle(Conclusion::Failure, uses.continue_on_error);
                return;
            }
        }
        self.announce(sink, hook.index, "Pre", &uses.uses);
        let outcome = match self.prepare_hook(hook, uses, action, r) {
            Ok((node, script, name, evaluated)) => {
                for warning in &evaluated.warnings {
                    self.say(sink, Line::Warning(warning));
                }
                self.run_hook(hook, &node, &script, &name, &evaluated.env, sink)
                    .await
                    .outcome
            }
            Err(message) => {
                self.say(sink, Line::Error(&message));
                Conclusion::Failure
            }
        };
        self.settle(outcome, uses.continue_on_error);
    }

    /// Runs the `post` scripts queued by the steps that ran, last first.
    pub(super) async fn post_phase(&mut self, sink: &mut dyn Sink) {
        while let Some(post) = self.posts.pop() {
            if self.aborted {
                self.posts.clear();
                break;
            }
            match self.hook_runs(Some(post.condition.as_deref().unwrap_or("always()"))) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(err) => {
                    self.announce(sink, post.index, "Post", &post.uses);
                    self.say(sink, Line::Error(&format!("post-if: {err}")));
                    self.settle(Conclusion::Failure, post.continue_on_error);
                    continue;
                }
            }
            self.announce(sink, post.index, "Post", &post.uses);
            let hook = Hook {
                index: post.index,
                slot: post.slot,
                version: "",
                script: "",
                condition: None,
            };
            let ran = self
                .run_hook(
                    &hook,
                    &post.node,
                    &post.script,
                    &post.action_name,
                    &post.extra,
                    sink,
                )
                .await;
            self.settle(ran.outcome, post.continue_on_error);
        }
    }

    /// Starts the log section of a `pre` or `post` script. It reuses the
    /// main step's index and has no summary of its own.
    fn announce(&mut self, sink: &mut dyn Sink, index: usize, phase: &str, uses: &str) {
        let name = self
            .masker
            .mask(&format!("{phase} Run {uses}"))
            .into_owned();
        sink.step_started(index, &name);
    }

    /// A `uses:` step whose action is fetched or local.
    pub(super) async fn uses_step(
        &mut self,
        index: usize,
        step: &UsesStep,
        sink: &mut dyn Sink,
    ) -> (Common, Conclusion, BTreeMap<String, String>) {
        let workspace = self.layout.workspace.clone();
        let mut name_error = None;
        let name = match &step.name {
            Some(name) => interp(name, &self.ctx, &workspace, self.status).unwrap_or_else(|err| {
                name_error = Some(format!("name: {err}"));
                name.clone()
            }),
            None => format!("Run {}", step.uses),
        };
        let common = Common {
            name,
            id: step.id.clone(),
            if_: step.if_.clone(),
            continue_on_error: step.continue_on_error,
        };
        sink.step_started(index, &self.masker.mask(&common.name));
        if let Some(message) = name_error {
            self.say(sink, Line::Warning(&message));
        }
        let failed = |job: &mut Self, sink: &mut dyn Sink, common, message: &str| {
            job.say(sink, Line::Error(message));
            (common, Conclusion::Failure, BTreeMap::new())
        };

        match self.runs(common.if_.as_deref()) {
            Ok(true) => {}
            Ok(false) => return (common, Conclusion::Skipped, BTreeMap::new()),
            Err(err) => return failed(self, sink, common, &format!("if: {err}")),
        }
        if let Some(message) = self.env_error.clone() {
            return failed(self, sink, common, &message);
        }

        let r = match action::parse_ref(&step.uses) {
            Ok(r) => r,
            Err(message) => return failed(self, sink, common, &message),
        };
        // A local action may have arrived since the job started.
        let action = match (&r, self.fetched.get(&index)) {
            (ActionRef::Remote { .. }, Some(found)) => found.clone(),
            _ => self.fetch_action(&r).await,
        };
        let action = match action {
            Ok(action) => action,
            Err(message) => return failed(self, sink, common, &message),
        };
        if let Runs::Composite { steps } = &action.meta.runs {
            let (outcome, outputs) = self.composite(step, &action, &r, steps, sink).await;
            return (common, outcome, outputs);
        }
        let Runs::Node {
            version,
            main,
            post,
            post_if,
            ..
        } = &action.meta.runs
        else {
            let message = format!(
                "`{}` is a Docker container action; Docker services and containers come in a later release",
                step.uses
            );
            return failed(self, sink, common, &message);
        };

        let hook = Hook {
            index,
            slot: index,
            version,
            script: main,
            condition: None,
        };
        let (node, script, name, evaluated) = match self.prepare_hook(&hook, step, &action, &r) {
            Ok(prepared) => prepared,
            Err(message) => return failed(self, sink, common, &message),
        };
        for warning in &evaluated.warnings {
            self.say(sink, Line::Warning(warning));
        }
        if let Some(post) = post {
            let due = Post {
                index,
                slot: 0,
                uses: step.uses.clone(),
                action_name: name.clone(),
                node: node.clone(),
                script: action.dir.join(post),
                condition: post_if.clone(),
                extra: evaluated.env.clone(),
                continue_on_error: step.continue_on_error,
            };
            self.queue_post(due);
        }
        let ran = self
            .run_hook(&hook, &node, &script, &name, &evaluated.env, sink)
            .await;
        (common, ran.outcome, ran.outputs)
    }

    /// Queues a `post` that is due at the end of the job. Inside a
    /// composite action the step has no slot reserved for it, and the
    /// composite step's `env` still applies to it.
    fn queue_post(&mut self, mut post: Post) {
        let steps = self.doc.job.steps.len();
        post.slot = if post.index < steps {
            2 * steps + post.index
        } else {
            self.fresh_slot()
        };
        let mut extra = self.scope_env.clone();
        extra.extend(std::mem::take(&mut post.extra));
        post.extra = extra;
        self.posts.push(post);
    }

    /// The Node binary, the script's path, `GITHUB_ACTION` and the
    /// inputs for a step's hook.
    fn prepare_hook(
        &self,
        hook: &Hook<'_>,
        step: &UsesStep,
        action: &Action,
        r: &ActionRef,
    ) -> Result<(PathBuf, PathBuf, String, Evaluated), String> {
        let node = node_binary(self.opts, hook.version).map_err(|e| format!("{e:#}"))?;
        let name = action_name(step, r);
        let evaluated = self.evaluate(step, action, r, &name)?;
        Ok((node, action.dir.join(hook.script), name, evaluated))
    }

    /// Evaluates the step's `env` and `with:` in the caller's contexts,
    /// and the action's input defaults in the action's.
    pub(super) fn evaluate(
        &self,
        step: &UsesStep,
        action: &Action,
        r: &ActionRef,
        name: &str,
    ) -> Result<Evaluated, String> {
        let workspace = &self.layout.workspace;
        let mut env = BTreeMap::new();
        for (key, value) in &step.env {
            let text = interp(value, &self.ctx, workspace, self.status)
                .map_err(|e| format!("env {key}: {e}"))?;
            env.insert(key.clone(), text);
        }
        // The step's own expressions see its env too.
        let mut caller = self.ctx.clone();
        let mut visible = self.env_context();
        visible.extend(env.clone());
        caller.set_env(&visible);

        let mut given = BTreeMap::new();
        for (key, value) in &step.with {
            let text = match value {
                Value::String(text) => interp(text, &caller, workspace, self.status)
                    .map_err(|e| format!("with {key}: {e}"))?,
                Value::Null => String::new(),
                other => other.to_string(),
            };
            given.insert(key.clone(), text);
        }

        let (repository, ref_) = match r {
            ActionRef::Remote {
                owner,
                repo,
                git_ref,
                ..
            } => (format!("{owner}/{repo}"), git_ref.clone()),
            ActionRef::Local(_) => (String::new(), String::new()),
        };
        let path = action.dir.to_string_lossy().into_owned();
        let mut action_scope = ActionScope {
            action: name.to_owned(),
            path: path.clone(),
            ref_: ref_.clone(),
            repository: repository.clone(),
            inputs: given.clone(),
        };
        let scope = caller.with_action(action_scope.clone());

        let mut warnings = Vec::new();
        let declared = &action.meta.inputs;
        let is_given = |input: &str| given.keys().any(|k| k.eq_ignore_ascii_case(input));
        let mut inputs = given.clone();
        for (input, spec) in declared {
            if is_given(input) {
                continue;
            }
            match &spec.default {
                Some(default) => {
                    let text = interp(default, &scope, workspace, self.status)
                        .map_err(|e| format!("input {input} default: {e}"))?;
                    inputs.insert(input.clone(), text);
                }
                None if spec.required => {
                    warnings.push(format!("Input required and not supplied: {input}"));
                }
                None => {}
            }
        }
        let unknown: Vec<String> = given
            .keys()
            .filter(|key| !declared.keys().any(|d| d.eq_ignore_ascii_case(key)))
            .map(|key| format!("'{key}'"))
            .collect();
        if !unknown.is_empty() {
            let valid: Vec<String> = declared.keys().map(|d| format!("'{d}'")).collect();
            warnings.push(format!(
                "Unexpected input(s) {}, valid inputs are [{}]",
                unknown.join(", "),
                valid.join(", ")
            ));
        }

        action_scope.inputs = inputs.clone();
        let step_env = env.clone();
        for (input, value) in inputs {
            let key = format!("INPUT_{}", input.replace(' ', "_").to_uppercase());
            env.insert(key, value);
        }
        env.insert("GITHUB_ACTION_PATH".to_owned(), path);
        if matches!(r, ActionRef::Remote { .. }) {
            env.insert("GITHUB_ACTION_REPOSITORY".to_owned(), repository);
            env.insert("GITHUB_ACTION_REF".to_owned(), ref_);
        }
        Ok(Evaluated {
            env,
            step_env,
            scope: action_scope,
            warnings,
        })
    }

    /// Runs one script of a JavaScript action on Node, as the runner
    /// user, through the same process path as `run:` steps.
    async fn run_hook(
        &mut self,
        hook: &Hook<'_>,
        node: &Path,
        script: &Path,
        action_name: &str,
        extra: &BTreeMap<String, String>,
        sink: &mut dyn Sink,
    ) -> Ran {
        let prepared = self.hook_env(hook, action_name, extra);
        let (files, env) = match prepared {
            Ok(prepared) => prepared,
            Err(message) => {
                self.say(sink, Line::Error(&message));
                return Ran {
                    outcome: Conclusion::Failure,
                    outputs: BTreeMap::new(),
                    state: Vec::new(),
                };
            }
        };
        let args = [script.to_string_lossy().into_owned()];
        let program = node.to_string_lossy();
        let cwd = self.layout.workspace.clone();
        let ran = self
            .execute(
                Launch {
                    program: &program,
                    args: &args,
                    cwd: &cwd,
                    env: &env,
                    timeout: None,
                    timeout_minutes: None,
                    files: &files,
                },
                sink,
            )
            .await;
        self.states
            .entry(hook.index)
            .or_default()
            .extend(ran.state.iter().cloned());
        ran
    }

    /// The file command files and the environment of a hook: the
    /// runner's variables, the job's `env` and `GITHUB_ENV`, then the
    /// step's own, then `STATE_*`.
    fn hook_env(
        &self,
        hook: &Hook<'_>,
        action_name: &str,
        extra: &BTreeMap<String, String>,
    ) -> Result<(StepFiles, BTreeMap<String, String>), String> {
        let files = StepFiles::create(&self.layout.temp, hook.slot, self.owner)
            .map_err(|e| format!("{e:#}"))?;
        let mut env = base_env(self, &files, action_name)?;
        env.extend(self.env_context());
        env.extend(extra.clone());
        if let Some(state) = self.states.get(&hook.index) {
            for (key, value) in state {
                env.insert(format!("STATE_{key}"), value.clone());
            }
        }
        self.with_path(&mut env);
        Ok((files, env))
    }
}

/// One script of an action, and where its bookkeeping lives.
struct Hook<'a> {
    /// The step's index.
    index: usize,
    /// Which file command files it uses. A step's `pre` and `post` get
    /// slots after the real steps, so no two scripts share files.
    slot: usize,
    /// `runs.using`, for choosing the Node binary.
    version: &'a str,
    /// The script, relative to the action directory.
    script: &'a str,
    /// Its condition, raw.
    condition: Option<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for runtime in ["node20", "node24"] {
            let bin = dir.path().join(runtime).join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join("node"), "").unwrap();
        }
        dir
    }

    #[test]
    fn old_versions_run_on_node20() {
        let dir = tree();
        for version in ["node12", "node16", "node20"] {
            let node = locate(None, dir.path(), None, version).unwrap();
            assert_eq!(node, dir.path().join("node20/bin/node"), "{version}");
        }
        for version in ["node24", "node30"] {
            let node = locate(None, dir.path(), None, version).unwrap();
            assert_eq!(node, dir.path().join("node24/bin/node"), "{version}");
        }
    }

    #[test]
    fn a_missing_runtime_falls_back_to_the_other() {
        let dir = tree();
        std::fs::remove_dir_all(dir.path().join("node20")).unwrap();
        let node = locate(None, dir.path(), None, "node16").unwrap();
        assert_eq!(node, dir.path().join("node24/bin/node"));
    }

    #[test]
    fn the_flag_wins_and_path_is_the_last_resort() {
        let dir = tree();
        let flag = Path::new("/x/node");
        assert_eq!(
            locate(Some(flag), dir.path(), None, "node20").unwrap(),
            flag
        );

        let empty = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        std::fs::write(bin.path().join("node"), "").unwrap();
        let path = std::env::join_paths([bin.path()]).unwrap();
        let node = locate(None, empty.path(), Some(path), "node20").unwrap();
        assert_eq!(node, bin.path().join("node"));
        assert!(locate(None, empty.path(), None, "node20").is_err());
    }
}
