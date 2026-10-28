//! Jobs, runners, matrices and steps.

use super::calls::{scalar, show};
use super::rewrite::LATER;
use super::{
    BTreeMap, BTreeSet, Cx, Deserialize, GhJob, GhMatrix, GhStep, JOB_KEYS, Job, Matrix,
    OLD_UBUNTU, ParameterKind, Require, RunStep, RunsOn, STEP_KEYS, Step, UsesStep, Value,
    WorkflowJob, plan, template, template_labels, valid_environment_name, vendor_template,
};

impl Cx {
    /// Reads one GitHub job, warning about keys GitHub does not define.
    pub(super) fn gh_job(&mut self, at: &str, value: &Value) -> Option<GhJob> {
        let Some(map) = value.as_object() else {
            self.error(format!("{at} is not a map"));
            return None;
        };
        for key in map.keys().filter(|k| !JOB_KEYS.contains(&k.as_str())) {
            self.warn(format!("{at}: unknown key `{key}` was dropped"));
        }
        match GhJob::deserialize(value) {
            Ok(job) => Some(job),
            Err(err) => {
                self.error(format!("{at}: {err}"));
                None
            }
        }
    }

    /// Reads a job's `environment`: a name, or a map whose `name` is the
    /// name. Records an error for anything hatchrun cannot read as written.
    pub(super) fn environment(&mut self, at: &str, value: Option<&Value>) -> Option<String> {
        let value = value?;
        let name = match value {
            Value::String(name) => name,
            Value::Object(map) => {
                for key in map.keys() {
                    match key.as_str() {
                        "name" => {}
                        "url" => self.warn(format!(
                            "{at}: `environment.url` is not supported and was dropped"
                        )),
                        other => self.warn(format!(
                            "{at}: unknown key `environment.{other}` was dropped"
                        )),
                    }
                }
                if let Some(Value::String(name)) = map.get("name") {
                    name
                } else {
                    self.error(format!(
                        "{at}: `environment` must be a name or a map with `name`"
                    ));
                    return None;
                }
            }
            _ => {
                self.error(format!(
                    "{at}: `environment` must be a name or a map with `name`"
                ));
                return None;
            }
        };
        if name.contains("${{") {
            self.error(format!(
                "{at}: `environment` uses an expression; hatchrun reads environment names as written"
            ));
            return None;
        }
        if let Err(rule) = valid_environment_name(name) {
            self.error(format!("{at}: `environment` `{name}`: {rule}"));
            return None;
        }
        Some(name.clone())
    }

    /// Converts one job into the job and its workflow entries: one, or
    /// more when the matrix `include` adds combinations of its own.
    /// `None` when the job has errors, which are recorded.
    pub(super) fn job(
        &mut self,
        at: &str,
        id: &str,
        key: &str,
        ids: &BTreeSet<String>,
        gh: GhJob,
    ) -> Option<(Job, Vec<WorkflowJob>)> {
        let before = self.errors.len();
        for (name, set, why) in [
            ("services", gh.services.is_some(), LATER),
            ("container", gh.container.is_some(), LATER),
            ("concurrency", gh.concurrency.is_some(), ""),
            ("continue-on-error", gh.continue_on_error.is_some(), ""),
        ] {
            if set {
                self.warn(format!(
                    "{at}: `{name}` is not supported and was dropped{why}"
                ));
            }
        }
        let environment = self.environment(at, gh.environment.as_ref());
        let mut matrix = self.strategy(at, gh.strategy);
        let runs_on = self.runs_on(at, gh.runs_on.as_ref(), &mut matrix);
        let leg_key = gh
            .runs_on
            .as_ref()
            .and_then(runs_on_text)
            .and_then(matrix_key);
        let matrices = self.include(at, matrix, leg_key);
        let requires = self.needs(at, gh.needs, ids);
        let timeout_minutes = self.minutes(at, "timeout-minutes", gh.timeout_minutes.as_ref());
        let outputs = self.env(at, "outputs", gh.outputs.as_ref());
        let env = self.env(at, "env", gh.env.as_ref());
        let defaults = self.defaults(at, gh.defaults.as_ref());
        let permissions = self.permissions(at, gh.permissions.as_ref(), true);
        let mut steps: Vec<Step> = gh
            .steps
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .filter_map(|(i, step)| self.step(at, i, &step))
            .collect();
        if self.errors.len() > before {
            return None;
        }
        defaults.apply(&mut steps);
        let (runs_on, parameters) = runs_on?.into_job();
        let job = Job {
            runs_on: Some(runs_on),
            parameters,
            env,
            if_: gh.if_.as_ref().and_then(scalar),
            timeout_minutes,
            outputs,
            permissions,
            environment,
            steps,
            ..Job::default()
        };
        Some((job, entries(id, key, ids, &requires, matrices)))
    }

