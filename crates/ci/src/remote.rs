// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Reads the reusable workflows of other repositories that a config
//! calls (`call: owner/repo/path.yml@ref`), before the workflow is
//! planned.
//!
//! A file is read with the installation token of the caller's tenant,
//! at the commit its ref names now. That commit is recorded in the plan
//! (`owner/repo/path.yml@<sha>`), so a run keeps the file it started
//! with. The file is a GitHub workflow with `on: workflow_call`, read
//! through the converter.
//!
//! Who may be called: the installation must be able to read the
//! repository, and the repository must belong to the caller's tenant or
//! be public. GitHub also lets a private repository share its workflows
//! through its "Access" setting (Settings, Actions, General); hatchrun
//! does not read that setting yet, so a private repository of another
//! tenant is refused whatever it says.

use hatchrun_cloud::db::Db;
use hatchrun_cloud::fetch::Counted;
use hatchrun_cloud::tokens::{App, Kv, Tokens};
use hatchrun_workflow::{
    Callee, Config, Library, MAX_UNIQUE_CALLEES, RemoteCall, convert, remote_calls_from,
};

use crate::env::Settings;
use crate::github;
use crate::runs::commit_of;
use crate::store;

/// What GitHub and the database say about the repositories a config
/// calls. Errors are those worth a retry; a repository or file that is
/// not there is an answer.
#[allow(async_fn_in_trait)] // Workers run one thread; no `Send` is needed.
pub trait Source {
    /// Whether the repository is private, `None` when the installation
    /// cannot read it.
    async fn private(&self, repo: &str) -> Result<Option<bool>, String>;
    /// Whether the caller's tenant owns the repository.
    async fn owned(&self, repo: &str) -> Result<bool, String>;
    /// The commit `git_ref` names, `None` when the repository has no
    /// such ref.
    async fn sha(&self, repo: &str, git_ref: &str) -> Result<Option<String>, String>;
    /// The file at a commit, `None` when it is not there.
    async fn file(&self, repo: &str, path: &str, sha: &str) -> Result<Option<String>, String>;
}

/// Why a repository may not be called, `None` when it may. The caller's
/// own repository and those of its tenant always may; any other may only
/// when it is public.
#[must_use]
pub fn refusal(call: &RemoteCall, owned: bool, private: bool) -> Option<String> {
    if owned || !private {
        return None;
    }
    Some(format!(
        "{} is a private repository that does not belong to this account; a workflow may call files of the account's own repositories and of public ones. GitHub's \"Access\" setting for sharing a private repository's workflows is not read by hatchrun; make the repository public, or move the workflow into a repository of this account",
        call.repository()
    ))
}

/// Reads every file of another repository that the workflows `workflows`
/// of `config` call, and those they call. A file that may not be used is recorded in the
/// library with the reason, for the plan to report where it is called.
///
/// # Errors
///
/// GitHub or the database failed; the caller retries the delivery.
pub async fn read_library<S: Source>(
    source: &S,
    config: &Config,
    workflows: &[&str],
) -> Result<Library, String> {
    let mut library = Library::default();
    // The limit counts the files one workflow reaches, as the planner
    // counts them for one plan.
    for name in workflows {
        let mut read = 0usize;
        loop {
            let wanted = remote_calls_from(config, &[name], &library);
            if wanted.is_empty() {
                break;
            }
            for call in wanted {
                read += 1;
                if read > MAX_UNIQUE_CALLEES {
                    library.refuse(
                        call,
                        format!("a run may call at most {MAX_UNIQUE_CALLEES} different workflows"),
                    );
                    continue;
                }
                match read_one(source, &call).await? {
                    Ok(callee) => library.insert(call, callee),
                    Err(reason) => library.refuse(call, reason),
                }
            }
        }
    }
    Ok(library)
}

