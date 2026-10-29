// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reusable workflow calls: inlining called files and binding their inputs, secrets and outputs.

use super::jobs::fit_default;
use super::rewrite::{
    expr_text, map_job, rewrite, rewrite_expr, rewrite_value, simple, walk_job, whole,
    workflow_call,
};
use super::{
    BTreeMap, BTreeSet, Call, Called, Converted, Cx, Expansion, GhJob, Job, MAX_CALL_DEPTH,
    Parameter, Permissions, Require, Scope, Value, WORKFLOW_DIRS, expr, plan,
};

impl Cx {
    /// Points what reads a job that called a workflow at the jobs the
    /// call became: `needs: caller` becomes a `requires` of each job the
    /// call ended with, and `needs.caller.outputs.X` becomes what the
    /// called workflow's `on.workflow_call.outputs.X` says. Jobs of a
    /// called file get their ids with the call in front, so that
    /// `needs.x` still names them.
    pub(super) fn link_calls(&mut self, scope: &Scope, level: &mut Converted) {
        let mut warned = Vec::new();
        if !scope.prefix.is_empty() || !level.calls.is_empty() {
            let mut map = needs_mapper(scope, &level.local, &level.calls, "needs", &mut warned);
            for key in &level.plain {
                if let Some(job) = self.config.jobs.get_mut(key) {
                    map_job(job, &mut map);
                }
            }
        }
        warned.sort();
        warned.dedup();
        for message in warned {
            self.warn(format!("{}{}: {message}", scope.label, scope.from));
        }
        let ends: BTreeMap<String, Vec<String>> = level
            .calls
            .iter()
            .map(|(id, call)| (scope.full(id), call.requires.clone()))
            .collect();
        if ends.is_empty() {
            return;
        }
        for (_, entries) in &mut level.entries {
            for entry in entries {
                let mut seen = BTreeSet::new();
                entry.requires = entry
                    .requires
                    .iter()
                    .flat_map(|r| match ends.get(&r.job) {
                        Some(jobs) => jobs
                            .iter()
                            .map(|job| Require {
                                job: job.clone(),
                                statuses: r.statuses,
                            })
                            .collect(),
                        None => vec![r.clone()],
                    })
                    .filter(|r| seen.insert(r.job.clone()))
                    .collect();
            }
        }
    }

    /// The file `uses: ./path` names, among the files with
    /// `on.workflow_call`. The path may be under `.github`, `.forgejo` or
    /// `.gitea`; a file is found under any of them.
    pub(super) fn callee(&self, uses: &str) -> Option<String> {
        let want = uses.strip_prefix("./")?.trim_start_matches("./");
        let name = WORKFLOW_DIRS
            .iter()
            .find_map(|dir| want.strip_prefix(dir))
            .unwrap_or(want);
        let at = |file: &str, path: &str| file == path || file.ends_with(&format!("/{path}"));
        self.library
            .keys()
            .find(|file| {
                let file = file.trim_start_matches("./");
                at(file, want)
                    || file == name
                    || WORKFLOW_DIRS
                        .iter()
                        .any(|dir| at(file, &format!("{dir}{name}")))
            })
            .cloned()
    }

    /// The `uses` of a call and the file it names. `Err` says what became
    /// of a call that cannot be inlined: a remote one, one of a file that
    /// was not given, a cycle or one nested too deep.
    pub(super) fn resolve(
        &mut self,
        scope: &Scope,
        at: &str,
        gh: &GhJob,
    ) -> Result<(String, String), Call> {
        let Some(uses) = gh.uses.as_ref().and_then(Value::as_str) else {
            self.warn(format!(
                "{at} calls a reusable workflow, which is not supported; the job was dropped"
            ));
            return Err(Call::Dropped);
        };
        if !uses.starts_with("./") {
            let path = uses.split('@').next().unwrap_or(uses);
            let name = path.rsplit('/').next().unwrap_or(path);
            self.warn(format!(
                "{at} calls `{uses}`, a reusable workflow in another repository, which is not supported; copy `{name}` into this repository and call it as `./.github/workflows/{name}`; the job was dropped"
            ));
            return Err(Call::Dropped);
        }
        let Some(file) = self.callee(uses) else {
            self.warn(format!(
                "{at} calls `{uses}`, which is not among the converted files or has no `on: workflow_call`; the job was dropped"
            ));
            return Err(Call::Dropped);
        };
        if scope.stack.contains(&file) {
            let chain: Vec<String> = scope
                .stack
                .iter()
                .chain([&file])
                .map(|f| format!("`{f}`"))
                .collect();
            self.error(format!(
                "{at} calls `{uses}`, which makes a cycle ({}); the job was dropped",
                chain.join(" calls ")
            ));
            return Err(Call::Failed);
        }
        if scope.depth >= MAX_CALL_DEPTH {
            self.error(format!(
                "{at} calls `{uses}` more than {MAX_CALL_DEPTH} levels deep; the job was dropped"
            ));
            return Err(Call::Failed);
        }
        Ok((uses.to_owned(), file))
    }