    /// Reads `needs`: a job id or a list. An id that is not a job of
    /// the workflow is an error; repeats are dropped.
    pub(super) fn needs(
        &mut self,
        at: &str,
        needs: Option<Value>,
        ids: &BTreeSet<String>,
    ) -> Vec<String> {
        let requires = match needs {
            None => Vec::new(),
            Some(Value::String(need)) => vec![need],
            Some(Value::Array(needs)) => needs.iter().filter_map(scalar).collect(),
            Some(other) => {
                self.error(format!(
                    "{at} has `needs: {other}`; it must be a job id or a list"
                ));
                Vec::new()
            }
        };
        let mut unique: Vec<String> = Vec::new();
        for need in requires {
            if !ids.contains(&need) {
                self.error(format!(
                    "{at} needs `{need}`, which is not a job of the workflow"
                ));
            } else if !unique.contains(&need) {
                unique.push(need);
            }
        }
        unique
    }

    /// Maps `runs-on` to a runner template label. A third-party label,
    /// such as `blacksmith-4vcpu-ubuntu-2404`, becomes the template of
    /// its arch and size with a warning; `{labels: ...}` reads its labels
    /// and `{group: ...}` is an error. `runs-on: ${{ matrix.KEY }}`
    /// keeps the legs whose labels name a template and drops the others,
    /// such as macOS and Windows legs, with a warning.
    pub(super) fn runs_on(
        &mut self,
        at: &str,
        runs_on: Option<&Value>,
        matrix: &mut GhMatrix,
    ) -> Option<RunsOn> {
        let label = runs_on.and_then(runs_on_text);
        if let Some(text) = label
            && let Some(key) = matrix_key(text)
            && (matrix.values.contains_key(key)
                || matrix.include.iter().any(|i| i.contains_key(key)))
        {
            return self.matrix_runs_on(at, text, key, matrix);
        }
        if let Some(Value::Object(map)) = runs_on
            && let Some(group) = map.get("group")
        {
            self.error(format!(
                "{at} has `runs-on: {{group: {}}}`; runner groups are not supported; use one of {}",
                show(group),
                template_labels().join(", ")
            ));
            return None;
        }
        if let Some(old) = label
            && let Some(new) = runner_label(old)
        {
            if new != old {
                self.warn(became(at, old, new));
            }
            return Some(RunsOn::Label(new.to_owned()));
        }
        let shown = runs_on.map_or_else(|| "nothing".to_owned(), show);
        self.error(format!(
            "{at} has `runs-on: {shown}`, which names no runner template; use one of {}",
            template_labels().join(", ")
        ));
        None
    }