/// The file `call` names as a callee, or why it cannot be one.
async fn read_one<S: Source>(
    source: &S,
    call: &RemoteCall,
) -> Result<Result<Callee, String>, String> {
    let repo = call.repository();
    let Some(private) = source.private(&repo).await? else {
        return Ok(Err(format!(
            "{repo} cannot be read: the GitHub App is not installed on it, or it does not exist"
        )));
    };
    let owned = source.owned(&repo).await?;
    if let Some(reason) = refusal(call, owned, private) {
        return Ok(Err(reason));
    }
    let Some(sha) = source.sha(&repo, &call.git_ref).await? else {
        return Ok(Err(format!("{repo} has no ref `{}`", call.git_ref)));
    };
    let Some(text) = source.file(&repo, &call.path, &sha).await? else {
        return Ok(Err(format!(
            "{repo} has no file `{}` at {} ({})",
            call.path,
            call.git_ref,
            &sha[..sha.len().min(12)]
        )));
    };
    Ok(callee(call, &text, &sha))
}

/// The file's workflow as a callee: the GitHub workflow converted, which
/// must have `on: workflow_call` and convert whole. A file that calls
/// another file of its own repository (`uses: ./...`) does not: that
/// path would name a file of the caller.
///
/// # Errors
///
/// What is wrong with the file.
pub fn callee(call: &RemoteCall, text: &str, sha: &str) -> Result<Callee, String> {
    let shown = format!("{}/{}", call.repository(), call.path);
    let base = call.path.rsplit('/').next().unwrap_or(&call.path);
    let lower = base.to_ascii_lowercase();
    let name = lower
        .strip_suffix(".yml")
        .or_else(|| lower.strip_suffix(".yaml"));
    let Some(name) = name.map(|n| base[..n.len()].to_owned()) else {
        return Err(format!("{shown} is not a .yml or .yaml workflow file"));
    };
    let converted = convert(&[(call.path.clone(), text.to_owned())]).map_err(|errors| {
        let found: Vec<String> = errors.iter().map(|d| d.message.clone()).collect();
        format!("{shown} does not convert: {}", found.join("; "))
    })?;
    let broken: Vec<&str> = converted
        .errors
        .iter()
        .chain(&converted.warnings)
        .map(|d| d.message.as_str())
        .filter(|m| m.contains("the job was dropped") || m.contains("` was dropped: "))
        .collect();
    if !converted.errors.is_empty() || !broken.is_empty() {
        let found: Vec<&str> = converted
            .errors
            .iter()
            .map(|d| d.message.as_str())
            .chain(broken)
            .collect();
        return Err(format!(
            "{shown} does not convert whole: {}",
            found.join("; ")
        ));
    }
    let Some(workflow) = converted.config.workflows.get(&name) else {
        return Err(format!("{shown} converted to no workflow"));
    };
    if workflow.on.call.is_none() {
        return Err(format!(
            "{shown} has no `on: workflow_call`, so it is not a reusable workflow"
        ));
    }
    Ok(Callee {
        config: converted.config,
        workflow: name,
        sha: sha.to_owned(),
    })
}

/// The Worker's view of GitHub and the database for one tenant.
pub struct Github<'a> {
    /// The database.
    pub db: &'a Db,
    /// Installation tokens.
    pub tokens: &'a Tokens<'a, Kv, App, Counted>,
    /// The Worker's settings.
    pub settings: &'a Settings,
    /// The tenant's installation.
    pub installation_id: u64,
    /// The tenant.
    pub tenant_id: &'a str,
}

