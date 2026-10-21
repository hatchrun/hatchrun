// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The expression contexts of a running job and the functions that
//! need the machine, such as `hashFiles`.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use hatchrun_workflow::trigger::Filter;
use hatchrun_workflow::{
    Arch, Conclusion, ExprError, Functions, JobDocument, JobKind, JobStatus, checks_status,
    evaluate, truthy,
};
use nix::fcntl::OFlag;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::layout::Layout;

/// The most files one `hashFiles` call may match.
const MAX_MATCHED: usize = 100_000;

/// The most directory entries one `hashFiles` call may walk.
const MAX_WALKED: usize = 1_000_000;

/// How long one `hashFiles` call may run.
const HASH_DEADLINE: Duration = Duration::from_secs(120);

/// How many bytes `hashFiles` reads between looks at the clock and the
/// cancel signal.
const HASH_CHUNK: usize = 1 << 20;

/// The action whose step is running, for `github.action_path` and
/// `inputs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionScope {
    /// `github.action`: the action's name or id.
    pub action: String,
    /// `github.action_path`: where the action was downloaded.
    pub path: String,
    /// `github.action_ref`: the ref the workflow asked for.
    pub ref_: String,
    /// `github.action_repository`: `owner/name`.
    pub repository: String,
    /// `inputs`: the action's inputs with defaults applied.
    pub inputs: BTreeMap<String, String>,
}

/// The context object that expressions read.
#[derive(Debug, Clone)]
pub struct Contexts {
    value: Value,
    cancel: Option<watch::Receiver<u32>>,
}

impl Contexts {
    /// The contexts at the start of the job.
    pub fn new(doc: &JobDocument, layout: &Layout) -> Self {
        let g = &doc.github;
        let workspace = layout.workspace.to_string_lossy();
        let github = json!({
            "repository": g.repository,
            "repository_owner": g.repository_owner,
            "sha": g.sha,
            "ref": g.ref_,
            "ref_name": g.ref_name,
            "ref_type": g.ref_type,
            "head_ref": g.head_ref,
            "base_ref": g.base_ref,
            "actor": g.actor,
            "server_url": g.server_url,
            "api_url": g.api_url,
            "graphql_url": g.graphql_url,
            "token": g.token,
            "event": doc.event,
            "event_name": doc.event_name,
            "run_id": doc.run_id,
            "run_number": doc.run_number,
            "run_attempt": doc.run_attempt,
            "workflow": doc.workflow,
            "job": doc.job.name,
            "workspace": workspace,
            "action": "",
            "action_path": "",
            "action_ref": "",
            "action_repository": "",
        });

        let arch = match doc.job.kind {
            JobKind::Run { template } => match template.arch {
                Arch::Amd64 => "X64",
                Arch::Arm64 => "ARM64",
            },
            JobKind::Approval => "X64",
        };
        let runner_workspace = layout.workspace.parent().unwrap_or(&layout.workspace);
        let runner = json!({
            "name": "hatchrun",
            "os": "Linux",
            "arch": arch,
            "temp": layout.temp.to_string_lossy(),
            "tool_cache": layout.tool_cache.to_string_lossy(),
            "workspace": runner_workspace.to_string_lossy(),
            "debug": "",
        });

        let needs: Map<String, Value> = doc
            .needs
            .iter()
            .map(|(name, need)| {
                let entry = json!({
                    "result": conclusion(need.result),
                    "outputs": need.outputs,
                });
                (name.clone(), entry)
            })
            .collect();

        let mut secrets = doc.secrets.clone();
        secrets.insert("GITHUB_TOKEN".to_owned(), g.token.clone());

        let value = json!({
            "github": github,
            "env": {},
            "matrix": doc.job.matrix,
            "needs": needs,
            "steps": {},
            "secrets": secrets,
            "vars": doc.vars,
            // The compiler substitutes every `parameters.x`, so none is
            // left to read.
            "parameters": {},
            "runner": runner,
            "job": { "status": "success" },
            "inputs": {},
        });
        Self {
            value,
            cancel: None,
        }
    }

