//! Permissions, triggers and `workflow_dispatch` inputs.

use super::calls::{scalar, show};
use super::jobs::fit_default;
use super::{
    BTreeMap, Cx, Deserialize, Dispatch, EVENTS, GITHUB_ONLY, GhInput, GhRelease, GhWorkflowRun,
    Level, Parameter, ParameterKind, Permissions, PullRequestTrigger, PushTrigger, RELEASE_TYPES,
    ReleaseTrigger, SCOPES, Schedule, Triggers, Value, WORKFLOW_RUN_TYPES, WorkflowRunTrigger,
};

impl Cx {
    /// Converts a `permissions:` value. `id-token`, scopes hatchrun does
    /// not know and levels it does not know are dropped with a warning.
    /// A value of another shape is dropped too; a job that said it
    /// then gets no permissions at all, never the workflow's, which
    /// could be wider than the author meant.
    pub(super) fn permissions(
        &mut self,
        at: &str,
        value: Option<&Value>,
        job: bool,
    ) -> Option<Permissions> {
        match value? {
            Value::String(text) if text == "read-all" => Some(Permissions::ReadAll),
            Value::String(text) if text == "write-all" => Some(Permissions::WriteAll),
            Value::Object(map) => {
                let mut scopes = BTreeMap::new();
                for (key, level) in map {
                    if key == "id-token" {
                        self.warn(format!("{at}: `id-token` is not supported and was dropped"));
                        continue;
                    }
                    if !SCOPES.iter().any(|(k, _)| k == key) {
                        self.warn(format!(
                            "{at}: `permissions.{key}` is not supported and was dropped"
                        ));
                        continue;
                    }
                    let level = match level.as_str() {
                        Some("read") => Level::Read,
                        Some("write") => Level::Write,
                        Some("none") => Level::None,
                        _ => {
                            self.warn(format!(
                                "{at}: `permissions.{key}` is `{}`, not read, write or none, and was dropped",
                                show(level)
                            ));
                            continue;
                        }
                    };
                    scopes.insert(key.clone(), level);
                }
                Some(Permissions::Scopes(scopes))
            }
            other => {
                self.warn(format!(
                    "{at}: `permissions: {}` is not read-all, write-all or a map and was dropped",
                    show(other)
                ));
                job.then(|| Permissions::Scopes(BTreeMap::new()))
            }
        }
    }

    /// Converts `on:` and warns about the events dropped. Returns `None`
    /// for a workflow left with no event, which is dropped with a warning.
    pub(super) fn kept_triggers(&mut self, file: &str, stem: &str, on: &Value) -> Option<Triggers> {
        let (triggers, unsupported) = self.triggers(file, stem, on);
        if triggers == Triggers::default() {
            self.no_events(file, &unsupported);
            return None;
        }
        self.dropped_events(file, &unsupported);
        Some(triggers)
    }

    /// Converts `on:`, which GitHub accepts as an event name, a list of
    /// names or a map of events to their filters. Returns the events
    /// hatchrun does not run on too, so the caller can tell a workflow
    /// that keeps some events from one left with none.
    pub(super) fn triggers(
        &mut self,
        file: &str,
        stem: &str,
        on: &Value,
    ) -> (Triggers, Vec<String>) {
        let mut triggers = Triggers::default();
        let mut unsupported = Vec::new();
        let mut event = |cx: &mut Self, name: &str, body: &Value| {
            if !cx.event(file, stem, name, body, &mut triggers) {
                unsupported.push(name.to_owned());
            }
        };
        match on {
            Value::String(name) => event(self, name, &Value::Null),
            Value::Array(events) => {
                for name in events {
                    match name {
                        Value::String(name) => event(self, name, &Value::Null),
                        other => self.error(format!(
                            "`{file}`: `on` lists `{other}`, which is not an event name"
                        )),
                    }
                }
            }
            Value::Object(events) => {
                for (name, body) in events {
                    event(self, name, body);
                }
            }
            other => self.error(format!(
                "`{file}`: `on` is `{other}`; it must be an event name, a list or a map"
            )),
        }
        (triggers, unsupported)
    }

