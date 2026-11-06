// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Jobs with containers, services and Docker actions, run as a binary
//! against a stand-in `docker` script that logs its calls. The script
//! answers like Docker does for the calls the agent makes; the real
//! thing is in `docker_real.rs`.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hatchrun_workflow::{
    Conclusion, Container, Credentials, JobDocument, JobResult, PlannedStep, UsesStep,
};

const FAKE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
printf '%s\n' "$*" >> "$dir/calls.log"
env >> "$dir/cli-env.log"
prev=
for a in "$@"; do
  [ "$prev" = --env-file ] && { cat "$a" >> "$dir/envfile.log"; echo "$a" >> "$dir/envfile.paths"; }
  prev=$a
done
case "$1" in
network)
  [ "$2" = inspect ] && exit 1
  echo netid ;;
login)
  cat > "$dir/login.pw"
  echo "login-stdin:$(cat "$dir/login.pw")" >> "$dir/calls.log" ;;
volume)
  [ "$2" = inspect ] && { [ -f "$dir/volume_exists" ] && exit 0; exit 1; } ;;
pull)
  [ -f "$dir/slow_pull" ] && sleep 5
  if [ -f "$dir/fail_pull" ]; then cat "$dir/login.pw" >&2; exit 1; fi ;;
create) echo cid ;;
port) echo "5432/tcp -> 0.0.0.0:49153"; echo "5432/tcp -> [::]:49153" ;;
inspect) cat "$dir/health" 2>/dev/null ;;
logs) echo "db log line" ;;
ps) ;;
exec)
  if [ "$2" = --workdir ]; then
    [ -f "$dir/slow_exec" ] && sleep 5
    echo "EXEC $*"
    [ -f "$dir/exec_code" ] && exit "$(cat "$dir/exec_code")"
  else
    case "$*" in
      *"command -v bash"*) echo yes ;;
      *PATH*) echo /usr/local/bin:/usr/bin ;;
    esac
  fi ;;
run) [ -f "$dir/slow_run" ] && sleep 5; echo "RUN $*" ;;
build) echo "BUILD $*" ;;
esac
exit 0
"#;

struct Setup {
    dir: tempfile::TempDir,
    docker: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(&docker, FAKE).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { dir, docker }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn set(&self, file: &str, text: &str) {
        std::fs::write(self.path().join("bin").join(file), text).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.path().join("bin/calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn run(&self, doc: &JobDocument) -> Output {
        let job = self.path().join("job.json");
        std::fs::write(&job, serde_json::to_string(doc).unwrap()).unwrap();
        let env = self.path().join("environment");
        std::fs::write(&env, "PATH=/usr/bin:/bin\nJAVA_HOME=/opt/jdk\n").unwrap();
        let state = self.path().canonicalize().unwrap().join("state");
        Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
            .arg("exec")
            .arg(&job)
            .arg("--work-dir")
            .arg(self.path().join("work"))
            .arg("--state-dir")
            .arg(state)
            .args(["--runner-user", ""])
            .arg("--env-file")
            .arg(&env)
            .arg("--result")
            .arg(self.path().join("result.json"))
            .args(["--minute", "30ms"])
            .arg("--docker")
            .arg(&self.docker)
            .output()
            .unwrap()
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.path().join("bin").join(file)).unwrap_or_default()
    }