    /// Resolves `runs-on: ${{ matrix.KEY }}`. Legs whose value names no
    /// template are dropped, and so is a value that runs on the same
    /// template as an earlier one. When the legs left run on one
    /// template the job runs on it and the values stay as written;
    /// otherwise the job takes `KEY` as a parameter and runs on the
    /// template it names, and each value becomes its template's label.
    pub(super) fn matrix_runs_on(
        &mut self,
        at: &str,
        text: &str,
        key: &str,
        matrix: &mut GhMatrix,
    ) -> Option<RunsOn> {
        let mut found: Vec<&'static str> = Vec::new();
        let mut dropped = Vec::new();
        if let Some(values) = matrix.values.get_mut(key) {
            let mut kept: Vec<String> = Vec::new();
            let mut duplicates = Vec::new();
            values.retain(|value| {
                let Some(new) = value.as_str().and_then(runner_label) else {
                    dropped.push(format!("`{}`", show(value)));
                    return false;
                };
                // Two labels of one template, such as `ubuntu-latest` and
                // `ubuntu-24.04`, are one runner: the second leg would
                // run the same job again.
                if let Some(i) = found.iter().position(|f| template(f) == template(new)) {
                    duplicates.push(format!("`{}` (same runner as `{}`)", show(value), kept[i]));
                    return false;
                }
                found.push(new);
                kept.push(show(value));
                true
            });
            // Every combination of the other keys ran on a dropped value.
            if values.is_empty() {
                matrix.values.clear();
                matrix.exclude.clear();
            }
            if !duplicates.is_empty() {
                self.warn(format!(
                    "{at}: matrix key `{key}` values {} were dropped: each runs on the same runner template as an earlier value, so its legs would repeat that value's jobs",
                    duplicates.join(", ")
                ));
            }
        }
        matrix.include.retain(|entry| {
            let Some(value) = entry.get(key) else {
                return true;
            };
            let Some(new) = value.as_str().and_then(runner_label) else {
                dropped.push(format!("`{}`", show(value)));
                return false;
            };
            if !found.contains(&new) {
                found.push(new);
            }
            true
        });
        // An `exclude` of a dropped leg has nothing left to match.
        matrix.exclude.retain(|entry| {
            entry
                .get(key)
                .is_none_or(|v| v.as_str().and_then(runner_label).is_some())
        });
        if !dropped.is_empty() {
            self.warn(format!(
                "{at}: matrix key `{key}` values {} name no runner template and were dropped; hatchrun runs Linux only",
                dropped.join(", ")
            ));
        }
        if found.is_empty() {
            self.error(format!(
                "{at} has `runs-on: {text}`, and matrix key `{key}` names no runner template; use one of {}",
                template_labels().join(", ")
            ));
            return None;
        }
        let per_leg = found.len() > 1;
        let mut warned = BTreeSet::new();
        let values = matrix.values.get_mut(key).into_iter().flatten();
        let include = matrix.include.iter_mut().filter_map(|e| e.get_mut(key));
        let exclude = matrix.exclude.iter_mut().filter_map(|e| e.get_mut(key));
        for value in values.chain(include).chain(exclude) {
            let Some(old) = value.as_str() else { continue };
            let Some(new) = runner_label(old) else {
                continue;
            };
            if new == old {
                continue;
            }
            if warned.insert(old.to_owned()) {
                self.warn(became(at, old, new));
            }
            if per_leg {
                *value = Value::String(new.to_owned());
            }
        }
        if !per_leg {
            return Some(RunsOn::Label(found[0].to_owned()));
        }
        if plan::ENTRY_KEYS.contains(&key) {
            self.error(format!(
                "{at} has `runs-on: {text}` over legs that run on more than one template, and matrix key `{key}` cannot be passed as a parameter; rename it"
            ));
            return None;
        }
        Some(RunsOn::Matrix(key.to_owned()))
    }

    /// Reads `strategy.matrix`: value lists, `exclude` and `include`. The
    /// rest of `strategy`, and a matrix or key that is an expression,
    /// such as `${{ fromJSON(...) }}`, are dropped with a warning.
    pub(super) fn strategy(
        &mut self,
        at: &str,
        strategy: Option<BTreeMap<String, Value>>,
    ) -> GhMatrix {
        let mut matrix = GhMatrix::default();
        for (key, value) in strategy.unwrap_or_default() {
            if key != "matrix" {
                self.warn(format!(
                    "{at}: `strategy.{key}` is not supported and was dropped"
                ));
                continue;
            }
            let Value::Object(keys) = value else {
                self.warn(format!(
                    "{at}: `strategy.matrix` is not a map of value lists and was dropped"
                ));
                continue;
            };
            for (name, values) in keys {
                match values {
                    _ if name == "include" || name == "exclude" => {
                        let entries = self.combinations(at, &name, values);
                        if name == "include" {
                            matrix.include = entries;
                        } else {
                            matrix.exclude = entries;
                        }
                    }
                    Value::Array(values) => {
                        if values.is_empty() {
                            self.warn(format!(
                                "{at}: matrix key `{name}` has no values and was dropped"
                            ));
                        } else {
                            matrix.values.insert(name, values);
                        }
                    }
                    other => self.warn(format!(
                        "{at}: matrix key `{name}` is `{}`, not a list, and was dropped",
                        show(&other)
                    )),
                }
            }
        }
        // GitHub rejects these; the key may also have been dropped above.
        let values = &matrix.values;
        let mut unknown = Vec::new();
        matrix.exclude.retain(|entry| {
            let missing = entry.keys().find(|k| !values.contains_key(*k));
            if let Some(k) = missing {
                unknown.push(k.clone());
            }
            missing.is_none()
        });
        for key in unknown {
            self.warn(format!(
                "{at}: a `strategy.matrix.exclude` entry names `{key}`, which is not a matrix key, and was dropped"
            ));
        }
        matrix
    }

