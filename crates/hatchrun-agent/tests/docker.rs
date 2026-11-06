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
case "$1" in
network)
  [ "$2" = inspect ] && exit 1
  echo netid ;;
login)
  cat > "$dir/login.pw"
  echo "login-stdin:$(cat "$dir/login.pw")" >> "$dir/calls.log" ;;
pull)
  if [ -f "$dir/fail_pull" ]; then cat "$dir/login.pw" >&2; exit 1; fi ;;
create) echo cid ;;
port) echo "5432/tcp -> 0.0.0.0:49153"; echo "5432/tcp -> [::]:49153" ;;
inspect) cat "$dir/health" 2>/dev/null ;;
logs) echo "db log line" ;;
ps) ;;
exec)
  if [ "$2" = --workdir ]; then
    echo "EXEC $*"
    [ -f "$dir/exec_code" ] && exit "$(cat "$dir/exec_code")"
  else
    case "$*" in
      *"command -v bash"*) echo yes ;;
      *PATH*) echo /usr/local/bin:/usr/bin ;;
    esac
  fi ;;
run) echo "RUN $*" ;;
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
        "-e CI=true".to_owned(),
        "-e MODE=acme/repo".to_owned(),
        "-p 8080:80".to_owned(),
        "-v /data:/data".to_owned(),
        format!("-v {}:/__w", work.display()),
        "--cpus 1 --user 1000:1000".to_owned(),
        "--entrypoint tail node:20 -f /dev/null".to_owned(),
    ] {
        assert!(create.contains(&want), "{want} not in {create}");
    }
    assert!(find(&calls, "start hatchrun_job_").starts_with("start "));

    let exec = find(&calls, "exec --workdir");
    assert!(exec.contains("--workdir /__w/repo/repo"), "{exec}");
    assert!(exec.contains("-e GITHUB_WORKSPACE"), "{exec}");
    assert!(exec.contains("-e HOME=/github/home"), "{exec}");
    // The image's environment file and the host's user are not the container's.
    assert!(!exec.contains("JAVA_HOME"), "{exec}");
    assert!(!exec.contains("-e USER"), "{exec}");
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
        "-e POSTGRES_PASSWORD=pw",
        "-p 5432",
        "--health-cmd pg_isready",
        "postgres:17",
    ] {
        assert!(create.contains(want), "{want} not in {create}");
    }
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
        "-e GITHUB_WORKSPACE",
        "-e INPUT_WHO",
        "-e HOME=/github/home",
        ":/github/workspace",
        ":/github/file_commands",
        ":/github/workflow",
        ":/github/runner_temp",
        "--entrypoint /bin/sh alpine:3.20 -c echo hi",
    ] {
        assert!(run.contains(want), "{want} not in {run}");
    }
    assert!(!run.contains("--network"), "{run}");
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
    assert!(runs[1].contains("-e FROM_ACTION"));
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