    fn result(&self) -> JobResult {
        let text = std::fs::read_to_string(self.path().join("result.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn image(name: &str) -> Container {
    Container {
        image: name.to_owned(),
        ..Container::default()
    }
}

fn find<'a>(calls: &'a [String], start: &str) -> &'a str {
    calls
        .iter()
        .find(|c| c.starts_with(start))
        .unwrap_or_else(|| panic!("no `{start}` call in {calls:#?}"))
}

#[test]
fn a_job_container_runs_steps_through_exec() {
    let setup = Setup::new();
    let mut doc = make::document(vec![make::step("echo hi")]);
    doc.job.container = Some(Container {
        env: BTreeMap::from([("MODE".to_owned(), "${{ github.repository }}".to_owned())]),
        ports: vec!["8080:80".to_owned()],
        volumes: vec!["/data:/data".to_owned()],
        options: Some("--cpus 1 --user '1000:1000'".to_owned()),
        ..image("node:20")
    });
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let calls = setup.calls();

    assert!(find(&calls, "network create --label hatchrun-").contains("hatchrun_network_"));
    assert_eq!(find(&calls, "pull node:20"), "pull node:20");
    let create = find(&calls, "create --name hatchrun_job_");
    let work = setup.path().join("work");
    for want in [
        "--workdir /__w/repo/repo".to_owned(),
        "--network hatchrun_network_".to_owned(),
        "--env-file ".to_owned(),
        "-p 8080:80".to_owned(),
        "-v /data:/data".to_owned(),
        format!("-v {}:/__w", work.display()),
        "--cpus 1 --user 1000:1000".to_owned(),
        "--entrypoint tail node:20 -f /dev/null".to_owned(),
    ] {
        assert!(create.contains(&want), "{want} not in {create}");
    }
    // Values are in the docker client's environment, not its arguments.
    assert!(!create.contains("acme/repo"), "{create}");
    assert!(find(&calls, "start hatchrun_job_").starts_with("start "));

    let exec = find(&calls, "exec --workdir");
    assert!(exec.contains("--workdir /__w/repo/repo"), "{exec}");
    let files = setup.read("envfile.log");
    for want in [
        "CI=true",
        "MODE=acme/repo",
        "GITHUB_WORKSPACE=/__w/repo/repo",
        "HOME=/github/home",
    ] {
        assert!(files.lines().any(|l| l == want), "{want} not in\n{files}");
    }
    // The image's environment file and the host's user are not the container's.
    assert!(!files.contains("JAVA_HOME"), "{files}");
    assert!(!files.lines().any(|l| l.starts_with("USER=")), "{files}");
    assert!(!files.lines().any(|l| l.starts_with("PATH=")), "{files}");
    // Default shell: the container has bash.
    assert!(exec.contains(" bash -e /__w/_temp/"), "{exec}");
    assert!(
        stdout(&out).contains("EXEC exec --workdir"),
        "{}",
        stdout(&out)
    );

    // Everything is removed at the end.
    assert!(find(&calls, "rm --force --volumes").contains("hatchrun_job_"));
    assert!(find(&calls, "network rm hatchrun_network_").contains("network rm"));
    assert_eq!(setup.result().conclusion, Conclusion::Success);
}

#[test]
fn a_failing_step_in_a_container_fails_the_job_and_still_cleans_up() {
    let setup = Setup::new();
    setup.set("exec_code", "3");
    let mut doc = make::document(vec![make::step("false")]);
    doc.job.container = Some(image("alpine"));
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("Process completed with exit code 3."),
        "{}",
        stdout(&out)
    );
    assert!(setup.calls().iter().any(|c| c.starts_with("rm --force")));
}

#[test]
fn services_get_aliases_ports_and_a_health_wait() {
    let setup = Setup::new();
    setup.set("health", "healthy\n");
    let mut doc = make::document(vec![make::step("true")]);
    doc.job.services = BTreeMap::from([(
        "db".to_owned(),
        Container {
            ports: vec!["5432".to_owned()],
            options: Some("--health-cmd pg_isready".to_owned()),
            env: BTreeMap::from([("POSTGRES_PASSWORD".to_owned(), "pw".to_owned())]),
            ..image("postgres:17")
        },
    )]);
    doc.job.outputs = BTreeMap::from([(
        "port".to_owned(),
        "${{ job.services.db.ports['5432'] }}".to_owned(),
    )]);
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let calls = setup.calls();
    let create = find(&calls, "create --name hatchrun_db_");
    for want in [
        "--network-alias db",
        "--env-file ",
        "-p 5432",
        "--health-cmd pg_isready",
        "postgres:17",
    ] {
        assert!(create.contains(want), "{want} not in {create}");
    }
    assert!(!create.contains("=pw"), "{create}");
    assert!(
        setup
            .read("envfile.log")
            .lines()
            .any(|l| l == "POSTGRES_PASSWORD=pw")
    );
    // No job container: steps run on the machine, not through exec.
    assert!(!calls.iter().any(|c| c.starts_with("exec ")), "{calls:#?}");
    assert_eq!(setup.result().outputs["port"], "49153");
}

#[test]
fn an_unhealthy_service_fails_the_job_before_any_step() {
    let setup = Setup::new();
    setup.set("health", "unhealthy\n");
    let mut doc = make::document(vec![make::step("echo should-not-run")]);
    doc.job.services = BTreeMap::from([("db".to_owned(), image("postgres"))]);
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("Service db did not become healthy"), "{text}");
    assert!(text.contains("db log line"), "{text}");
    assert!(!text.lines().any(|l| l == "should-not-run"), "{text}");
    assert!(text.contains("== Stop containers"), "{text}");
    let result = setup.result();
    assert_eq!(result.conclusion, Conclusion::Failure);
    assert_eq!(result.steps[0].outcome, Conclusion::Skipped);
    assert!(setup.calls().iter().any(|c| c.starts_with("rm --force")));
}

