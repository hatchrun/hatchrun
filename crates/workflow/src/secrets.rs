// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The secrets a job references, found without running it.
//!
//! The Worker puts only these in a job's document, so a job never
//! receives a secret its workflow does not name.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{CacheStep, RunStep, UsesStep, WorkspaceStep};
use crate::expr;
use crate::plan::{PlannedJob, PlannedStep};

/// Read by every job when set, as on GitHub.
pub const DEBUG_SECRETS: [&str; 2] = ["ACTIONS_RUNNER_DEBUG", "ACTIONS_STEP_DEBUG"];

/// The secrets a text or job reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRefs {
    /// Every secret is needed: a name the scan cannot see.
    pub all: bool,
    /// Uppercase names.
    pub names: BTreeSet<String>,
}

impl SecretRefs {
    /// Adds a name, uppercased. `GITHUB_TOKEN` is the job token, not a
    /// stored secret, and is left out.
    pub(crate) fn add(&mut self, name: &str) {
        let name = name.to_ascii_uppercase();
        if name != "GITHUB_TOKEN" {
            self.names.insert(name);
        }
    }

    fn merge(&mut self, other: Self) {
        self.all |= other.all;
        self.names.extend(other.names);
    }

    /// Reads `text`; text that does not parse names nothing, since
    /// compile already reported it.
    fn scan(&mut self, text: &str, bare: bool) {
        if let Ok(found) = expr::secret_refs(text, bare) {
            self.merge(found);
        }
    }

    fn scan_all<'a>(&mut self, texts: impl IntoIterator<Item = &'a String>) {
        for text in texts {
            self.scan(text, false);
        }
    }

    fn scan_opt(&mut self, text: Option<&str>, bare: bool) {
        if let Some(text) = text {
            self.scan(text, bare);
        }
    }

    fn scan_value(&mut self, value: &Value) {
        match value {
            Value::String(text) => self.scan(text, false),
            Value::Array(items) => {
                for v in items {
                    self.scan_value(v);
                }
            }
            Value::Object(map) => {
                for v in map.values() {
                    self.scan_value(v);
                }
            }
            _ => {}
        }
    }

    fn scan_step(&mut self, step: &PlannedStep) {
        match step {
            PlannedStep::Run(RunStep {
                name,
                id,
                run,
                shell,
                working_directory,
                env,
                if_,
                ..
            }) => {
                self.scan_opt(name.as_deref(), false);
                self.scan_opt(id.as_deref(), false);
                self.scan(run, false);
                self.scan_opt(shell.as_deref(), false);
                self.scan_opt(working_directory.as_deref(), false);
                self.scan_all(env.values());
                self.scan_opt(if_.as_deref(), true);
            }
            PlannedStep::Uses(UsesStep {
                name,
                id,
                uses,
                with,
                env,
                if_,
                ..
            }) => {
                self.scan_opt(name.as_deref(), false);
                self.scan_opt(id.as_deref(), false);
                self.scan(uses, false);
                for v in with.values() {
                    self.scan_value(v);
                }
                self.scan_all(env.values());
                self.scan_opt(if_.as_deref(), true);
            }
            PlannedStep::Cache(CacheStep {
                key,
                restore_keys,
                paths,
            }) => {
                self.scan(key, false);
                self.scan_all(restore_keys);
                self.scan_all(paths);
            }
            PlannedStep::Workspace(WorkspaceStep::Persist { root, paths }) => {
                self.scan(root, false);
                self.scan_all(paths);
            }
            PlannedStep::Workspace(WorkspaceStep::Attach { at }) => self.scan(at, false),
            PlannedStep::TestResults { path } => self.scan(path, false),
            PlannedStep::Checkout(checkout) => {
                self.scan_opt(checkout.r#ref.as_deref(), false);
                self.scan_opt(checkout.path.as_deref(), false);
            }
        }
    }
}