impl Source for Github<'_> {
    async fn private(&self, repo: &str) -> Result<Option<bool>, String> {
        let response = self
            .tokens
            .send(self.installation_id, |token| {
                github::repo_by_name(&self.settings.github_api, repo, token)
            })
            .await
            .map_err(|e| e.to_string())?;
        github::repo_is_private(&response)
    }

    async fn owned(&self, repo: &str) -> Result<bool, String> {
        let row = store::repo_by_name(self.db, repo)
            .await
            .map_err(|e| e.to_string())?;
        Ok(row.is_some_and(|row| row.tenant_id == self.tenant_id))
    }

    async fn sha(&self, repo: &str, git_ref: &str) -> Result<Option<String>, String> {
        let response = self
            .tokens
            .send(self.installation_id, |token| {
                github::commit_sha(&self.settings.github_api, repo, git_ref, token)
            })
            .await
            .map_err(|e| e.to_string())?;
        commit_of(&response)
    }

    async fn file(&self, repo: &str, path: &str, sha: &str) -> Result<Option<String>, String> {
        let response = self
            .tokens
            .send(self.installation_id, |token| {
                github::file_at(&self.settings.github_api, repo, path, sha, token)
            })
            .await
            .map_err(|e| e.to_string())?;
        github::file_text(&response, &format!("{repo}/{path}"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use hatchrun_workflow::{compile_with, parse, remote_calls};

    use super::*;

    const REUSABLE: &str = "
on:
  workflow_call:
    inputs:
      target: { type: string, required: true }
    secrets:
      TOKEN: { required: false }
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: make ${{ inputs.target }}
        env: { T: '${{ secrets.TOKEN }}' }
";

    /// Repositories by name: private, owned by the tenant, and files.
    #[derive(Default)]
    struct Fake {
        repos: BTreeMap<&'static str, (bool, bool)>,
        files: BTreeMap<(&'static str, &'static str), &'static str>,
        /// Every path of a known repository is the reusable file.
        any_file: bool,
    }

    #[allow(clippy::unused_async_trait_impl)]
    impl Source for Fake {
        async fn private(&self, repo: &str) -> Result<Option<bool>, String> {
            Ok(self.repos.get(repo).map(|(private, _)| *private))
        }

        async fn owned(&self, repo: &str) -> Result<bool, String> {
            Ok(self.repos.get(repo).is_some_and(|(_, owned)| *owned))
        }

        async fn sha(&self, repo: &str, git_ref: &str) -> Result<Option<String>, String> {
            Ok((git_ref != "nope" && self.repos.contains_key(repo))
                .then(|| "0123456789abcdef0123456789abcdef01234567".to_owned()))
        }

        async fn file(&self, repo: &str, path: &str, _sha: &str) -> Result<Option<String>, String> {
            if self.any_file && self.repos.contains_key(repo) {
                return Ok(Some(REUSABLE.to_owned()));
            }
            Ok(self
                .files
                .iter()
                .find(|((r, p), _)| *r == repo && *p == path)
                .map(|(_, text)| (*text).to_owned()))
        }
    }

    fn fake(repo: &'static str, private: bool, owned: bool) -> Fake {
        let mut fake = Fake::default();
        fake.repos.insert(repo, (private, owned));
        fake.files
            .insert((repo, ".github/workflows/build.yml"), REUSABLE);
        fake
    }

    fn caller(call: &str) -> Config {
        parse(&format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs:
      - up:
          call: {call}
          with: {{ target: web }}
          secrets: {{ TOKEN: '${{{{ secrets.DEPLOY }}}}' }}
"
        ))
        .unwrap()
    }

    fn read(source: &Fake, config: &Config) -> Library {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_library(source, config, &["ci"]))
            .unwrap()
    }

    fn plan_error(library: &Library, config: &Config) -> String {
        compile_with(config, "ci", "refs/heads/main", library)
            .unwrap_err()
            .iter()
            .map(|d| d.message.clone())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const CALL: &str = "acme/shared/.github/workflows/build.yml@v1";

    #[test]
    fn a_public_repository_of_another_tenant_may_be_called() {
        let config = caller(CALL);
        let library = read(&fake("acme/shared", false, false), &config);
        let plan = compile_with(&config, "ci", "refs/heads/main", &library).unwrap();
        assert_eq!(plan.jobs[0].id, "up / build");
        assert_eq!(
            plan.jobs[0].scope[0].source.as_deref(),
            Some(
                "acme/shared/.github/workflows/build.yml@0123456789abcdef0123456789abcdef01234567"
            )
        );
    }

    #[test]
    fn a_private_repository_of_the_tenant_may_be_called() {
        let config = caller(CALL);
        let library = read(&fake("acme/shared", true, true), &config);
        assert!(compile_with(&config, "ci", "refs/heads/main", &library).is_ok());
    }

    #[test]
    fn a_private_repository_of_another_tenant_is_refused_with_the_reason() {
        let config = caller(CALL);
        let library = read(&fake("acme/shared", true, false), &config);
        let message = plan_error(&library, &config);
        assert!(
            message.contains(
                "acme/shared is a private repository that does not belong to this account"
            ),
            "{message}"
        );
        assert!(message.contains("\"Access\" setting"), "{message}");
        assert!(message.contains("workflow `ci` entry `up`"), "{message}");
    }

    #[test]
    fn a_repository_the_installation_cannot_read_is_refused() {
        let config = caller(CALL);
        let library = read(&Fake::default(), &config);
        let message = plan_error(&library, &config);
        assert!(
            message.contains("cannot be read: the GitHub App is not installed"),
            "{message}"
        );
    }

    #[test]
    fn a_missing_ref_or_file_is_named() {
        let config = caller("acme/shared/.github/workflows/build.yml@nope");
        let library = read(&fake("acme/shared", false, true), &config);
        assert!(plan_error(&library, &config).contains("has no ref `nope`"));
        let config = caller("acme/shared/.github/workflows/other.yml@v1");
        let library = read(&fake("acme/shared", false, true), &config);
        assert!(
            plan_error(&library, &config).contains("has no file `.github/workflows/other.yml`")
        );
    }

    #[test]
    fn a_file_without_workflow_call_is_not_reusable() {
        let mut source = fake("acme/shared", false, true);
        source.files.insert(
            ("acme/shared", ".github/workflows/build.yml"),
            "on: push\njobs:\n  j: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        );
        let config = caller(CALL);
        let library = read(&source, &config);
        assert!(plan_error(&library, &config).contains("has no `on: workflow_call`"));
    }

    #[test]
    fn a_file_that_calls_a_file_of_its_own_repository_is_refused() {
        let mut source = fake("acme/shared", false, true);
        source.files.insert(
            ("acme/shared", ".github/workflows/build.yml"),
            "on: workflow_call\njobs:\n  j:\n    uses: ./.github/workflows/inner.yml\n",
        );
        let config = caller(CALL);
        let library = read(&source, &config);
        assert!(plan_error(&library, &config).contains("does not convert whole"));
    }

    #[test]
    fn a_file_that_calls_another_repository_is_read_too() {
        let mut source = fake("acme/shared", false, true);
        source.repos.insert("acme/deep", (false, false));
        source.files.insert(
            ("acme/shared", ".github/workflows/build.yml"),
            "on: workflow_call\njobs:\n  j:\n    uses: acme/deep/.github/workflows/leaf.yml@v2\n",
        );
        source.files.insert(
            ("acme/deep", ".github/workflows/leaf.yml"),
            "on: workflow_call\njobs:\n  leaf: { runs-on: ubuntu-latest, steps: [{ run: x }] }\n",
        );
        let config = parse(&format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [{{ up: {{ call: '{CALL}' }} }}]
"
        ))
        .unwrap();
        let library = read(&source, &config);
        let plan = compile_with(&config, "ci", "refs/heads/main", &library)
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(plan.jobs[0].id, "up / j / leaf");
        assert_eq!(plan.jobs[0].scope.len(), 2);
        assert_eq!(remote_calls(&config, &library).len(), 0);
    }

    #[test]
    fn the_callee_gets_only_the_secrets_the_call_passes() {
        let config = caller(CALL);
        let library = read(&fake("acme/shared", false, false), &config);
        let plan = compile_with(&config, "ci", "refs/heads/main", &library).unwrap();
        let job = &plan.jobs[0];
        assert!(job.secret_refs().names.contains("DEPLOY"));
        let opened: BTreeMap<String, String> = [("DEPLOY", "d"), ("OTHER", "o")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let seen = job.visible_secrets(&opened);
        assert_eq!(seen.get("TOKEN").map(String::as_str), Some("d"));
        assert!(!seen.contains_key("OTHER") && !seen.contains_key("DEPLOY"));
    }

    #[test]
    fn the_policy_names_the_repository() {
        let call = RemoteCall {
            owner: "o".into(),
            repo: "r".into(),
            path: "p.yml".into(),
            git_ref: "main".into(),
        };
        assert!(refusal(&call, true, true).is_none());
        assert!(refusal(&call, false, false).is_none());
        assert!(
            refusal(&call, false, true)
                .unwrap()
                .starts_with("o/r is a private repository")
        );
    }

    #[test]
    fn a_file_that_calls_itself_ends_the_walk_and_the_plan_reports_the_cycle() {
        let mut source = fake("acme/shared", false, true);
        source.files.insert(
            ("acme/shared", ".github/workflows/build.yml"),
            "on: workflow_call\njobs:\n  j:\n    uses: acme/shared/.github/workflows/build.yml@v1\n",
        );
        let config = parse(&format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [{{ up: {{ call: '{CALL}' }} }}]
"
        ))
        .unwrap();
        let library = read(&source, &config);
        assert!(plan_error(&library, &config).contains("makes a cycle"));
    }

    #[test]
    fn two_files_that_call_each_other_end_the_walk() {
        let mut source = fake("acme/shared", false, true);
        source.files.insert(
            ("acme/shared", ".github/workflows/build.yml"),
            "on: workflow_call\njobs:\n  j:\n    uses: acme/shared/.github/workflows/other.yml@v1\n",
        );
        source.files.insert(
            ("acme/shared", ".github/workflows/other.yml"),
            "on: workflow_call\njobs:\n  k:\n    uses: acme/shared/.github/workflows/build.yml@v1\n",
        );
        let config = parse(&format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  ci:
    on: {{ push: {{}} }}
    jobs: [{{ up: {{ call: '{CALL}' }} }}]
"
        ))
        .unwrap();
        let library = read(&source, &config);
        assert_eq!(remote_calls(&config, &library).len(), 0);
        assert!(plan_error(&library, &config).contains("makes a cycle"));
    }

    #[test]
    fn the_limit_counts_the_files_one_workflow_reaches() {
        let mut source = fake("acme/shared", false, true);
        source.any_file = true;
        let jobs = |from: usize, to: usize| {
            (from..to)
                .map(|i| {
                    format!("{{ c{i}: {{ call: 'acme/shared/.github/workflows/f{i}.yml@v1', with: {{ target: x }} }} }}")
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        // 12 and 12 files: 24 between two workflows, 12 for each.
        let config = parse(&format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  a:
    on: {{ push: {{}} }}
    jobs: [{}]
  b:
    on: {{ push: {{}} }}
    jobs: [{}]
",
            jobs(0, 12),
            jobs(12, 24)
        ))
        .unwrap();
        let both = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_library(&source, &config, &["a", "b"]))
            .unwrap();
        assert!(compile_with(&config, "a", "refs/heads/main", &both).is_ok());
        assert!(compile_with(&config, "b", "refs/heads/main", &both).is_ok());
        // Only the workflow asked for is read.
        let only_a = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_library(&source, &config, &["a"]))
            .unwrap();
        assert!(compile_with(&config, "a", "refs/heads/main", &only_a).is_ok());
        assert!(compile_with(&config, "b", "refs/heads/main", &only_a).is_err());
        // 25 files for one workflow: the extra one is refused.
        let config = parse(&format!(
            "
jobs: {{ t: {{ runs-on: ubuntu-latest, steps: [] }} }}
workflows:
  a:
    on: {{ push: {{}} }}
    jobs: [{}]
",
            jobs(0, 25)
        ))
        .unwrap();
        let big = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(read_library(&source, &config, &["a"]))
            .unwrap();
        let errors = compile_with(&config, "a", "refs/heads/main", &big).unwrap_err();
        assert!(
            errors
                .iter()
                .any(|d| d.message.contains("at most 20 different workflows"))
        );
    }
}