#[test]
fn a_starting_service_is_waited_for_then_gives_up() {
    let setup = Setup::new();
    setup.set("health", "starting\n");
    let mut doc = make::document(vec![make::step("true")]);
    doc.job.services = BTreeMap::from([("db".to_owned(), image("postgres"))]);
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("Waiting for service db"),
        "{}",
        stdout(&out)
    );
    let inspects = setup
        .calls()
        .iter()
        .filter(|c| c.starts_with("inspect "))
        .count();
    assert_eq!(inspects, 6);
}

#[test]
fn registry_credentials_go_on_stdin_and_never_into_the_log() {
    let setup = Setup::new();
    setup.set("fail_pull", "");
    let mut doc = make::document(vec![make::step("true")]);
    doc.secrets
        .insert("REGISTRY".to_owned(), "s3cr3t-token".to_owned());
    doc.job.container = Some(Container {
        credentials: Some(Credentials {
            username: "bot".to_owned(),
            password: "${{ secrets.REGISTRY }}".to_owned(),
        }),
        ..image("ghcr.io/acme/build:1")
    });
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(1));
    let calls = setup.calls();
    assert_eq!(
        find(&calls, "login "),
        "login ghcr.io --username bot --password-stdin"
    );
    assert!(calls.contains(&"login-stdin:s3cr3t-token".to_owned()));
    // The failed pull printed the password; the agent masked it.
    let text = stdout(&out) + &String::from_utf8_lossy(&out.stderr);
    assert!(!text.contains("s3cr3t-token"), "{text}");
    assert!(text.contains("***"), "{text}");
    let result = std::fs::read_to_string(setup.path().join("result.json")).unwrap();
    assert!(!result.contains("s3cr3t-token"));
}

fn uses(action: &str, with: &[(&str, &str)]) -> PlannedStep {
    PlannedStep::Uses(UsesStep {
        id: Some("a".to_owned()),
        uses: action.to_owned(),
        with: with
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned().into()))
            .collect(),
        ..UsesStep::default()
    })
}

#[test]
fn uses_docker_runs_the_image_with_the_runners_mounts() {
    let setup = Setup::new();
    let doc = make::document(vec![uses(
        "docker://alpine:3.20",
        &[
            ("entrypoint", "/bin/sh"),
            ("args", "-c 'echo hi'"),
            ("who", "x"),
        ],
    )]);
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let calls = setup.calls();
    assert_eq!(find(&calls, "pull alpine:3.20"), "pull alpine:3.20");
    let run = find(&calls, "run --rm");
    for want in [
        "--workdir /github/workspace",
        "--env-file ",
        ":/github/workspace",
        ":/github/file_commands",
        ":/github/workflow",
        ":/github/runner_temp",
        "--entrypoint /bin/sh alpine:3.20 -c echo hi",
    ] {
        assert!(run.contains(want), "{want} not in {run}");
    }
    assert!(!run.contains("--network"), "{run}");
    // The host's environment file and PATH stay out of the container.
    let files = setup.read("envfile.log");
    for want in [
        "GITHUB_WORKSPACE=/github/workspace",
        "INPUT_WHO=x",
        "HOME=/github/home",
    ] {
        assert!(files.lines().any(|l| l == want), "{want} not in\n{files}");
    }
    assert!(!files.contains("JAVA_HOME"), "{files}");
    assert!(!files.lines().any(|l| l.starts_with("PATH=")), "{files}");
    assert!(stdout(&out).contains("RUN run --rm"), "{}", stdout(&out));
}