    /// Lets `hashFiles` stop early when the job is asked to stop.
    #[must_use]
    pub fn with_cancel(mut self, cancel: watch::Receiver<u32>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    /// The signal that tells a long expression function to give up.
    pub fn cancel(&self) -> Option<&watch::Receiver<u32>> {
        self.cancel.as_ref()
    }

    /// The object [`evaluate`] reads.
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// Replaces the `env` context.
    pub fn set_env(&mut self, env: &BTreeMap<String, String>) {
        self.value["env"] = json!(env);
    }

    /// Records a finished step under `steps.<id>`.
    pub fn record_step(
        &mut self,
        id: &str,
        outcome: Conclusion,
        conclusion_: Conclusion,
        outputs: &BTreeMap<String, String>,
    ) {
        self.value["steps"][id] = json!({
            "outcome": conclusion(outcome),
            "conclusion": conclusion(conclusion_),
            "outputs": outputs,
        });
    }

    /// Sets `job.status`.
    pub fn set_status(&mut self, status: JobStatus) {
        let text = match status {
            JobStatus::Success => "success",
            JobStatus::Failure => "failure",
            JobStatus::Cancelled => "cancelled",
        };
        self.value["job"]["status"] = json!(text);
    }

    /// The contexts a step of `action` sees. The action has its own
    /// `steps` and `inputs` and cannot read the caller's secrets.
    #[must_use]
    pub fn with_action(&self, action: ActionScope) -> Contexts {
        let mut value = self.value.clone();
        value["github"]["action"] = Value::String(action.action);
        value["github"]["action_path"] = Value::String(action.path);
        value["github"]["action_ref"] = Value::String(action.ref_);
        value["github"]["action_repository"] = Value::String(action.repository);
        value["inputs"] = json!(&action.inputs);
        value["steps"] = json!({});
        value["secrets"] = json!({});
        Contexts {
            value,
            cancel: self.cancel.clone(),
        }
    }
}

fn conclusion(c: Conclusion) -> &'static str {
    match c {
        Conclusion::Success => "success",
        Conclusion::Failure => "failure",
        Conclusion::Cancelled => "cancelled",
        Conclusion::Skipped => "skipped",
    }
}

/// What the expression functions need from the machine.
pub struct Runtime<'a> {
    /// `GITHUB_WORKSPACE`, the root `hashFiles` patterns are relative to.
    pub workspace: &'a Path,
    /// The job's status so far.
    pub status: JobStatus,
    /// Set once the job is asked to stop. `hashFiles` gives up then.
    pub cancel: Option<&'a watch::Receiver<u32>>,
}

impl Functions for Runtime<'_> {
    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError> {
        let stop = Stop {
            cancel: self.cancel,
            deadline: Instant::now() + HASH_DEADLINE,
        };
        hash_files_in(self.workspace, patterns, MAX_MATCHED, MAX_WALKED, &stop)
    }

    fn status(&self) -> JobStatus {
        self.status
    }
}

/// Whether a step's or job's `if:` holds. No condition, and one that
/// calls no status function, mean `success() && (...)`.
///
/// # Errors
///
/// When the expression does not parse or fails to evaluate.
pub fn condition(expr: Option<&str>, ctx: &Contexts, rt: &Runtime) -> Result<bool, ExprError> {
    let Some(expr) = expr else {
        return Ok(rt.status == JobStatus::Success);
    };
    let trimmed = expr.trim();
    let inner = trimmed
        .strip_prefix("${{")
        .and_then(|rest| rest.strip_suffix("}}"))
        .filter(|inner| !inner.contains("${{"))
        .map_or(trimmed, str::trim);
    let wrapped;
    let run = if checks_status(inner)? {
        inner
    } else {
        wrapped = format!("success() && ({inner})");
        &wrapped
    };
    Ok(truthy(&evaluate(run, ctx.value(), rt)?))
}

fn fail(message: impl Into<String>) -> ExprError {
    ExprError {
        message: message.into(),
        offset: 0,
    }
}

/// When a `hashFiles` call has to give up.
struct Stop<'a> {
    cancel: Option<&'a watch::Receiver<u32>>,
    deadline: Instant,
}

impl Stop<'_> {
    /// Whether the call must end now.
    fn interrupted(&self) -> impl Fn() -> bool + '_ {
        move || self.check(&[]).is_err()
    }

    /// An error that names `patterns` when the call must end.
    fn check(&self, patterns: &[String]) -> Result<(), ExprError> {
        if self.cancel.is_some_and(|c| *c.borrow() > 0) {
            Err(fail(format!(
                "hashFiles: stopped while looking for {}: the job was cancelled",
                patterns.join(", ")
            )))
        } else if Instant::now() >= self.deadline {
            Err(fail(format!(
                "hashFiles: gave up after {} seconds while looking for {}",
                HASH_DEADLINE.as_secs(),
                patterns.join(", ")
            )))
        } else {
            Ok(())
        }
    }
}