    /// Converts a job that calls a reusable workflow. A call of a file
    /// among the converted ones is inlined: each job of the called file
    /// becomes the job `<stem>-<caller>-<job>`, with the `inputs` of
    /// `on.workflow_call` as `parameters` and the call's `with:` as the
    /// entries' `with`. A remote call, or one of a file that was not
    /// given, is dropped with a warning; a cycle, a call nested more
    /// than [`MAX_CALL_DEPTH`] levels deep or a call that does not
    /// convert is an error.
    pub(super) fn call(
        &mut self,
        scope: &Scope,
        at: &str,
        id: &str,
        ids: &BTreeSet<String>,
        gh: &GhJob,
    ) -> Call {
        let (uses, file) = match self.resolve(scope, at, gh) {
            Ok(found) => found,
            Err(call) => return call,
        };
        let wf = self.library[&file].clone();
        let before = self.errors.len();
        for (key, set) in [
            ("strategy", gh.strategy.is_some()),
            ("concurrency", gh.concurrency.is_some()),
        ] {
            if set {
                self.warn(format!(
                    "{at}: `{key}` on a job that calls a reusable workflow is not supported and was dropped"
                ));
            }
        }
        let caller_permissions = self.permissions(at, gh.permissions.as_ref(), true);
        let needs = self.needs(at, gh.needs.clone(), ids);

        // The inputs of the called file and the values the call gives.
        let body = workflow_call(&wf.on).unwrap_or(Value::Null);
        let params = self.dispatch(&file, &body).inputs;
        self.reserved_inputs(at, &file, &params);
        let with = self.call_with(scope, at, &file, gh, &params);
        let mut inputs: BTreeMap<String, Value> = params
            .iter()
            .filter_map(|(name, p)| Some((name.clone(), p.default.clone()?)))
            .collect();
        inputs.extend(with.clone());
        if self.errors.len() > before {
            return Call::Failed;
        }

        let label = format!("`{file}`");
        let env = self.env(&label, "env", wf.env.as_ref());
        let defaults = self.defaults(&label, wf.defaults.as_ref());
        let wf_permissions = self.permissions(&label, wf.permissions.as_ref(), false);
        let inner = Scope {
            label,
            from: format!(" (called by {at})"),
            stem: scope.stem.clone(),
            prefix: if scope.prefix.is_empty() {
                id.to_owned()
            } else {
                format!("{}-{id}", scope.prefix)
            },
            depth: scope.depth + 1,
            stack: scope.stack.iter().cloned().chain([file.clone()]).collect(),
            inputs: Some(inputs),
        };
        let mut level = self.jobs(&inner, wf.jobs.0, &env, &defaults);
        if level.failed || level.entries.is_empty() {
            for (_, entries) in &level.entries {
                for entry in entries {
                    self.config.jobs.remove(&entry.job);
                }
            }
            if level.failed {
                return Call::Failed;
            }
            self.warn(format!(
                "{at} calls `{uses}`, which has no job left to run; the job was dropped"
            ));
            return Call::Dropped;
        }

        // What the called jobs read as `inputs` and `secrets`.
        let all: BTreeSet<String> = level
            .entries
            .iter()
            .flat_map(|(_, entries)| entries.iter().map(|e| e.job.clone()))
            .collect();
        self.bind_inputs(&mut level, &params, &with, wf_permissions.as_ref());
        self.pass_secrets(at, &file, gh.secrets.as_ref(), &body, &all);
        if let Some(permissions) = &caller_permissions {
            for key in &all {
                if let Some(job) = self.config.jobs.get_mut(key) {
                    job.permissions.get_or_insert_with(|| permissions.clone());
                }
            }
        }

        // What jobs that need the call read, and wait for.
        let required: BTreeSet<&str> = level
            .entries
            .iter()
            .flat_map(|(_, e)| e[0].requires.iter().map(|r| r.job.as_str()))
            .collect();
        let mut ends: Vec<String> = level
            .entries
            .iter()
            .map(|(id, _)| id)
            .filter(|id| !required.contains(id.as_str()))
            .cloned()
            .collect();
        let outputs = self.call_outputs(at, &inner, &level, &body, &mut ends);

        let given = inner.inputs.clone().unwrap_or_default();
        self.attach(scope, at, gh, &needs, &given, &mut level);
        Call::Done(Called {
            entries: level.entries,
            expansion: Expansion {
                requires: ends,
                outputs,
            },
        })
    }

