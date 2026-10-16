// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Starts the GitHub Actions runner for one job.

use std::future::Future;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use tokio::process::Command;
use tracing::{info, warn};

use crate::envfile;

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The runner runs this script before "Initialize containers" in every
/// job and applies what it writes to `$GITHUB_ENV`.
const JOB_STARTED_HOOK: &str = "ACTIONS_RUNNER_HOOK_JOB_STARTED";

/// The job-started hook the agent writes, relative to the runner directory.
const JOB_STARTED_SCRIPT: &str = "hatchrun-job-started.sh";

/// Runs `run.sh` for exactly one job.
pub struct Runner {
    /// Directory holding `run.sh`, for example `/home/runner`.
    pub dir: PathBuf,
    /// User the runner runs as. `None` keeps the agent's own user.
    pub user: Option<String>,
    /// Read like `/etc/environment` and passed to the runner as its whole
    /// environment, so jobs see the same `PATH` and tool variables as on
    /// GitHub hosted runners.
    pub env_file: PathBuf,
    /// How long the runner gets to exit after SIGTERM before it is killed.
    pub stop_timeout: Duration,
    /// Variables every step of the job gets, such as the cache proxy's.
    /// They are set on top of the environment file, and a job-started hook
    /// writes them to `$GITHUB_ENV`: container jobs and Docker actions see
    /// only the step environment, never the runner's own.
    pub env: Vec<(String, String)>,
}

impl Runner {
    /// Runs the runner until it exits or `shutdown` resolves.
    ///
    /// The config goes in `ACTIONS_RUNNER_INPUT_JITCONFIG` instead of the
    /// `--jitconfig` argument: any process on the instance can read argv,
    /// while the runner masks the variable and removes it before job steps
    /// start.
    pub async fn run(
        &self,
        jit_config: &str,
        shutdown: impl Future<Output = ()>,
    ) -> Result<ExitStatus> {
        let mut extra = self.env.clone();
        if !self.env.is_empty()
            && let Some(hook) = self.write_job_started_hook()?
        {
            extra.push((JOB_STARTED_HOOK.into(), hook.to_string_lossy().into_owned()));
        }
        let mut command = Command::new(self.dir.join("run.sh"));
        command
            .current_dir(&self.dir)
            .env_clear()
            .envs(self.environment(&extra)?)
            .env("ACTIONS_RUNNER_INPUT_JITCONFIG", jit_config)
            .stdin(Stdio::null());
        if let Some(user) = &self.user {
            run_as(&mut command, user)?;
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("start {}", self.dir.join("run.sh").display()))?;
        tokio::select! {
            status = child.wait() => status.context("wait for runner"),
            () = shutdown => {
                info!("stopping runner");
                if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok())
                    && let Err(err) = kill(Pid::from_raw(pid), Signal::SIGTERM)
                {
                    warn!("signal runner: {err}");
                }
                if let Ok(status) = tokio::time::timeout(self.stop_timeout, child.wait()).await {
                    return status.context("wait for runner");
                }
                warn!("runner did not exit after {:?}, killing it", self.stop_timeout);
                child.kill().await.context("kill runner")?;
                child.wait().await.context("wait for runner")
            }
        }
    }

    /// Writes a script that appends `env` to `$GITHUB_ENV`. Any old file
    /// is removed first so the write cannot follow a planted symlink.
    ///
    /// A job-started hook the environment file already sets runs first,
    /// started the way the runner would start it, so its failure still
    /// fails the job and our variables win over any it writes. A hook the
    /// agent cannot start that way is left in place without ours, and
    /// container jobs then miss the proxy.
    fn write_job_started_hook(&self) -> Result<Option<PathBuf>> {
        use std::fmt::Write as _;
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;

        let path = self.dir.join(JOB_STARTED_SCRIPT);
        let mut script = String::from("#!/bin/sh\n");
        if let Some(existing) = envfile::read(&self.env_file)?
            .into_iter()
            .rev()
            .find(|(key, value)| key == JOB_STARTED_HOOK && !value.is_empty())
            .map(|(_, value)| value)
        {
            // The runner runs .sh hooks with bash and .ps1 hooks with pwsh,
            // with these arguments (ScriptHandlerHelpers.cs).
            let start = match std::path::Path::new(&existing)
                .extension()
                .and_then(|ext| ext.to_str())
            {
                Some("sh") => "bash --noprofile --norc -e -o pipefail",
                Some("ps1") => "pwsh -command",
                _ => {
                    warn!(
                        hook = existing,
                        "keeping the job-started hook, which is not a .sh or .ps1 script; \
                         container jobs do not get the job cache"
                    );
                    return Ok(None);
                }
            };
            let target = if start.starts_with("pwsh") {
                sh_quote(&format!(". '{}'", existing.replace('\'', "''")))
            } else {
                sh_quote(&existing)
            };
            writeln!(script, "{start} {target}")?;
        }
        for (key, value) in &self.env {
            let line = sh_quote(&format!("{key}={value}"));
            writeln!(script, "printf '%s\\n' {line} >> \"$GITHUB_ENV\"")?;
        }
        match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("remove {}", path.display()));
            }
            _ => {}
        }
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&path)
            .and_then(|mut file| file.write_all(script.as_bytes()))
            .with_context(|| format!("write {}", path.display()))?;
        Ok(Some(path))
    }

    fn environment(&self, extra: &[(String, String)]) -> Result<Vec<(String, String)>> {
        let mut env = envfile::read(&self.env_file)?;
        if !env.iter().any(|(key, _)| key == "PATH") {
            env.push(("PATH".into(), DEFAULT_PATH.into()));
        }
        let (name, home) = match &self.user {
            Some(name) => (name.clone(), home_of(name)?),
            None => (
                std::env::var("USER").unwrap_or_default(),
                std::env::var("HOME").unwrap_or_default(),
            ),
        };
        env.extend([
            ("HOME".into(), home),
            ("USER".into(), name.clone()),
            ("LOGNAME".into(), name),
        ]);
        env.retain(|(key, _)| !extra.iter().any(|(ours, _)| ours == key));
        env.extend(extra.iter().cloned());
        Ok(env)
    }
}