/// `hashFiles`: the SHA-256, in hex, of the SHA-256 digests of the
/// files under `workspace` that match `patterns`, in path order. A
/// file outside the workspace is never read. Symbolic links are not
/// followed into directories, and a link to a file counts only when it
/// resolves inside the workspace. `.git` directories are not entered.
/// A call that matches more than `max_matched` files or walks more than
/// `max_walked` entries fails and names its patterns, and so does one
/// that outlasts `stop`. Reads are made in 1 MiB steps and `stop` is
/// looked at between them, so a huge or growing file cannot hold the
/// job past its timeout.
fn hash_files_in(
    workspace: &Path,
    patterns: &[String],
    max_matched: usize,
    max_walked: usize,
    stop: &Stop<'_>,
) -> Result<String, ExprError> {
    let filter = Filter::new(with_root_variants(patterns));
    let root = fs::canonicalize(workspace)
        .map_err(|e| fail(format!("hashFiles: {}: {e}", workspace.display())))?;
    let mut matched: Vec<(String, PathBuf, (u64, u64))> = Vec::new();
    let mut walked = 0usize;
    let mut dirs = vec![(root.clone(), String::new())];
    while let Some((dir, rel)) = dirs.pop() {
        let entries = fs::read_dir(&dir)
            .map_err(|e| fail(format!("hashFiles: read {}: {e}", dir.display())))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| fail(format!("hashFiles: read {}: {e}", dir.display())))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            walked += 1;
            stop.check(patterns)?;
            if walked > max_walked {
                return Err(fail(format!(
                    "hashFiles: more than {max_walked} entries in the workspace while looking for {}",
                    patterns.join(", ")
                )));
            }
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|e| fail(format!("hashFiles: {}: {e}", path.display())))?;
            if kind.is_dir() && name == ".git" {
                continue;
            }
            let rel_path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if kind.is_dir() {
                dirs.push((path, rel_path));
                continue;
            }
            let (file, meta) = if kind.is_symlink() {
                match fs::canonicalize(&path) {
                    Ok(target) if target.starts_with(&root) => match fs::metadata(&target) {
                        Ok(meta) if meta.is_file() => (target, meta),
                        _ => continue,
                    },
                    _ => continue,
                }
            } else if kind.is_file() {
                match entry.metadata() {
                    Ok(meta) => (path, meta),
                    Err(_) => continue,
                }
            } else {
                continue;
            };
            if filter.matches(&rel_path) {
                if matched.len() >= max_matched {
                    return Err(fail(format!(
                        "hashFiles: more than {max_matched} files match {}",
                        patterns.join(", ")
                    )));
                }
                matched.push((rel_path, file, (meta.dev(), meta.ino())));
            }
        }
    }
    if matched.is_empty() {
        return Ok(String::new());
    }
    matched.sort_by(|a, b| a.0.cmp(&b.0));
    let mut all = Sha256::new();
    for (rel, path, id) in &matched {
        stop.check(patterns)?;
        let digest = digest_file(path, *id, &root, stop.interrupted()).map_err(|e| {
            stop.check(patterns)
                .err()
                .unwrap_or_else(|| fail(format!("hashFiles: read {rel}: {e}")))
        })?;
        if let Some(digest) = digest {
            all.update(digest);
        }
    }
    Ok(hex(&all.finalize()))
}