    /// An input a workflow entry reads as its own key, such as `name`, is
    /// an error: the called jobs would take it as a parameter that no
    /// entry could set.
    pub(super) fn reserved_inputs(
        &mut self,
        at: &str,
        file: &str,
        params: &BTreeMap<String, Parameter>,
    ) {
        for name in params
            .keys()
            .filter(|n| plan::ENTRY_KEYS.contains(&n.as_str()))
        {
            self.error(format!(
                "{at} calls `{file}`, whose input `{name}` would be a job parameter that a workflow entry reads as its own key; rename the input; the job was dropped"
            ));
        }
    }

    /// The values a call gives the inputs of the file it calls, as the
    /// entries' `with`: those of inputs the file does not have are dropped
    /// with a warning, and an input with no default that the call does
    /// not give is an error.
    pub(super) fn call_with(
        &mut self,
        scope: &Scope,
        at: &str,
        file: &str,
        gh: &GhJob,
        params: &BTreeMap<String, Parameter>,
    ) -> BTreeMap<String, Value> {
        let mut with = BTreeMap::new();
        let given = match &gh.with_ {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Object(map)) => map.iter().collect(),
            Some(other) => {
                self.warn(format!(
                    "{at}: `with: {}` is not a map and was dropped",
                    show(other)
                ));
                Vec::new()
            }
        };
        for (name, value) in given {
            let Some(param) = params.get(name) else {
                self.warn(format!(
                    "{at}: `with.{name}` is not an input of `{file}` and was dropped"
                ));
                continue;
            };
            let value = match &scope.inputs {
                Some(inputs) => substitute(value, inputs),
                None => value.clone(),
            };
            let value = fit_default(&param.kind, &value).unwrap_or(value);
            with.insert(name.clone(), value);
        }
        for (name, param) in params {
            if param.default.is_none() && !with.contains_key(name) {
                self.error(format!(
                    "{at}: input `{name}` of `{file}` is required and the call does not give it"
                ));
            }
        }
        with
    }

    /// Gives the jobs a called file has itself the file's inputs as
    /// parameters and the call's values as the `with` of their entries;
    /// what they read as `inputs` becomes `parameters`.
    pub(super) fn bind_inputs(
        &mut self,
        level: &mut Converted,
        params: &BTreeMap<String, Parameter>,
        with: &BTreeMap<String, Value>,
        permissions: Option<&Permissions>,
    ) {
        for key in &level.plain {
            if let Some(job) = self.config.jobs.get_mut(key) {
                for (name, param) in params {
                    job.parameters
                        .entry(name.clone())
                        .or_insert_with(|| param.clone());
                }
                job.permissions = job.permissions.take().or_else(|| permissions.cloned());
                map_job(job, &mut |seg| {
                    (seg.len() >= 2 && seg[0] == "inputs")
                        .then(|| (2, format!("parameters.{}", seg[1])))
                });
            }
        }
        for (_, entries) in &mut level.entries {
            for entry in entries {
                if level.plain.contains(&entry.job) {
                    entry.with.extend(with.clone());
                }
            }
        }
    }

    /// Puts the call's `needs` and `if` on the jobs of the called file
    /// that need nothing inside it.
    pub(super) fn attach(
        &mut self,
        scope: &Scope,
        at: &str,
        gh: &GhJob,
        needs: &[String],
        given: &BTreeMap<String, Value>,
        level: &mut Converted,
    ) {
        let roots: BTreeSet<String> = level
            .entries
            .iter()
            .filter(|(_, e)| e[0].requires.is_empty())
            .flat_map(|(_, e)| e.iter().map(|e| e.job.clone()))
            .collect();
        for (_, entries) in &mut level.entries {
            if entries[0].requires.is_empty() {
                for entry in entries {
                    entry.requires = needs
                        .iter()
                        .map(|need| Require::new(scope.full(need)))
                        .collect();
                }
            }
        }
        let condition = gh
            .if_
            .as_ref()
            .and_then(scalar)
            .filter(|c| !c.trim().is_empty());
        let Some(condition) = condition else {
            return;
        };
        let condition = match &scope.inputs {
            Some(inputs) => rewrite(
                &condition,
                !condition.contains("${{"),
                &mut input_mapper(inputs),
            ),
            None => condition,
        };
        let mut kept = false;
        for key in &roots {
            let Some(job) = self.config.jobs.get_mut(key) else {
                continue;
            };
            // `parameters` can be read only by a whole `${{ }}`, and the
            // combined condition is not one: use the value the call gave.
            if let Some(current) = &job.if_
                && current.contains("parameters.")
            {
                let bare = !current.contains("${{");
                job.if_ = Some(rewrite(current, bare, &mut |parts| match parts {
                    ["parameters", name, ..] => Some((2, expr_text(literal_input(given, name)?))),
                    _ => None,
                }));
            }
            if job
                .if_
                .as_deref()
                .is_some_and(|c| c.contains("parameters."))
            {
                kept = true;
            } else if let Some(and) = both(&condition, job.if_.as_deref()) {
                job.if_ = Some(and);
            } else {
                kept = true;
            }
        }
        if kept {
            self.warn(format!(
                "{at}: `if` could not be combined with the `if` of the called jobs and was dropped for them"
            ));
        }
    }

    /// Passes the call's `secrets` to the called jobs: `inherit` passes
    /// every secret on, and a map renames the reads of each name to what
    /// the call gives it. Any other name reads as `''`, since GitHub
    /// passes nothing the call leaves out. A secret the called file declares and the call
    /// does not give is a warning.
    pub(super) fn pass_secrets(
        &mut self,
        at: &str,
        file: &str,
        secrets: Option<&Value>,
        body: &Value,
        jobs: &BTreeSet<String>,
    ) {
        let mut renames: BTreeMap<String, String> = BTreeMap::new();
        let inherit = matches!(secrets, Some(Value::String(s)) if s == "inherit");
        match secrets {
            Some(Value::Object(map)) => {
                for (name, value) in map {
                    renames.insert(name.to_ascii_uppercase(), expr_text(value));
                }
            }
            Some(_) if !inherit => self.warn(format!(
                "{at}: `secrets` is not `inherit` or a map and was dropped"
            )),
            _ => {}
        }
        if !inherit && let Some(Value::Object(declared)) = body.get("secrets") {
            for name in declared.keys() {
                if !renames.contains_key(&name.to_ascii_uppercase()) {
                    self.warn(format!(
                        "{at}: `{file}` declares secret `{name}`, which the call does not pass"
                    ));
                }
            }
        }
        if inherit {
            return;
        }
        // Without `inherit` the called jobs read only what the call
        // passes: any other `secrets.X` is empty, as on GitHub.
        // `GITHUB_TOKEN` is always available.
        let mut reads_all = false;
        for key in jobs {
            if let Some(job) = self.config.jobs.get_mut(key) {
                walk_job(job, &mut |text, bare| {
                    if expr::secret_refs(text, bare).is_ok_and(|refs| refs.all) {
                        reads_all = true;
                    }
                    text.to_owned()
                });
            }
        }
        if reads_all {
            self.warn(format!(
                "{at}: `{file}` reads the whole `secrets` context or a computed secret name, which the call does not pass with `inherit`; the read is left as is"
            ));
        }
        let mut map = |seg: &[&str]| {
            if seg[0] != "secrets" {
                return None;
            }
            let name = seg.get(1)?.to_ascii_uppercase();
            if name == "GITHUB_TOKEN" {
                return None;
            }
            let found = renames.get(&name).cloned();
            Some((2, found.unwrap_or_else(|| "''".to_owned())))
        };
        for key in jobs {
            if let Some(job) = self.config.jobs.get_mut(key) {
                map_job(job, &mut map);
            }
        }
    }

    /// Maps `on.workflow_call.outputs` of a called file to the
    /// expressions `needs.<call>.outputs.<name>` stands for in the jobs
    /// that need the call. An output whose value is not an expression
    /// over `jobs.<job>.outputs.<name>` of the file's jobs maps to
    /// `None` and is dropped with a warning. The jobs the outputs read
    /// are added to `ends`, so that those that need the call can read
    /// them as `needs`.
    pub(super) fn call_outputs(
        &mut self,
        at: &str,
        inner: &Scope,
        level: &Converted,
        body: &Value,
        ends: &mut Vec<String>,
    ) -> BTreeMap<String, Option<String>> {
        let mut outputs = BTreeMap::new();
        let Some(Value::Object(declared)) = body.get("outputs") else {
            return outputs;
        };
        let mut warned = Vec::new();
        let mut map = needs_mapper(inner, &level.local, &level.calls, "jobs", &mut warned);
        let mut dropped = Vec::new();
        for (name, output) in declared {
            let text = output.get("value").and_then(Value::as_str);
            let mapped = text.and_then(|text| match whole(text) {
                Some(expr) => Some(rewrite(expr, true, &mut map)),
                None if !text.contains("${{") => Some(expr_text(&Value::String(text.to_owned()))),
                None => None,
            });
            let mapped = mapped.and_then(|expr| {
                let mut ok = true;
                let mut read = Vec::new();
                rewrite(&expr, true, &mut |seg| {
                    match seg {
                        ["jobs", ..] => ok = false,
                        ["needs", job, ..] => read.push((*job).to_owned()),
                        _ => {}
                    }
                    None
                });
                for job in read {
                    if level.entries.iter().any(|(id, _)| *id == job) && !ends.contains(&job) {
                        ends.push(job);
                    }
                }
                ok.then(|| {
                    if simple(&expr) {
                        expr
                    } else {
                        format!("({expr})")
                    }
                })
            });
            if mapped.is_none() {
                dropped.push(name.clone());
            }
            outputs.insert(name.clone(), mapped);
        }
        drop(map);
        for name in dropped {
            self.warn(format!(
                "{at}: output `{name}` of the called workflow is not an expression over `jobs.<job>.outputs.<name>` of its own jobs; reads of it were dropped"
            ));
        }
        warned.sort();
        warned.dedup();
        for message in warned {
            self.warn(format!("{at}: {message}"));
        }
        outputs
    }
}