    /// Converts one event into `triggers`. Returns false when hatchrun
    /// does not run on it.
    pub(super) fn event(
        &mut self,
        file: &str,
        stem: &str,
        event: &str,
        body: &Value,
        triggers: &mut Triggers,
    ) -> bool {
        let bad = |err: serde_json::Error| format!("`{file}`: `on.{event}`: {err}");
        match event {
            "push" => match Option::<PushTrigger>::deserialize(body) {
                Ok(push) => triggers.push = Some(push.unwrap_or_default()),
                Err(err) => self.error(bad(err)),
            },
            "pull_request" => match Option::<PullRequestTrigger>::deserialize(body) {
                Ok(pr) => triggers.pull_request = Some(pr.unwrap_or_default()),
                Err(err) => self.error(bad(err)),
            },
            "schedule" => match Vec::<Schedule>::deserialize(body) {
                Ok(schedule) => triggers.schedule = schedule,
                Err(err) => self.error(bad(err)),
            },
            "workflow_dispatch" => {
                triggers.workflow_dispatch = Some(self.dispatch(file, body));
            }
            "workflow_run" => match GhWorkflowRun::deserialize(body) {
                Ok(run) => triggers.workflow_run = self.workflow_run(file, stem, run),
                Err(err) => self.error(bad(err)),
            },
            "release" => match Option::<GhRelease>::deserialize(body) {
                Ok(release) => {
                    let types = release.and_then(|r| r.types);
                    triggers.release = self.release(file, types.as_ref());
                }
                Err(err) => self.error(bad(err)),
            },
            _ => return false,
        }
        true
    }

    /// Warns about each event of a workflow that hatchrun does not run
    /// on, and for those only GitHub delivers, to keep them there.
    pub(super) fn dropped_events(&mut self, file: &str, unsupported: &[String]) {
        for event in unsupported {
            let keep = if GITHUB_ONLY.contains(&event.as_str()) {
                format!("; keep `on.{event}` on GitHub Actions")
            } else {
                String::new()
            };
            self.warn(format!(
                "`{file}`: `on.{event}` is not supported and was dropped; hatchrun runs on {EVENTS}{keep}"
            ));
        }
    }

    /// Reports a workflow left with no event to run on, which is dropped
    /// with its jobs: they could never start. `unsupported` names the
    /// events it had that hatchrun does not run on.
    pub(super) fn no_events(&mut self, file: &str, unsupported: &[String]) {
        if !unsupported.is_empty() && unsupported.iter().all(|e| e == "workflow_call") {
            if self.called.contains(file) {
                return;
            }
            self.warn(format!(
                "`{file}` was dropped: it is a reusable workflow (`on: workflow_call`), which is not supported"
            ));
            return;
        }
        if unsupported.is_empty() {
            // `on` named no event, or the events it named did not convert
            // and are errors already.
            self.warn(format!(
                "`{file}` was dropped: none of its events converted"
            ));
            return;
        }
        let shown: Vec<String> = unsupported.iter().map(|e| format!("`{e}`")).collect();
        self.warn(format!(
            "`{file}` was dropped: it runs on {}, and hatchrun runs on {EVENTS}; keep it on GitHub Actions",
            shown.join(", ")
        ));
    }

    /// Converts `on.workflow_run`, naming each workflow by the stem of
    /// the file that has its GitHub name. Names no file has, the file's
    /// own name and types other than `completed` are dropped with a
    /// warning. `None` when no workflow or no type is left.
    pub(super) fn workflow_run(
        &mut self,
        file: &str,
        stem: &str,
        run: GhWorkflowRun,
    ) -> Option<WorkflowRunTrigger> {
        let at = format!("`{file}`: `on.workflow_run`");
        let mut workflows: Vec<String> = Vec::new();
        for name in run.workflows {
            let Some(found) = self.names.get(&name).cloned() else {
                self.warn(format!(
                    "{at} names workflow `{name}`, which none of the converted files is; it was dropped"
                ));
                continue;
            };
            for other in found {
                if other == stem {
                    self.warn(format!(
                        "{at} names workflow `{name}`, which is this file; it was dropped"
                    ));
                } else if !workflows.contains(&other) {
                    workflows.push(other);
                }
            }
        }
        let types = match &run.types {
            None | Some(Value::Null) => vec!["completed".to_owned()],
            Some(types) => self.types(&at, types, WORKFLOW_RUN_TYPES)?,
        };
        if workflows.is_empty() {
            self.warn(format!(
                "{at} was dropped: it names no other converted workflow"
            ));
            return None;
        }
        Some(WorkflowRunTrigger {
            workflows,
            types,
            branches: run.branches,
            branches_ignore: run.branches_ignore,
        })
    }

    /// Converts `on.release`. Types hatchrun does not run on are dropped
    /// with a warning; with no `types`, GitHub runs on every type, and
    /// the release runs on `published` only. `None` when no type is left.
    pub(super) fn release(&mut self, file: &str, types: Option<&Value>) -> Option<ReleaseTrigger> {
        let at = format!("`{file}`: `on.release`");
        let Some(types) = types.filter(|t| !t.is_null()) else {
            self.warn(format!("{at} has no `types`; it runs on `published` only"));
            return Some(ReleaseTrigger::default());
        };
        Some(ReleaseTrigger {
            types: self.types(&at, types, RELEASE_TYPES)?,
            ..ReleaseTrigger::default()
        })
    }