/// `Filter` needs a directory for `**/` to match; `@actions/glob`, which
/// `hashFiles` uses, also lets it match nothing, so `**/*.lock` finds
/// `a.lock` at the root. Each pattern that starts with `**/` is followed
/// by the same pattern without it.
fn with_root_variants(patterns: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        out.push(pattern.clone());
        let (bang, body) = pattern
            .strip_prefix('!')
            .map_or(("", pattern.as_str()), |rest| ("!", rest));
        let mut stripped = body;
        while let Some(rest) = stripped.strip_prefix("**/") {
            stripped = rest;
            out.push(format!("{bang}{stripped}"));
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Hashes `path` if it is still the file the walk saw. The file is
/// opened without following a link, and the open file must have the
/// walked file's device and inode and, where `/proc` shows it, lie under
/// `root`. A file that was swapped for something else since the walk is
/// skipped (`None`). `interrupted` is asked before each 1 MiB read.
fn digest_file(
    path: &Path,
    id: (u64, u64),
    root: &Path,
    interrupted: impl Fn() -> bool,
) -> std::io::Result<Option<[u8; 32]>> {
    let opened = fs::OpenOptions::new()
        .read(true)
        // A FIFO swapped in since the walk must not block the open.
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path);
    let mut file = match opened {
        Ok(file) => file,
        // A link now sits where the file was.
        Err(e) if e.raw_os_error() == Some(nix::libc::ELOOP) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || (meta.dev(), meta.ino()) != id {
        return Ok(None);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let real = fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        if !real.starts_with(root) {
            return Ok(None);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = root;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK];
    loop {
        if interrupted() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(hasher.finalize().into()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use hatchrun_workflow::{
        Conclusion, Github, JobDocument, JobKind, JobStatus, PlannedJob, evaluate, interpolate,
        template,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::exec::layout::Layout;

    fn job(template_label: &str) -> PlannedJob {
        PlannedJob {
            id: "build (linux) [1/2]".into(),
            name: "build".into(),
            requires: Vec::new(),
            kind: JobKind::Run {
                template: template(template_label).unwrap(),
            },
            matrix: BTreeMap::new(),
            node: None,
            env: BTreeMap::new(),
            if_: None,
            timeout_minutes: 10,
            outputs: BTreeMap::new(),
            steps: Vec::new(),
        }
    }

    fn doc(template_label: &str) -> JobDocument {
        JobDocument {
            run_id: "42".into(),
            run_number: 7,
            run_attempt: 1,
            workflow: "ci".into(),
            job: job(template_label),
            event_name: "pull_request".into(),
            event: json!({"pull_request": {"number": 12}}),
            github: Github {
                repository: "acme/repo".into(),
                repository_owner: "acme".into(),
                sha: "abc".into(),
                ref_: "refs/heads/main".into(),
                ref_name: "main".into(),
                ref_type: "branch".into(),
                head_ref: String::new(),
                base_ref: String::new(),
                actor: "octo".into(),
                server_url: "https://github.com".into(),
                api_url: "https://api.github.com".into(),
                graphql_url: "https://api.github.com/graphql".into(),
                token: "tok".into(),
            },
            needs: BTreeMap::new(),
            secrets: BTreeMap::from([("NPM".to_owned(), "s3".to_owned())]),
            vars: BTreeMap::new(),
            timings: BTreeMap::new(),
            cache: None,
        }
    }

    fn layout() -> Layout {
        Layout::new(
            Path::new("/w"),
            "acme/repo",
            Path::new("/opt/hostedtoolcache"),
        )
    }

    fn eval(ctx: &Contexts, expr: &str) -> Value {
        let rt = Runtime {
            workspace: Path::new("/nonexistent"),
            status: JobStatus::Success,
            cancel: None,
        };
        evaluate(expr, ctx.value(), &rt).unwrap()
    }

    #[test]
    fn github_context_has_the_runner_fields() {
        let ctx = Contexts::new(&doc("ubuntu-latest"), &layout());
        assert_eq!(eval(&ctx, "github.workspace"), json!("/w/repo/repo"));
        assert_eq!(eval(&ctx, "github.event.pull_request.number"), json!(12));
        assert_eq!(eval(&ctx, "github.ref"), json!("refs/heads/main"));
        assert_eq!(eval(&ctx, "github.run_id"), json!("42"));
        assert_eq!(eval(&ctx, "github.run_number"), json!(7));
        assert_eq!(eval(&ctx, "github.job"), json!("build"));
        assert_eq!(eval(&ctx, "github.event_name"), json!("pull_request"));
        assert_eq!(eval(&ctx, "secrets.GITHUB_TOKEN"), json!("tok"));
        assert_eq!(eval(&ctx, "secrets.NPM"), json!("s3"));
    }

    #[test]
    fn runner_context() {
        let ctx = Contexts::new(&doc("ubuntu-latest"), &layout());
        assert_eq!(eval(&ctx, "runner.os"), json!("Linux"));
        assert_eq!(eval(&ctx, "runner.arch"), json!("X64"));
        assert_eq!(eval(&ctx, "runner.temp"), json!("/w/_temp"));
        let arm = Contexts::new(&doc("ubuntu-26.04-arm"), &layout());
        assert_eq!(eval(&arm, "runner.arch"), json!("ARM64"));
    }

    #[test]
    fn missing_step_reads_null() {
        let ctx = Contexts::new(&doc("ubuntu-latest"), &layout());
        assert_eq!(eval(&ctx, "steps.nope.outputs.x"), Value::Null);
        let rt = Runtime {
            workspace: Path::new("/nonexistent"),
            status: JobStatus::Success,
            cancel: None,
        };
        let text = interpolate("a${{ steps.nope.outputs.x }}b", ctx.value(), &rt).unwrap();
        assert_eq!(text, "ab");
    }

    #[test]
    fn steps_env_and_action_scope() {
        let mut ctx = Contexts::new(&doc("ubuntu-latest"), &layout());
        ctx.record_step(
            "build",
            Conclusion::Failure,
            Conclusion::Success,
            &BTreeMap::from([("v".to_owned(), "1".to_owned())]),
        );
        ctx.set_env(&BTreeMap::from([("A".to_owned(), "b".to_owned())]));
        assert_eq!(eval(&ctx, "steps.build.outcome"), json!("failure"));
        assert_eq!(eval(&ctx, "steps.build.conclusion"), json!("success"));
        assert_eq!(eval(&ctx, "steps.build.outputs.v"), json!("1"));
        assert_eq!(eval(&ctx, "env.A"), json!("b"));

        let scoped = ctx.with_action(ActionScope {
            action: "setup".into(),
            path: "/w/_actions/o/r/v1".into(),
            ref_: "v1".into(),
            repository: "o/r".into(),
            inputs: BTreeMap::from([("k".to_owned(), "v".to_owned())]),
        });
        assert_eq!(
            eval(&scoped, "github.action_path"),
            json!("/w/_actions/o/r/v1")
        );
        assert_eq!(eval(&scoped, "github.action_repository"), json!("o/r"));
        assert_eq!(eval(&scoped, "inputs.k"), json!("v"));
        assert_eq!(eval(&scoped, "steps.build.outcome"), Value::Null);
        assert_eq!(eval(&ctx, "github.action_path"), json!(""));
    }

    #[test]
    fn default_condition_is_success() {
        let ctx = Contexts::new(&doc("ubuntu-latest"), &layout());
        let ws = Path::new("/nonexistent");
        let ok = Runtime {
            workspace: ws,
            status: JobStatus::Success,
            cancel: None,
        };
        let failed = Runtime {
            workspace: ws,
            status: JobStatus::Failure,
            cancel: None,
        };
        assert!(condition(None, &ctx, &ok).unwrap());
        assert!(!condition(None, &ctx, &failed).unwrap());
        assert!(!condition(Some("true"), &ctx, &failed).unwrap());
        assert!(condition(Some("true"), &ctx, &ok).unwrap());
        assert!(!condition(Some("false"), &ctx, &ok).unwrap());
        assert!(condition(Some("always()"), &ctx, &failed).unwrap());
        assert!(condition(Some("${{ failure() }}"), &ctx, &failed).unwrap());
        assert!(!condition(Some("failure()"), &ctx, &ok).unwrap());
    }

    fn free() -> Stop<'static> {
        Stop {
            cancel: None,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }

    #[test]
    fn hash_files_gives_up_at_the_deadline_and_names_the_pattern() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.lock"), vec![0u8; 3 << 20]).unwrap();
        let stop = Stop {
            cancel: None,
            deadline: Instant::now(),
        };
        let err = hash_files_in(dir.path(), &["big.lock".to_owned()], 10, 100, &stop).unwrap_err();
        assert!(
            err.message.contains("gave up") && err.message.contains("big.lock"),
            "{}",
            err.message
        );
    }

    #[test]
    fn hash_files_stops_when_the_job_is_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big.lock"), vec![0u8; 3 << 20]).unwrap();
        let (tx, rx) = watch::channel(0u32);
        let stop = Stop {
            cancel: Some(&rx),
            deadline: Instant::now() + Duration::from_secs(60),
        };
        assert!(hash_files_in(dir.path(), &["big.lock".to_owned()], 10, 100, &stop).is_ok());
        tx.send_replace(1);
        let err = hash_files_in(dir.path(), &["big.lock".to_owned()], 10, 100, &stop).unwrap_err();
        assert!(err.message.contains("cancelled"), "{}", err.message);
    }

    fn digest_of(parts: &[&str]) -> String {
        let mut all = Sha256::new();
        for p in parts {
            all.update(Sha256::digest(p.as_bytes()));
        }
        hex(&all.finalize())
    }

    fn hash(ws: &Path, patterns: &[&str]) -> String {
        let rt = Runtime {
            workspace: ws,
            status: JobStatus::Success,
            cancel: None,
        };
        let patterns: Vec<String> = patterns.iter().map(|s| (*s).to_owned()).collect();
        rt.hash_files(&patterns).unwrap()
    }

    #[test]
    fn hash_files_matches_github() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("b")).unwrap();
        std::fs::write(ws.join("a.lock"), "x").unwrap();
        std::fs::write(ws.join("b/c.lock"), "y").unwrap();
        std::fs::write(ws.join("skip.txt"), "z").unwrap();
        std::fs::write(dir.path().join("outside.lock"), "o").unwrap();

        assert_eq!(hash(&ws, &["**/*.lock"]), digest_of(&["x", "y"]));
        assert_eq!(hash(&ws, &["**/*.lock", "!b/**"]), digest_of(&["x"]));
        assert_eq!(hash(&ws, &["**/*.nothing"]), "");
        assert_eq!(hash(&ws, &["../outside.lock"]), "");
        assert_eq!(
            hash(&ws, &["**/*.lock", "../outside.lock"]),
            digest_of(&["x", "y"])
        );
    }

    #[test]
    fn hash_files_stays_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let out = dir.path().join("out");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("secret.lock"), "s").unwrap();
        std::fs::write(dir.path().join("elsewhere.lock"), "e").unwrap();
        std::os::unix::fs::symlink(&out, ws.join("link")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("elsewhere.lock"), ws.join("e.lock")).unwrap();
        assert_eq!(hash(&ws, &["**/*.lock"]), "");
    }

    #[test]
    fn hash_files_refuses_a_huge_tree() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            std::fs::write(dir.path().join(format!("f{i}")), "x").unwrap();
        }
        let err = hash_files_in(dir.path(), &["*".to_owned()], 3, 1000, &free()).unwrap_err();
        assert!(err.message.contains("more than 3 files"), "{}", err.message);
        assert!(err.message.contains('*'), "{}", err.message);
    }

    #[test]
    fn hash_files_stops_walking_after_too_many_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..10 {
            std::fs::write(dir.path().join(format!("f{i}")), "x").unwrap();
        }
        let err =
            hash_files_in(dir.path(), &["**/*.lock".to_owned()], 100, 5, &free()).unwrap_err();
        assert!(
            err.message.contains("more than 5 entries"),
            "{}",
            err.message
        );
        assert!(err.message.contains("**/*.lock"), "{}", err.message);
    }

    #[test]
    fn hash_files_does_not_look_inside_git_directories() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::create_dir_all(ws.join("sub/.git")).unwrap();
        std::fs::write(ws.join(".git/hooks/a.lock"), "g").unwrap();
        std::fs::write(ws.join("sub/.git/b.lock"), "g").unwrap();
        std::fs::write(ws.join("c.lock"), "x").unwrap();
        assert_eq!(hash(ws, &["**/*.lock"]), digest_of(&["x"]));
        // Only matches count against the file cap.
        for i in 0..5 {
            std::fs::write(ws.join(format!("other{i}")), "o").unwrap();
        }
        assert!(hash_files_in(ws, &["*.lock".to_owned()], 1, 1000, &free()).is_ok());
    }

    #[test]
    fn digest_does_not_block_on_a_fifo() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let fifo = root.join("a.lock");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(digest_file(&path, (0, 0), &root, || false));
        });
        let done = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("digest_file blocked on a FIFO");
        assert!(done.unwrap().is_none());
    }

    #[test]
    fn digest_skips_a_file_swapped_after_the_walk() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let file = root.join("a.lock");
        fs::write(&file, "x").unwrap();
        let meta = fs::metadata(&file).unwrap();
        let id = (meta.dev(), meta.ino());
        assert!(digest_file(&file, id, &root, || false).unwrap().is_some());
        fs::write(root.join("other"), "o").unwrap();
        fs::remove_file(&file).unwrap();
        std::os::unix::fs::symlink(root.join("other"), &file).unwrap();
        assert!(digest_file(&file, id, &root, || false).unwrap().is_none());
        fs::remove_file(&file).unwrap();
        fs::write(&file, "y").unwrap();
        // Same path, different inode than the walk saw.
        assert!(
            digest_file(&file, (id.0, id.1 + 1_000_000), &root, || false)
                .unwrap()
                .is_none()
        );
    }
}
