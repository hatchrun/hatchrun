// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `uses:` steps that run composite actions: the inner steps run on the
//! job's step runner with the action's own `inputs` and `steps`.

use std::collections::BTreeMap;

use hatchrun_workflow::{Annotation, Conclusion, JobStatus, RunStep, StepSummary, UsesStep};
use serde_json::Value;

use super::action::{Action, ActionRef, CompositeStep};
use super::node::action_name;
use super::sink::Sink;
use super::step::StepOutput;
use super::{Job, Line, interp};

/// How many composite actions may nest. The eleventh level fails.
const MAX_DEPTH: usize = 10;

/// The sink for the inner steps of a composite action. They are not
/// steps of the job, so a header is a plain line and nothing ends.
struct Inner<'a> {
    sink: &'a mut dyn Sink,
}

impl Sink for Inner<'_> {
    fn step_started(&mut self, _index: usize, name: &str) {
        self.sink.line(&format!("== {name}"));
    }

    fn line(&mut self, text: &str) {
        self.sink.line(text);
    }

    fn annotation(&mut self, annotation: &Annotation) {
        self.sink.annotation(annotation);
    }

    fn step_finished(&mut self, _index: usize, _summary: &StepSummary) {}
}

/// Checks every step before the first one runs, as GitHub does when it
/// loads the action.
fn validate(steps: &[CompositeStep]) -> Result<(), String> {
    for step in steps {
        match (&step.run, &step.uses) {
            (Some(_), None) if step.shell.is_none() => {
                return Err("Required property is missing: shell".to_owned());
            }
            (Some(_), None) | (None, Some(_)) => {}
            _ => return Err("a composite step needs either `run` or `uses`, not both".to_owned()),
        }
    }
    Ok(())
}

impl Job<'_> {
    /// Runs a composite action's steps for the `uses:` step `step`.
    /// Returns the step's outcome and its outputs.
    pub(super) async fn composite(
        &mut self,
        step: &UsesStep,
        action: &Action,
        r: &ActionRef,
        steps: &[CompositeStep],
        sink: &mut dyn Sink,
    ) -> (Conclusion, BTreeMap<String, String>) {
        let failed = |job: &mut Self, sink: &mut dyn Sink, message: &str| {
            job.say(sink, Line::Error(message));
            (Conclusion::Failure, BTreeMap::new())
        };
        if self.depth >= MAX_DEPTH {
            let message = format!(
                "`{}` is nested more than {MAX_DEPTH} composite actions deep",
                step.uses
            );
            return failed(self, sink, &message);
        }
        if let Err(message) = validate(steps) {
            return failed(self, sink, &message);
        }
        let name = action_name(step, r);
        let evaluated = match self.evaluate(step, action, r, &name) {
            Ok(evaluated) => evaluated,
            Err(message) => return failed(self, sink, &message),
        };
        for warning in &evaluated.warnings {
            self.say(sink, Line::Warning(warning));
        }

        // Inside, the contexts are the caller's with the action's own
        // `inputs` and `steps` and no secrets. The step's `env` applies
        // to every inner step.
        let caller_ctx = self.ctx.clone();
        let caller_status = self.status;
        let caller_scope = self.scope_env.clone();
        self.depth += 1;
        self.scope_env.extend(evaluated.step_env);
        self.scope_env.insert(
            "GITHUB_ACTION_PATH".to_owned(),
            evaluated.scope.path.clone(),
        );
        self.ctx = caller_ctx.with_action(evaluated.scope);
        // A cancelled job keeps cancelling: `success()` stays false.
        let start = match caller_status {
            JobStatus::Cancelled => JobStatus::Cancelled,
            _ => JobStatus::Success,
        };
        self.set_status(start);
        self.sync_env();

        let mut cancelled = false;
        let mut failure = false;
        let mut inner = Inner { sink };
        for inner_step in steps {
            if self.aborted {
                break;
            }
            let (outcome, conclusion) = self.inner_step(inner_step, &mut inner).await;
            cancelled |= outcome == Conclusion::Cancelled;
            failure |= conclusion == Conclusion::Failure;
        }

        let workspace = self.layout.workspace.clone();
        let mut pairs = Vec::new();
        for (id, output) in &action.meta.outputs {
            let Some(value) = &output.value else {
                continue;
            };
            match interp(value, &self.ctx, &workspace, self.status) {
                Ok(text) => pairs.push((id.clone(), text)),
                Err(err) => {
                    self.say(inner.sink, Line::Error(&format!("output {id}: {err}")));
                    failure = true;
                }
            }
        }
        let outputs = {
            let mut out = StepOutput::new(
                &mut self.masker,
                inner.sink,
                &mut self.annotations,
                &mut self.budget,
            );
            out.push_outputs(pairs);
            out.take_outputs().into_iter().collect()
        };

        self.depth -= 1;
        self.scope_env = caller_scope;
        self.ctx = caller_ctx;
        self.set_status(caller_status);
        self.sync_env();
        let outcome = if cancelled {
            Conclusion::Cancelled
        } else if failure {
            Conclusion::Failure
        } else {
            Conclusion::Success
        };
        (outcome, outputs)
    }

    /// Runs one inner step through the same runners as a job's steps.
    /// Returns its outcome and its conclusion.
    async fn inner_step(
        &mut self,
        step: &CompositeStep,
        sink: &mut dyn Sink,
    ) -> (Conclusion, Conclusion) {
        let workspace = self.layout.workspace.clone();
        let continue_on_error = match &step.continue_on_error {
            None => Ok(false),
            Some(text) => interp(text, &self.ctx, &workspace, self.status)
                .map(|text| text.trim() == "true")
                .map_err(|err| format!("continue-on-error: {err}")),
        };
        let continue_on_error = match continue_on_error {
            Ok(value) => value,
            Err(message) => {
                self.say(sink, Line::Error(&message));
                let conclusion = self.settle(Conclusion::Failure, false);
                return (Conclusion::Failure, conclusion);
            }
        };

        let slot = self.fresh_slot();
        let (common, outcome, outputs) = match (&step.run, &step.uses) {
            (Some(run), None) => {
                let run = RunStep {
                    name: step.name.clone(),
                    id: step.id.clone(),
                    run: run.clone(),
                    shell: step.shell.clone(),
                    working_directory: step.working_directory.clone(),
                    env: step.env.clone(),
                    if_: step.if_.clone(),
                    continue_on_error,
                    timeout_minutes: None,
                };
                self.run_step(slot, &run, sink).await
            }
            (None, Some(uses)) => {
                let uses = UsesStep {
                    name: step.name.clone(),
                    id: step.id.clone(),
                    uses: uses.clone(),
                    with: step
                        .with
                        .iter()
                        .map(|(key, value)| (key.clone(), Value::String(value.clone())))
                        .collect(),
                    env: step.env.clone(),
                    if_: step.if_.clone(),
                    continue_on_error,
                };
                // A composite action may use composite actions, so this
                // future would contain itself without the box.
                Box::pin(self.uses_step(slot, &uses, sink)).await
            }
            // `validate` rejected anything else.
            _ => return (Conclusion::Failure, Conclusion::Failure),
        };
        let conclusion = self.settle(outcome, common.continue_on_error);
        if let Some(id) = &common.id {
            self.ctx.record_step(id, outcome, conclusion, &outputs);
        }
        (outcome, conclusion)
    }
}