    /// Reads `strategy.matrix.include` or `exclude`: a list of maps.
    /// Anything else is dropped with a warning.
    pub(super) fn combinations(
        &mut self,
        at: &str,
        name: &str,
        value: Value,
    ) -> Vec<BTreeMap<String, Value>> {
        let Value::Array(items) = value else {
            self.warn(format!(
                "{at}: `strategy.matrix.{name}` is `{}`, not a list of maps, and was dropped",
                show(&value)
            ));
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in items {
            match item {
                Value::Object(map) => out.push(map.into_iter().collect()),
                other => self.warn(format!(
                    "{at}: `strategy.matrix.{name}` entry `{}` is not a map and was dropped",
                    show(&other)
                )),
            }
        }
        out
    }

    /// Applies GitHub's `include` to the matrix and returns the matrix of
    /// each workflow entry the job becomes.
    ///
    /// As on GitHub, `exclude` applies first. Then each `include` entry
    /// extends every combination whose matrix values it does not change
    /// with its other keys (a later entry can change keys an earlier one
    /// added), and an entry that extends none is a combination of its
    /// own. The first matrix holds the combinations no entry extended,
    /// with the others excluded; each extended or added combination gets
    /// a matrix of its own with one value per key. When every
    /// combination was extended the same way, the added keys join the
    /// first matrix instead. A combination with no `leg_key`, which
    /// `runs-on` reads, is dropped with a warning.
    pub(super) fn include(
        &mut self,
        at: &str,
        matrix: GhMatrix,
        leg_key: Option<&str>,
    ) -> Vec<Matrix> {
        let GhMatrix {
            values,
            exclude,
            include,
        } = matrix;
        let base = Matrix { values, exclude };
        if include.is_empty() {
            return vec![base];
        }
        let total = base
            .values
            .values()
            .fold(1usize, |n, v| n.saturating_mul(v.len()));
        if total > plan::MAX_COMBINATIONS {
            self.error(format!(
                "{at} has a matrix of {total} combinations with `include`; the limit is {}",
                plan::MAX_COMBINATIONS
            ));
            return Vec::new();
        }
        // Each combination left after `exclude`: its matrix values and
        // the keys `include` added.
        let mut legs: Vec<(Combo, Combo)> = Vec::new();
        if !base.values.is_empty() {
            for index in (0..total).filter(|&i| !plan::excluded(&base, i)) {
                let mut rest = index;
                let mut combo = BTreeMap::new();
                for (key, values) in base.values.iter().rev() {
                    combo.insert(key.clone(), values[rest % values.len()].clone());
                    rest /= values.len();
                }
                legs.push((combo, BTreeMap::new()));
            }
        }
        let mut added: Vec<Combo> = Vec::new();
        for entry in include {
            let mut matched = false;
            for (combo, extra) in &mut legs {
                if entry
                    .iter()
                    .all(|(k, v)| combo.get(k).is_none_or(|c| c == v))
                {
                    matched = true;
                    for (k, v) in &entry {
                        if !combo.contains_key(k) {
                            extra.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            if matched {
                continue;
            }
            if let Some(key) = leg_key
                && !entry.contains_key(key)
            {
                self.warn(format!(
                    "{at}: a `strategy.matrix.include` entry adds a combination without `{key}`, which `runs-on` reads, and was dropped"
                ));
                continue;
            }
            added.push(entry);
        }
        let mut plain = Vec::new();
        let mut extended = Vec::new();
        let mut left_out = Vec::new();
        for (combo, extra) in legs {
            if leg_key.is_some_and(|k| !combo.contains_key(k) && !extra.contains_key(k)) {
                left_out.push(combo);
            } else if extra.is_empty() {
                plain.push(combo);
            } else {
                extended.push((combo, extra));
            }
        }
        if let Some(key) = leg_key
            && !left_out.is_empty()
        {
            self.warn(format!(
                "{at}: {} matrix combinations get no `{key}`, which `runs-on` reads, from `strategy.matrix.include` and were dropped",
                left_out.len()
            ));
        }
        let mut out = group(base, plain, extended, left_out);
        out.extend(added.into_iter().map(one_each));
        if out.is_empty() {
            self.error(format!("{at} has a matrix with no combination left to run"));
        }
        out
    }

    /// Reads `defaults`, keeping `run.shell` and `run.working-directory`
    /// and dropping the rest with a warning.
    pub(super) fn defaults(&mut self, at: &str, value: Option<&Value>) -> RunDefaults {
        let mut defaults = RunDefaults::default();
        let Some(value) = value else {
            return defaults;
        };
        let Value::Object(map) = value else {
            self.warn(format!("{at}: `defaults` is not a map and was dropped"));
            return defaults;
        };
        for (key, value) in map {
            if key != "run" {
                self.warn(format!(
                    "{at}: `defaults.{key}` is not supported and was dropped"
                ));
                continue;
            }
            let Value::Object(run) = value else {
                self.warn(format!("{at}: `defaults.run` is not a map and was dropped"));
                continue;
            };
            for (key, value) in run {
                let slot = match key.as_str() {
                    "shell" => &mut defaults.shell,
                    "working-directory" => &mut defaults.working_directory,
                    _ => {
                        self.warn(format!(
                            "{at}: `defaults.run.{key}` is not supported and was dropped"
                        ));
                        continue;
                    }
                };
                match scalar(value) {
                    Some(text) => *slot = Some(text),
                    None => self.warn(format!(
                        "{at}: `defaults.run.{key}` is not a string and was dropped"
                    )),
                }
            }
        }
        defaults
    }

    /// Reads a whole number of minutes, dropping an expression.
    pub(super) fn minutes(&mut self, at: &str, key: &str, value: Option<&Value>) -> Option<u32> {
        let value = value?;
        let minutes = value.as_u64().and_then(|m| u32::try_from(m).ok());
        if minutes.is_none() {
            self.warn(format!(
                "{at}: `{key}: {}` is not a whole number and was dropped",
                show(value)
            ));
        }
        minutes
    }

    /// Reads a map of strings, writing numbers and booleans as text.
    pub(super) fn env(
        &mut self,
        at: &str,
        key: &str,
        map: Option<&BTreeMap<String, Value>>,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for (name, value) in map.into_iter().flatten() {
            match scalar(value) {
                Some(text) => {
                    out.insert(name.clone(), text);
                }
                None => self.warn(format!(
                    "{at}: `{key}.{name}` is not a string and was dropped"
                )),
            }
        }
        out
    }

    /// Converts one step. `None` when it is an error, which is recorded.
    pub(super) fn step(&mut self, job_at: &str, index: usize, value: &Value) -> Option<Step> {
        let label = ["name", "id"]
            .iter()
            .find_map(|k| value.get(k).and_then(scalar))
            .map_or_else(|| format!("{}", index + 1), |n| format!("`{n}`"));
        let at = format!("{job_at} step {label}");
        let Some(map) = value.as_object() else {
            self.error(format!("{at} is not a map"));
            return None;
        };
        for key in map.keys().filter(|k| !STEP_KEYS.contains(&k.as_str())) {
            self.warn(format!("{at}: unknown key `{key}` was dropped"));
        }
        let gh = match GhStep::deserialize(value) {
            Ok(gh) => gh,
            Err(err) => {
                self.error(format!("{at}: {err}"));
                return None;
            }
        };
        let continue_on_error = match &gh.continue_on_error {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(Value::String(s)) if s == "true" || s == "false" => s == "true",
            Some(other) => {
                self.warn(format!(
                    "{at}: `continue-on-error: {}` is an expression and was dropped; hatchrun takes only true or false",
                    show(other)
                ));
                false
            }
        };
        let env = self.env(&at, "env", gh.env.as_ref());
        let if_ = gh.if_.as_ref().and_then(scalar);
        match (gh.run, gh.uses) {
            (Some(run), None) => {
                if gh.with.as_ref().is_some_and(|w| !w.is_empty()) {
                    self.warn(format!("{at}: `with` on a run step was dropped"));
                }
                let timeout_minutes =
                    self.minutes(&at, "timeout-minutes", gh.timeout_minutes.as_ref());
                Some(Step::Run(RunStep {
                    name: gh.name,
                    id: gh.id,
                    run,
                    shell: gh.shell,
                    working_directory: gh.working_directory,
                    env,
                    if_,
                    continue_on_error,
                    timeout_minutes,
                }))
            }
            (None, Some(uses)) if uses.starts_with("docker://") => {
                self.error(format!(
                    "{at} has `uses: {uses}`; Docker steps are not supported yet and come in a later release"
                ));
                None
            }
            (None, Some(uses)) => {
                for (key, set) in [
                    ("shell", gh.shell.is_some()),
                    ("working-directory", gh.working_directory.is_some()),
                    ("timeout-minutes", gh.timeout_minutes.is_some()),
                ] {
                    if set {
                        self.warn(format!(
                            "{at}: `{key}` is valid only on run steps and was dropped"
                        ));
                    }
                }
                let step = UsesStep {
                    name: gh.name,
                    id: gh.id,
                    uses,
                    with: gh.with.unwrap_or_default(),
                    env,
                    if_,
                    continue_on_error,
                };
                Some(Step::Uses(step))
            }
            (Some(_), Some(_)) => {
                self.error(format!("{at} has both `run` and `uses`"));
                None
            }
            (None, None) => {
                self.error(format!("{at} has neither `run` nor `uses`"));
                None
            }
        }
    }
}

/// One combination of matrix keys and values.
pub(super) type Combo = BTreeMap<String, Value>;

/// A matrix of one combination: one value per key.
pub(super) fn one_each(combo: Combo) -> Matrix {
    Matrix {
        values: combo.into_iter().map(|(k, v)| (k, vec![v])).collect(),
        exclude: Vec::new(),
    }
}

/// The matrices of a job's combinations after `include`: `plain` ones
/// no entry extended, `extended` ones with the keys added, and
/// `left_out` ones that run nowhere. `base` runs the plain ones, with
/// the others excluded, unless that is more `exclude` entries than a
/// matrix may have; then each plain one gets a matrix of its own, as
/// each extended one does. When every combination was extended alike,
/// `base` takes the added keys and runs them all.
pub(super) fn group(
    mut base: Matrix,
    plain: Vec<Combo>,
    extended: Vec<(Combo, Combo)>,
    left_out: Vec<Combo>,
) -> Vec<Matrix> {
    let mut out = Vec::new();
    let same = extended.first().map(|(_, extra)| extra);
    if plain.is_empty() && left_out.is_empty() && extended.iter().all(|(_, e)| Some(e) == same) {
        if let Some(extra) = same {
            for (k, v) in extra {
                base.values.insert(k.clone(), vec![v.clone()]);
            }
            out.push(base);
        }
        return out;
    }
    let taken = extended.len() + left_out.len();
    if !plain.is_empty() && base.exclude.len() + taken <= plan::MAX_EXCLUDE {
        base.exclude
            .extend(extended.iter().map(|(combo, _)| combo.clone()));
        base.exclude.extend(left_out);
        out.push(base);
    } else {
        out.extend(plain.into_iter().map(one_each));
    }
    out.extend(extended.into_iter().map(|(mut combo, extra)| {
        combo.extend(extra);
        one_each(combo)
    }));
    out
}

/// Lists the entries of each converted job, given with its GitHub id,
/// with `requires` naming every entry each needed job became.
pub(super) fn flatten(entries: Vec<(String, Vec<WorkflowJob>)>) -> Vec<WorkflowJob> {
    let names: BTreeMap<String, Vec<String>> = entries
        .iter()
        .map(|(id, e)| {
            (
                id.clone(),
                e.iter().filter_map(|e| e.name.clone()).collect(),
            )
        })
        .collect();
    entries
        .into_iter()
        .flat_map(|(_, e)| e)
        .map(|mut entry| {
            entry.requires = entry
                .requires
                .iter()
                .flat_map(|r| {
                    names.get(&r.job).into_iter().flatten().map(|name| Require {
                        job: name.clone(),
                        statuses: r.statuses,
                    })
                })
                .collect();
            entry
        })
        .collect()
}

/// The workflow entries of job `key`, GitHub id `id`, one per matrix.
/// The first is named `id` and the others `id-2`, `id-3` and on,
/// skipping the ids of the workflow's other jobs.
pub(super) fn entries(
    id: &str,
    key: &str,
    ids: &BTreeSet<String>,
    requires: &[String],
    matrices: Vec<Matrix>,
) -> Vec<WorkflowJob> {
    let mut n = 1;
    matrices
        .into_iter()
        .enumerate()
        .map(|(i, matrix)| {
            let name = if i == 0 {
                id.to_owned()
            } else {
                loop {
                    n += 1;
                    let name = format!("{id}-{n}");
                    if !ids.contains(&name) {
                        break name;
                    }
                }
            };
            WorkflowJob {
                job: key.to_owned(),
                name: Some(name),
                requires: requires
                    .iter()
                    .map(String::as_str)
                    .map(Require::new)
                    .collect(),
                matrix,
                ..WorkflowJob::default()
            }
        })
        .collect()
}

/// `defaults.run` of a workflow or job: what its run steps get when they
/// set no shell or working directory of their own.
#[derive(Default)]
pub(super) struct RunDefaults {
    shell: Option<String>,
    working_directory: Option<String>,
}

impl RunDefaults {
    /// Fills in what each run step leaves unset.
    pub(super) fn apply(&self, steps: &mut [Step]) {
        for step in steps {
            if let Step::Run(run) = step {
                if run.shell.is_none() {
                    run.shell.clone_from(&self.shell);
                }
                if run.working_directory.is_none() {
                    run.working_directory.clone_from(&self.working_directory);
                }
            }
        }
    }
}

/// The template label a GitHub `runs-on` label becomes, if any: the
/// label itself, the template a retired Ubuntu label moved to, or the
/// template of a third-party label's arch and size.
pub(super) fn runner_label(label: &str) -> Option<&'static str> {
    if let Some((_, new)) = OLD_UBUNTU.iter().find(|(old, _)| *old == label) {
        return Some(new);
    }
    if let Some(found) = template_labels().into_iter().find(|l| *l == label) {
        return Some(found);
    }
    vendor_template(label).map(|(t, _)| t.labels[0])
}

/// The warning for a label that became another template label, saying
/// when a third-party label's size was rounded.
pub(super) fn became(at: &str, old: &str, new: &str) -> String {
    let size = match vendor_template(old) {
        Some((t, asked)) if asked > t.vcpu => format!(
            "; it asks for {asked} vCPUs and the largest template has {}",
            t.vcpu
        ),
        Some((t, asked)) if asked < t.vcpu => {
            format!("; it asks for {asked} vCPUs, rounded up to {}", t.vcpu)
        }
        _ => String::new(),
    };
    format!("{at}: `runs-on: {old}` became `{new}`, which runs Ubuntu 26.04{size}")
}

/// The label `runs-on` names: a string, a list of one, or the `labels`
/// of a `{labels: ...}` map. `None` for anything else, including a
/// runner `group`.
pub(super) fn runs_on_text(runs_on: &Value) -> Option<&str> {
    match runs_on {
        Value::String(label) => Some(label),
        Value::Array(labels) if labels.len() == 1 => labels[0].as_str(),
        Value::Object(map) if map.len() == 1 => runs_on_text(map.get("labels")?),
        _ => None,
    }
}

/// The key `KEY` of `${{ matrix.KEY }}`, when that is the whole text.
pub(super) fn matrix_key(text: &str) -> Option<&str> {
    let key = text
        .trim()
        .strip_prefix("${{")?
        .strip_suffix("}}")?
        .trim()
        .strip_prefix("matrix.")?;
    let ok = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    ok.then_some(key)
}

/// A dispatch input default as a value of `kind`: numeric strings become
/// integers and `"true"`/`"false"` booleans. `None` when it does not fit.
pub(super) fn fit_default(kind: &ParameterKind, value: &Value) -> Option<Value> {
    match kind {
        ParameterKind::String => scalar(value).map(Value::String),
        ParameterKind::Boolean => match value {
            Value::Bool(_) => Some(value.clone()),
            Value::String(s) if s == "true" || s == "false" => Some(Value::Bool(s == "true")),
            _ => None,
        },
        ParameterKind::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Some(value.clone()),
            Value::String(s) => s.trim().parse::<i64>().ok().map(Value::from),
            _ => None,
        },
        ParameterKind::Enum(options) => scalar(value)
            .filter(|s| options.contains(s))
            .map(Value::String),
    }
}