/// The value a call gives an input, when it is a literal: not a list, a
/// map or text with an expression in it.
pub(super) fn literal_input<'v>(
    inputs: &'v BTreeMap<String, Value>,
    name: &str,
) -> Option<&'v Value> {
    inputs.get(name).filter(|v| match v {
        Value::String(s) => !s.contains("${{"),
        Value::Array(_) | Value::Object(_) => false,
        _ => true,
    })
}

/// Makes the reads of `inputs.NAME` that are not a whole `${{ }}` into
/// the value the call gave, which `parameters` cannot do: they are
/// evaluated at run time, where parameters are not. A read of an input
/// whose value is not a literal stays, and the result is a message for
/// each such input.
pub(super) fn bind_literals(job: &mut Job, inputs: &BTreeMap<String, Value>) -> Vec<String> {
    let literal = |name: &str| literal_input(inputs, name);
    let mut stuck = BTreeSet::new();
    let mut expr = |inner: &str| {
        rewrite_expr(inner, &mut |parts| match parts {
            ["inputs", name, ..] => {
                let value = literal(name);
                if value.is_none() {
                    stuck.insert((*name).to_owned());
                }
                value.map(|v| (2, expr_text(v)))
            }
            _ => None,
        })
    };
    walk_job(job, &mut |text, bare| {
        if bare {
            return expr(text);
        }
        let Ok(found) = expr::expressions(text) else {
            return text.to_owned();
        };
        let mut out = text.to_owned();
        for (start, inner) in found.into_iter().rev() {
            // `parameters` can be read as `${{ parameters.NAME }}` and
            // nothing else.
            let plain = inner
                .trim()
                .strip_prefix("inputs.")
                .is_some_and(|name| simple(name) && !name.contains('.'));
            if !plain {
                out.replace_range(start..start + inner.len(), &expr(inner));
            }
        }
        out
    });
    stuck
        .into_iter()
        .map(|name| {
            format!(
                "reads `inputs.{name}` inside an expression, and the call does not give it a literal; only a whole `${{{{ inputs.{name} }}}}` can be passed on"
            )
        })
        .collect()
}