/// Quotes `value` for sh.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn home_of(name: &str) -> Result<String> {
    let user = nix::unistd::User::from_name(name)
        .with_context(|| format!("look up user {name}"))?
        .with_context(|| format!("user {name} does not exist"))?;
    Ok(user.dir.to_string_lossy().into_owned())
}

/// Switches the child to `name` with all its supplementary groups. std's
/// `uid()` drops supplementary groups, which would take away `docker`.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn run_as(command: &mut Command, name: &str) -> Result<()> {
    use std::ffi::CString;

    use nix::unistd::{User, getgrouplist, setgid, setgroups, setuid};

    let user = User::from_name(name)
        .with_context(|| format!("look up user {name}"))?
        .with_context(|| format!("user {name} does not exist"))?;
    let groups = getgrouplist(&CString::new(name)?, user.gid)
        .with_context(|| format!("list groups of {name}"))?;
    let (uid, gid) = (user.uid, user.gid);

    // SAFETY: the closure runs in the forked child before exec. It only
    // calls setgroups, setgid and setuid, which are async-signal-safe, and
    // does not allocate.
    unsafe {
        command.pre_exec(move || {
            setgroups(&groups)?;
            setgid(gid)?;
            setuid(uid)?;
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn run_as(_: &mut Command, _: &str) -> Result<()> {
    anyhow::bail!("running the runner as another user is only supported on Linux")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    /// A fake run.sh that records its environment and exits with `code`, or
    /// waits for SIGTERM when `code` is "wait".
    fn runner_dir(code: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        let script = format!(
            "#!/bin/sh\nenv > \"$(dirname \"$0\")/env.out\"\n\
             if [ {code} = wait ]; then trap 'echo stopped > \"$(dirname \"$0\")/signal.out\"; exit 143' TERM; \
             touch \"$(dirname \"$0\")/ready\"; while :; do sleep 0.1; done; fi\nexit {code}\n"
        );
        let path = dir.path().join("run.sh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            dir.path().join("environment"),
            "ImageOS=ubuntu26\nPATH=/usr/bin:/bin\n",
        )
        .unwrap();
        dir
    }

    fn runner(dir: &Path) -> Runner {
        Runner {
            dir: dir.to_path_buf(),
            user: None,
            env_file: dir.join("environment"),
            stop_timeout: Duration::from_secs(5),
            env: Vec::new(),
        }
    }

    fn recorded_env(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("env.out")).unwrap()
    }

    #[tokio::test]
    async fn passes_config_and_environment() {
        let dir = runner_dir("0");
        let status = runner(dir.path())
            .run("encoded-jit", std::future::pending())
            .await
            .unwrap();

        assert!(status.success());
        let env = recorded_env(dir.path());
        assert!(
            env.contains("ACTIONS_RUNNER_INPUT_JITCONFIG=encoded-jit"),
            "{env}"
        );
        assert!(env.contains("ImageOS=ubuntu26"), "{env}");
        assert!(env.contains("PATH=/usr/bin:/bin"), "{env}");
    }

    #[tokio::test]
    async fn extra_variables_win_over_the_env_file() {
        let dir = runner_dir("0");
        std::fs::write(
            dir.path().join("environment"),
            "ACTIONS_RESULTS_URL=http://elsewhere/\nPATH=/usr/bin:/bin\n",
        )
        .unwrap();
        let runner = Runner {
            env: vec![
                ("ACTIONS_RESULTS_URL".into(), "http://10.0.0.5:7419/".into()),
                ("ACTIONS_CACHE_SERVICE_V2".into(), "true".into()),
            ],
            ..runner(dir.path())
        };
        runner.run("jit", std::future::pending()).await.unwrap();

        let env = recorded_env(dir.path());
        assert!(
            env.contains("ACTIONS_RESULTS_URL=http://10.0.0.5:7419/\n"),
            "{env}"
        );
        assert!(!env.contains("elsewhere"), "{env}");
        assert!(env.contains("ACTIONS_CACHE_SERVICE_V2=true"), "{env}");
    }

    #[tokio::test]
    async fn job_started_hook_writes_extra_variables_to_github_env() {
        let dir = runner_dir("0");
        let runner = Runner {
            env: vec![
                ("ACTIONS_RESULTS_URL".into(), "http://10.0.0.5:7419/".into()),
                ("ACTIONS_CACHE_SERVICE_V2".into(), "true".into()),
                ("QUOTED".into(), "it's".into()),
            ],
            ..runner(dir.path())
        };
        runner.run("jit", std::future::pending()).await.unwrap();

        let hook = dir.path().join(JOB_STARTED_SCRIPT);
        let env = recorded_env(dir.path());
        assert!(
            env.contains(&format!("{JOB_STARTED_HOOK}={}\n", hook.display())),
            "{env}"
        );
        let github_env = dir.path().join("github_env");
        std::fs::write(&github_env, "EARLIER=1\n").unwrap();
        let status = std::process::Command::new("bash")
            .arg("-e")
            .arg(&hook)
            .env("GITHUB_ENV", &github_env)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&github_env).unwrap(),
            "EARLIER=1\n\
             ACTIONS_RESULTS_URL=http://10.0.0.5:7419/\n\
             ACTIONS_CACHE_SERVICE_V2=true\n\
             QUOTED=it's\n"
        );
    }

    #[tokio::test]
    async fn job_started_hook_replaces_an_old_file() {
        let dir = runner_dir("0");
        let hook = dir.path().join(JOB_STARTED_SCRIPT);
        let target = dir.path().join("target");
        std::fs::write(&target, "keep").unwrap();
        std::os::unix::fs::symlink(&target, &hook).unwrap();
        let runner = Runner {
            env: vec![("ACTIONS_CACHE_SERVICE_V2".into(), "true".into())],
            ..runner(dir.path())
        };
        runner.run("jit", std::future::pending()).await.unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        assert!(!std::fs::symlink_metadata(&hook).unwrap().is_symlink());
        assert_eq!(
            std::fs::metadata(&hook).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[tokio::test]
    async fn job_started_hook_runs_an_existing_hook_first() {
        let dir = runner_dir("0");
        let existing = dir.path().join("customer-hook.sh");
        std::fs::write(&existing, "echo ACTIONS_CACHE_SERVICE_V2=false >> \"$GITHUB_ENV\"\necho CUSTOMER=1 >> \"$GITHUB_ENV\"\n").unwrap();
        std::fs::write(
            dir.path().join("environment"),
            format!(
                "{JOB_STARTED_HOOK}={}\nPATH=/usr/bin:/bin\n",
                existing.display()
            ),
        )
        .unwrap();
        let runner = Runner {
            env: vec![("ACTIONS_CACHE_SERVICE_V2".into(), "true".into())],
            ..runner(dir.path())
        };
        runner.run("jit", std::future::pending()).await.unwrap();

        let hook = dir.path().join(JOB_STARTED_SCRIPT);
        let env = recorded_env(dir.path());
        assert!(
            env.contains(&format!("{JOB_STARTED_HOOK}={}\n", hook.display())),
            "{env}"
        );
        let github_env = dir.path().join("github_env");
        let status = std::process::Command::new("bash")
            .arg("-e")
            .arg(&hook)
            .env("GITHUB_ENV", &github_env)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&github_env).unwrap(),
            "ACTIONS_CACHE_SERVICE_V2=false\nCUSTOMER=1\nACTIONS_CACHE_SERVICE_V2=true\n"
        );
    }

    #[tokio::test]
    async fn job_started_hook_fails_when_the_existing_hook_fails() {
        let dir = runner_dir("0");
        let existing = dir.path().join("fails.sh");
        std::fs::write(&existing, "exit 7\n").unwrap();
        std::fs::write(
            dir.path().join("environment"),
            format!("{JOB_STARTED_HOOK}={}\n", existing.display()),
        )
        .unwrap();
        let runner = Runner {
            env: vec![("ACTIONS_CACHE_SERVICE_V2".into(), "true".into())],
            ..runner(dir.path())
        };
        runner.run("jit", std::future::pending()).await.unwrap();

        let github_env = dir.path().join("github_env");
        let status = std::process::Command::new("bash")
            .arg("-e")
            .arg(dir.path().join(JOB_STARTED_SCRIPT))
            .env("GITHUB_ENV", &github_env)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(7));
        assert!(!github_env.exists());
    }

    #[tokio::test]
    async fn job_started_hook_keeps_a_hook_it_cannot_start() {
        let dir = runner_dir("0");
        std::fs::write(
            dir.path().join("environment"),
            format!("{JOB_STARTED_HOOK}=/opt/hooks/started.js\n"),
        )
        .unwrap();
        let runner = Runner {
            env: vec![("ACTIONS_CACHE_SERVICE_V2".into(), "true".into())],
            ..runner(dir.path())
        };
        runner.run("jit", std::future::pending()).await.unwrap();

        let env = recorded_env(dir.path());
        assert!(
            env.contains(&format!("{JOB_STARTED_HOOK}=/opt/hooks/started.js\n")),
            "{env}"
        );
        assert!(env.contains("ACTIONS_CACHE_SERVICE_V2=true"), "{env}");
        assert!(!dir.path().join(JOB_STARTED_SCRIPT).exists());
    }

    #[tokio::test]
    async fn no_job_started_hook_without_extra_variables() {
        let dir = runner_dir("0");
        runner(dir.path())
            .run("jit", std::future::pending())
            .await
            .unwrap();

        assert!(!recorded_env(dir.path()).contains(JOB_STARTED_HOOK));
        assert!(!dir.path().join(JOB_STARTED_SCRIPT).exists());
    }

    #[tokio::test]
    async fn reports_runner_exit_code() {
        let dir = runner_dir("3");
        let status = runner(dir.path())
            .run("jit", std::future::pending())
            .await
            .unwrap();
        assert_eq!(status.code(), Some(3));
    }

    #[tokio::test]
    async fn default_path_when_env_file_missing() {
        let dir = runner_dir("0");
        std::fs::remove_file(dir.path().join("environment")).unwrap();
        runner(dir.path())
            .run("jit", std::future::pending())
            .await
            .unwrap();
        assert!(recorded_env(dir.path()).contains(&format!("PATH={DEFAULT_PATH}")));
    }

    #[tokio::test]
    async fn shutdown_sends_sigterm() {
        let dir = runner_dir("wait");
        let ready = dir.path().join("ready");
        // Signal only once the script has installed its trap.
        let shutdown = async move {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let status = runner(dir.path()).run("jit", shutdown).await.unwrap();

        assert_eq!(status.code(), Some(143));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("signal.out")).unwrap(),
            "stopped\n"
        );
    }

    /// Needs root and a `hatchrun-test` user that belongs to a
    /// `hatchrun-extra` supplementary group. CI creates both in a container.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "needs root and the hatchrun-test user"]
    async fn runs_as_user_with_supplementary_groups() {
        let dir = runner_dir("0");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(
            dir.path().join("run.sh"),
            "#!/bin/sh\nid -un > /tmp/hatchrun-test-id.out\nid -Gn >> /tmp/hatchrun-test-id.out\n",
        )
        .unwrap();
        let runner = Runner {
            user: Some("hatchrun-test".into()),
            ..runner(dir.path())
        };

        assert!(
            runner
                .run("jit", std::future::pending())
                .await
                .unwrap()
                .success()
        );
        let id = std::fs::read_to_string("/tmp/hatchrun-test-id.out").unwrap();
        let mut lines = id.lines();
        assert_eq!(lines.next(), Some("hatchrun-test"));
        assert!(
            lines
                .next()
                .unwrap()
                .split(' ')
                .any(|group| group == "hatchrun-extra"),
            "{id}"
        );
    }
}