impl PlannedJob {
    /// The secrets this job reads: every `${{ secrets.X }}` in its
    /// `env`, `outputs` and steps, and the secrets its `if:` conditions
    /// name, plus [`DEBUG_SECRETS`].
    #[must_use]
    pub fn secret_refs(&self) -> SecretRefs {
        let mut refs = SecretRefs::default();
        for name in DEBUG_SECRETS {
            refs.add(name);
        }
        refs.scan_all(self.env.values());
        refs.scan_all(self.outputs.values());
        refs.scan_opt(self.if_.as_deref(), true);
        for step in &self.steps {
            refs.scan_step(step);
        }
        refs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expr::secret_refs;

    fn names(text: &str, bare: bool) -> SecretRefs {
        secret_refs(text, bare).expect("parses")
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn named_reference() {
        let r = names("${{ secrets.NPM_TOKEN }}", false);
        assert_eq!(r.names, set(&["NPM_TOKEN"]));
        assert!(!r.all);
    }

    #[test]
    fn index_with_a_literal() {
        assert_eq!(names("${{ secrets['A_B'] }}", false).names, set(&["A_B"]));
    }

    #[test]
    fn names_are_case_insensitive() {
        assert_eq!(
            names("${{ secrets.npm_token }}", false).names,
            set(&["NPM_TOKEN"])
        );
    }

    #[test]
    fn dynamic_index_sends_every_secret() {
        assert!(names("${{ secrets[matrix.key] }}", false).all);
    }

    #[test]
    fn to_json_of_secrets_sends_every_secret() {
        assert!(names("${{ toJSON(secrets) }}", false).all);
    }

    #[test]
    fn bare_if_is_read() {
        assert_eq!(names("secrets.DEPLOY != ''", true).names, set(&["DEPLOY"]));
    }

    #[test]
    fn github_token_is_not_a_stored_secret() {
        assert_eq!(
            names("${{ secrets.GITHUB_TOKEN }}", false),
            SecretRefs::default()
        );
    }

    #[test]
    fn string_literal_is_not_a_reference() {
        assert_eq!(names("${{ 'secrets.X' }}", false), SecretRefs::default());
    }

    #[test]
    fn job_scan_covers_env_steps_with_and_if() {
        use crate::templates::TEMPLATES;
        use std::collections::BTreeMap;
        let step = PlannedStep::Uses(UsesStep {
            uses: "actions/x@v1".into(),
            with: BTreeMap::from([("t".into(), Value::String("${{ secrets.TWO }}".into()))]),
            if_: Some("secrets.THREE".into()),
            ..UsesStep::default()
        });
        let job = PlannedJob {
            id: "j".into(),
            name: "j".into(),
            requires: vec![],
            statuses: BTreeMap::new(),
            kind: crate::plan::JobKind::Run {
                template: &TEMPLATES[0],
            },
            matrix: BTreeMap::new(),
            node: None,
            env: BTreeMap::from([("A".into(), "${{ secrets.ONE }}".into())]),
            if_: None,
            timeout_minutes: 10,
            outputs: BTreeMap::new(),
            token_permissions: BTreeMap::new(),
            environment: None,
            steps: vec![step],
        };
        let refs = job.secret_refs();
        assert!(!refs.all);
        let mut want = set(&["ONE", "TWO", "THREE"]);
        want.extend(DEBUG_SECRETS.iter().map(|s| (*s).to_string()));
        assert_eq!(refs.names, want);
    }

    #[test]
    fn a_checkout_step_is_scanned_for_secrets() {
        let mut refs = SecretRefs::default();
        refs.scan_step(&PlannedStep::Checkout(crate::config::CheckoutStep {
            r#ref: Some("${{ secrets.REF }}".into()),
            path: Some("${{ secrets.DIR }}".into()),
            ..crate::config::CheckoutStep::default()
        }));
        assert_eq!(refs.names, set(&["REF", "DIR"]));
    }
}