/// Answers reads of `inputs.NAME` with the value the call gave it.
pub(super) fn input_mapper(
    inputs: &BTreeMap<String, Value>,
) -> impl FnMut(&[&str]) -> Option<(usize, String)> {
    move |parts| match parts {
        ["inputs", name, ..] => Some((2, expr_text(inputs.get(*name)?))),
        _ => None,
    }
}

/// A value with the reads of `inputs.NAME` in it replaced by the values
/// the call gave. A text that is only such a read becomes the value.
pub(super) fn substitute(value: &Value, inputs: &BTreeMap<String, Value>) -> Value {
    if let Value::String(text) = value
        && let Some(name) = whole(text).and_then(|e| e.strip_prefix("inputs."))
        && let Some(found) = inputs.get(name)
    {
        return found.clone();
    }
    let mut value = value.clone();
    rewrite_value(&mut value, &mut input_mapper(inputs));
    value
}

/// Answers reads of `context.JOB.outputs.NAME`, `context` being `needs`
/// in the jobs of a file and `jobs` in the outputs of a called file: a
/// job of a called file by its id in the workflow, and a job that made a
/// call by what the call says. A read of a call's output that has no
/// expression is replaced by an empty string, and a warning is added.
pub(super) fn needs_mapper<'a>(
    scope: &'a Scope,
    plain: &'a BTreeSet<String>,
    calls: &'a BTreeMap<String, Expansion>,
    context: &'a str,
    warned: &'a mut Vec<String>,
) -> impl FnMut(&[&str]) -> Option<(usize, String)> + 'a {
    move |parts| {
        let [first, job, rest @ ..] = parts else {
            return None;
        };
        if *first != context {
            return None;
        }
        if plain.contains(*job) {
            let renamed = context == "jobs" || !scope.prefix.is_empty();
            return renamed.then(|| (2, format!("needs.{}", scope.full(job))));
        }
        let call = calls.get(*job)?;
        if let ["outputs", name, ..] = rest
            && let Some(Some(expr)) = call.outputs.get(*name)
        {
            return Some((4, expr.clone()));
        }
        let used = parts.len().min(4);
        warned.push(format!(
            "`{}` was dropped: the called workflow gives no such output",
            parts[..used].join(".")
        ));
        Some((used, "''".to_owned()))
    }
}

/// `a && b` for two conditions, each bare or one `${{ }}`. `None` when
/// one of them is text with expressions in it.
pub(super) fn both(call: &str, job: Option<&str>) -> Option<String> {
    let bare = |c: &str| match whole(c) {
        Some(expr) => Some(expr.to_owned()),
        None => (!c.contains("${{")).then(|| c.trim().to_owned()),
    };
    let call = bare(call)?;
    match job {
        None => Some(call),
        Some(job) => Some(format!("({call}) && ({})", bare(job)?)),
    }
}

/// A string, number or boolean as text. `None` for lists and maps; null
/// is the empty string.
pub(super) fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => Some(String::new()),
        Value::Array(_) | Value::Object(_) => None,
    }
}

/// A value as it appears in a message.
pub(super) fn show(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