    /// The activity types of an event out of `known`, dropping the rest
    /// with a warning. `None` when none of them is known, which drops
    /// the event with a warning.
    pub(super) fn types(&mut self, at: &str, types: &Value, known: &[&str]) -> Option<Vec<String>> {
        let given: Vec<String> = match types {
            Value::Array(items) => items.iter().filter_map(scalar).collect(),
            other => scalar(other).into_iter().collect(),
        };
        let (kept, dropped): (Vec<String>, Vec<String>) =
            given.into_iter().partition(|t| known.contains(&t.as_str()));
        let shown: Vec<String> = dropped.iter().map(|t| format!("`{t}`")).collect();
        let runs = known.join(", ");
        if kept.is_empty() {
            self.warn(format!(
                "{at} was dropped: its types {} are not supported; hatchrun runs on {runs}",
                shown.join(", ")
            ));
            return None;
        }
        if !dropped.is_empty() {
            self.warn(format!(
                "{at}: types {} are not supported and were dropped; hatchrun runs on {runs}",
                shown.join(", ")
            ));
        }
        Some(kept)
    }

    /// Drops from `on.workflow_run` the workflows that did not convert,
    /// then the event when none is left, then a workflow left with no
    /// event and its jobs, until every name it keeps is a workflow.
    /// `stems` maps each stem to its file.
    pub(super) fn unreachable_runs(&mut self, stems: &BTreeMap<String, &str>) {
        loop {
            let mut changed = false;
            let keys: Vec<String> = self.config.workflows.keys().cloned().collect();
            for key in keys {
                let file = stems.get(&key).copied().unwrap_or(key.as_str());
                let wf = &self.config.workflows[&key];
                let Some(run) = &wf.on.workflow_run else {
                    continue;
                };
                let gone: Vec<String> = run
                    .workflows
                    .iter()
                    .filter(|w| !self.config.workflows.contains_key(*w))
                    .cloned()
                    .collect();
                if gone.is_empty() {
                    continue;
                }
                changed = true;
                let at = format!("`{file}`: `on.workflow_run`");
                for other in &gone {
                    self.warn(format!(
                        "{at} names workflow `{other}`, which did not convert; it was dropped"
                    ));
                }
                let Some(wf) = self.config.workflows.get_mut(&key) else {
                    continue;
                };
                let Some(run) = wf.on.workflow_run.as_mut() else {
                    continue;
                };
                run.workflows.retain(|w| !gone.contains(w));
                if !run.workflows.is_empty() {
                    continue;
                }
                wf.on.workflow_run = None;
                if wf.on != Triggers::default() {
                    self.warn(format!(
                        "{at} was dropped: it names no other converted workflow"
                    ));
                    continue;
                }
                let Some(wf) = self.config.workflows.remove(&key) else {
                    continue;
                };
                for entry in wf.jobs {
                    self.config.jobs.remove(&entry.job);
                }
                self.warn(format!(
                    "`{file}` was dropped: it runs only after workflows that did not convert"
                ));
            }
            if !changed {
                break;
            }
        }
    }

    /// Converts `on.workflow_dispatch`, mapping GitHub input types onto
    /// parameter kinds.
    pub(super) fn dispatch(&mut self, file: &str, body: &Value) -> Dispatch {
        let mut dispatch = Dispatch::default();
        let Some(inputs) = body.get("inputs").and_then(Value::as_object) else {
            return dispatch;
        };
        for (name, input) in inputs {
            let at = format!("`{file}`: input `{name}`");
            let input = match GhInput::deserialize(input) {
                Ok(input) => input,
                Err(err) => {
                    self.error(format!("{at}: {err}"));
                    continue;
                }
            };
            let kind = match input.kind.as_deref().unwrap_or("string") {
                "string" => ParameterKind::String,
                "boolean" => ParameterKind::Boolean,
                "number" => {
                    self.warn(format!(
                        "{at} is a number input and became an integer parameter"
                    ));
                    ParameterKind::Integer
                }
                "environment" => {
                    self.warn(format!(
                        "{at} is an environment input and became a string parameter"
                    ));
                    ParameterKind::String
                }
                "choice" => {
                    let options: Option<Vec<String>> = input
                        .options
                        .as_ref()
                        .map(|o| o.iter().filter_map(scalar).collect());
                    match options {
                        Some(options) if !options.is_empty() => ParameterKind::Enum(options),
                        _ => {
                            self.error(format!("{at} is a choice input with no `options`"));
                            continue;
                        }
                    }
                }
                other => {
                    self.error(format!(
                        "{at} has type `{other}`, which GitHub does not define"
                    ));
                    continue;
                }
            };
            let default = input.default.and_then(|value| {
                let fitted = fit_default(&kind, &value);
                if fitted.is_none() {
                    self.warn(format!(
                        "{at}: `default: {}` does not fit the input type and was dropped",
                        show(&value)
                    ));
                }
                fitted
            });
            dispatch.inputs.insert(
                name.clone(),
                Parameter {
                    kind,
                    default,
                    description: input.description,
                },
            );
        }
        dispatch
    }
}