#[test]
fn a_docker_action_builds_its_dockerfile_and_runs_pre_main_and_post() {
    let setup = Setup::new();
    let act = setup.path().join("work/repo/repo/act");
    std::fs::create_dir_all(&act).unwrap();
    std::fs::write(
        act.join("action.yml"),
        "name: d
inputs:
  who:
    default: world
runs:
  using: docker
  image: Dockerfile
  entrypoint: /main.sh
  args: ['hello', '${{ inputs.who }}']
  env:
    FROM_ACTION: yes
  pre-entrypoint: /pre.sh
  post-entrypoint: /post.sh
",
    )
    .unwrap();
    std::fs::write(act.join("Dockerfile"), "FROM scratch\n").unwrap();
    let mut doc = make::document(vec![uses("./act", &[])]);
    doc.job.services = BTreeMap::from([("db".to_owned(), image("redis"))]);
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let calls = setup.calls();
    let build = find(&calls, "build --label");
    assert!(
        build.contains("--file ") && build.contains("/act/Dockerfile"),
        "{build}"
    );
    let runs: Vec<&String> = calls.iter().filter(|c| c.starts_with("run --rm")).collect();
    assert_eq!(runs.len(), 3, "{calls:#?}");
    assert!(runs[0].contains("--entrypoint /pre.sh") && !runs[0].contains("hello"));
    assert!(runs[1].contains("--entrypoint /main.sh") && runs[1].ends_with("hello world"));
    assert!(
        setup
            .read("envfile.log")
            .lines()
            .any(|l| l == "FROM_ACTION=yes")
    );
    assert!(runs[2].contains("--entrypoint /post.sh") && !runs[2].contains("hello"));
    // The job has a service, so the action joins its network.
    assert!(runs[1].contains("--network hatchrun_network_"));
    // The built image goes away with the job.
    assert!(find(&calls, "rmi --force hatchrun_action_").contains(":latest"));
}

#[test]
fn a_dockerfile_outside_the_action_is_refused() {
    let setup = Setup::new();
    let act = setup.path().join("work/repo/repo/act");
    std::fs::create_dir_all(&act).unwrap();
    std::fs::write(
        act.join("action.yml"),
        "name: d\nruns:\n  using: docker\n  image: ../Dockerfile\n",
    )
    .unwrap();
    let out = setup.run(&make::document(vec![uses("./act", &[])]));
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("must name a Dockerfile inside the action"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn a_job_without_containers_never_calls_docker() {
    let setup = Setup::new();
    let out = setup.run(&make::document(vec![make::step("echo plain")]));
    assert_eq!(out.status.code(), Some(0));
    assert!(setup.calls().is_empty(), "{:#?}", setup.calls());
}

#[test]
fn empty_credentials_skip_the_login_and_still_pull() {
    let setup = Setup::new();
    let mut doc = make::document(vec![make::step("true")]);
    // A fork's pull request has no secrets: the password reads empty.
    doc.job.container = Some(Container {
        credentials: Some(Credentials {
            username: "bot".to_owned(),
            password: "${{ secrets.MISSING }}".to_owned(),
        }),
        ..image("postgres:17")
    });
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    let calls = setup.calls();
    assert!(!calls.iter().any(|c| c.starts_with("login")), "{calls:#?}");
    assert!(calls.contains(&"pull postgres:17".to_owned()));
}

#[test]
fn a_dockerfile_may_start_with_dot_slash() {
    let setup = Setup::new();
    let act = setup.path().join("work/repo/repo/act");
    std::fs::create_dir_all(&act).unwrap();
    std::fs::write(
        act.join("action.yml"),
        "name: d\nruns:\n  using: docker\n  image: ./Dockerfile\n",
    )
    .unwrap();
    let out = setup.run(&make::document(vec![uses("./act", &[])]));
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));
    assert!(find(&setup.calls(), "build --label").contains("Dockerfile"));
}

fn with_volume(name: &str) -> Container {
    Container {
        volumes: vec![format!("{name}:/data")],
        ..image("postgres")
    }
}

#[test]
fn only_volumes_the_job_made_are_removed_and_never_forced() {
    let setup = Setup::new();
    let mut doc = make::document(vec![make::step("true")]);
    doc.job.services = BTreeMap::from([("db".to_owned(), with_volume("fresh"))]);
    assert_eq!(setup.run(&doc).status.code(), Some(0));
    assert!(
        setup.calls().contains(&"volume rm fresh".to_owned()),
        "{:#?}",
        setup.calls()
    );

    let setup = Setup::new();
    setup.set("volume_exists", "");
    doc.job.services = BTreeMap::from([("db".to_owned(), with_volume("kept"))]);
    assert_eq!(setup.run(&doc).status.code(), Some(0));
    assert!(
        !setup.calls().iter().any(|c| c.starts_with("volume rm")),
        "{:#?}",
        setup.calls()
    );
}

#[test]
fn a_timeout_during_a_pull_stops_it_and_cleans_up() {
    let setup = Setup::new();
    setup.set("slow_pull", "");
    let mut doc = make::document(vec![make::step("echo never")]);
    doc.job.timeout_minutes = 1;
    doc.job.container = Some(image("postgres"));
    let started = std::time::Instant::now();
    let out = setup.run(&doc);
    assert!(started.elapsed() < std::time::Duration::from_secs(4));
    let text = stdout(&out);
    assert!(text.contains("The operation was canceled."), "{text}");
    // A job that ran out of time is a failure; no step ran.
    assert_eq!(setup.result().conclusion, Conclusion::Failure);
    assert_eq!(setup.result().steps[0].outcome, Conclusion::Skipped);
    // A network made before the timeout is removed.
    let calls = setup.calls();
    let made = calls.iter().any(|c| c.starts_with("network create"));
    let removed = calls.iter().any(|c| c.starts_with("network rm"));
    assert_eq!(made, removed, "{calls:#?}");
}

#[test]
fn the_job_context_keeps_the_machines_paths_for_host_side_expansion() {
    let setup = Setup::new();
    let mut doc = make::document(vec![make::step("true")]);
    doc.job.container = Some(image("alpine"));
    doc.job.outputs = BTreeMap::from([("ws".to_owned(), "${{ github.workspace }}".to_owned())]);
    assert_eq!(setup.run(&doc).status.code(), Some(0));
    let ws = setup.result().outputs["ws"].clone();
    assert!(
        ws.ends_with("work/repo/repo") && !ws.starts_with("/__w"),
        "{ws}"
    );
}

#[test]
fn workflow_values_reach_the_container_by_env_file_never_the_docker_client() {
    let setup = Setup::new();
    let mut step = make::run("true");
    step.env = BTreeMap::from([
        ("LD_PRELOAD".to_owned(), "/tmp/evil.so".to_owned()),
        ("SSL_CERT_FILE".to_owned(), "/tmp/ca".to_owned()),
        ("MULTI".to_owned(), "line1\nline2".to_owned()),
    ]);
    let mut doc = make::document(vec![PlannedStep::Run(step)]);
    doc.job.container = Some(Container {
        env: BTreeMap::from([("SVC_SECRET".to_owned(), "topsecret".to_owned())]),
        ..image("node:20")
    });
    doc.job.services = BTreeMap::from([(
        "db".to_owned(),
        Container {
            env: BTreeMap::from([("HTTPS_PROXY".to_owned(), "http://p".to_owned())]),
            ..image("postgres")
        },
    )]);
    let out = setup.run(&doc);
    assert_eq!(out.status.code(), Some(0), "{}", stdout(&out));

    // The docker client's environment never held a workflow value.
    let cli = setup.read("cli-env.log");
    for name in [
        "LD_PRELOAD",
        "SSL_CERT_FILE",
        "MULTI",
        "SVC_SECRET",
        "HTTPS_PROXY",
    ] {
        assert!(
            !cli.contains(name),
            "{name} reached the docker client:\n{cli}"
        );
    }
    // They went in env files, which are gone afterwards.
    let files = setup.read("envfile.log");
    for want in [
        "LD_PRELOAD=/tmp/evil.so",
        "SSL_CERT_FILE=/tmp/ca",
        "SVC_SECRET=topsecret",
        "HTTPS_PROXY=http://p",
    ] {
        assert!(files.lines().any(|l| l == want), "{want} not in\n{files}");
    }
    assert!(!files.contains("MULTI"), "{files}");
    for path in setup.read("envfile.paths").lines() {
        assert!(!Path::new(path).exists(), "{path} was left behind");
    }
    // A value an env file cannot carry is a flag, and no secret is on a
    // create line.
    let calls = setup.calls().join("\n");
    assert!(calls.contains("-e MULTI=line1"), "{calls}");
    assert!(
        !calls.contains("topsecret") && !calls.contains("evil.so"),
        "{calls}"
    );
}

#[test]
fn a_step_timeout_covers_docker_actions_and_steps_in_the_container() {
    let slow = std::time::Duration::from_secs(4);

    let setup = Setup::new();
    setup.set("slow_run", "");
    let mut step = UsesStep {
        id: Some("a".to_owned()),
        uses: "docker://alpine".to_owned(),
        ..UsesStep::default()
    };
    step.timeout_minutes = Some(1);
    let started = std::time::Instant::now();
    let out = setup.run(&make::document(vec![PlannedStep::Uses(step)]));
    assert!(started.elapsed() < slow);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("timed out after 1 minutes"),
        "{}",
        stdout(&out)
    );

    let setup = Setup::new();
    setup.set("slow_exec", "");
    let mut run = make::run("sleep 100");
    run.timeout_minutes = Some(1);
    let mut doc = make::document(vec![PlannedStep::Run(run)]);
    doc.job.container = Some(image("alpine"));
    let started = std::time::Instant::now();
    let out = setup.run(&doc);
    assert!(started.elapsed() < slow);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("timed out after 1 minutes"),
        "{}",
        stdout(&out)
    );
}
