// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The real `actions/cache` toolkit against the agent's cache proxy and a
//! stand-in for the Worker's `/v1/agent/cache` service, in two jobs: one
//! saves, a later one restores. They run only when
//! `HATCHRUN_ACTIONS_CACHE_DIR` names a checkout of `actions/cache` that
//! has its `dist/restore/index.js` and `dist/save/index.js` (v4 or
//! later), and `node` is on `PATH`.

#[path = "fixtures/jobs/make.rs"]
mod make;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use hatchrun_workflow::{CacheService, PlannedStep};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The Worker's cache table and S3, in memory.
#[derive(Default)]
struct Store {
    /// Started uploads by id.
    uploads: HashMap<String, Pending>,
    /// Finalized entries: (version, key) to (object id, bytes).
    entries: HashMap<(String, String), (String, Vec<u8>)>,
    next: u32,
}

/// An upload: its version, key and the parts that arrived, by number.
type Pending = (String, String, HashMap<u16, Vec<u8>>);

type Shared = Arc<Mutex<Store>>;

fn lock(store: &Shared) -> std::sync::MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[allow(clippy::too_many_lines)]
async fn worker() -> (MockServer, Shared) {
    let server = MockServer::start().await;
    let store = Shared::default();
    let uri = server.uri();

    let st = store.clone();
    let origin = uri.clone();
    Mock::given(method("POST"))
        .and(path("/v1/agent/cache/lookup"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let version = body["version"].as_str().unwrap().to_owned();
            let mut keys = vec![body["key"].as_str().unwrap().to_owned()];
            keys.extend(
                body["restoreKeys"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|k| k.as_str().unwrap().to_owned()),
            );
            let store = lock(&st);
            for (at, key) in keys.iter().enumerate() {
                // The first key is exact, the rest are prefixes.
                let found = if at == 0 {
                    store
                        .entries
                        .get(&(version.clone(), key.clone()))
                        .map(|(id, bytes)| (key.clone(), id, bytes))
                } else {
                    store
                        .entries
                        .iter()
                        .filter(|((v, k), _)| *v == version && k.starts_with(key.as_str()))
                        .map(|((_, k), (id, bytes))| (k.clone(), id, bytes))
                        .next()
                };
                if let Some((key, id, bytes)) = found {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "key": key,
                        "url": format!("{origin}/s3/object/{id}?X-Amz-Signature=x"),
                        "size": bytes.len(),
                    }));
                }
            }
            ResponseTemplate::new(404).set_body_json(json!({ "error": "no entry" }))
        })
        .mount(&server)
        .await;

    let st = store.clone();
    Mock::given(method("POST"))
        .and(path("/v1/agent/cache/uploads"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let version = body["version"].as_str().unwrap().to_owned();
            let key = body["key"].as_str().unwrap().to_owned();
            let mut store = lock(&st);
            if store.entries.contains_key(&(version.clone(), key.clone())) {
                return ResponseTemplate::new(409).set_body_json(json!({ "error": "exists" }));
            }
            store.next += 1;
            let id = store.next.to_string();
            store
                .uploads
                .insert(id.clone(), (version, key, HashMap::new()));
            ResponseTemplate::new(201).set_body_json(json!({ "uploadId": id }))
        })
        .mount(&server)
        .await;

    let origin = uri.clone();
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/agent/cache/uploads/[^/]+/parts$"))
        .respond_with(move |req: &wiremock::Request| {
            let id = req.url.path().split('/').nth_back(1).unwrap().to_owned();
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let parts: Vec<Value> = body["numbers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| json!({ "number": n, "url": format!("{origin}/s3/part/{id}/{n}") }))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "parts": parts }))
        })
        .mount(&server)
        .await;

    let st = store.clone();
    Mock::given(method("PUT"))
        .and(path_regex(r"^/s3/part/[^/]+/\d+$"))
        .respond_with(move |req: &wiremock::Request| {
            let mut segments = req.url.path().rsplit('/');
            let number: u16 = segments.next().unwrap().parse().unwrap();
            let id = segments.next().unwrap();
            lock(&st)
                .uploads
                .get_mut(id)
                .unwrap()
                .2
                .insert(number, req.body.clone());
            ResponseTemplate::new(200).insert_header("ETag", format!("\"e{number}\""))
        })
        .mount(&server)
        .await;

    let st = store.clone();
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/agent/cache/uploads/[^/]+/complete$"))
        .respond_with(move |req: &wiremock::Request| {
            let id = req.url.path().split('/').nth_back(1).unwrap().to_owned();
            let mut store = lock(&st);
            let Some((version, key, parts)) = store.uploads.remove(&id) else {
                return ResponseTemplate::new(404);
            };
            let mut numbers: Vec<u16> = parts.keys().copied().collect();
            numbers.sort_unstable();
            let bytes: Vec<u8> = numbers.iter().flat_map(|n| parts[n].clone()).collect();
            store.entries.insert((version, key), (id, bytes));
            ResponseTemplate::new(204)
        })
        .mount(&server)
        .await;

    let st = store.clone();
    Mock::given(method("GET"))
        .and(path_regex(r"^/s3/object/[^/]+$"))
        .respond_with(move |req: &wiremock::Request| {
            let id = req.url.path().rsplit('/').next().unwrap();
            let store = lock(&st);
            let bytes = store
                .entries
                .values()
                .find(|(object, _)| object == id)
                .map(|(_, bytes)| bytes.clone())
                .unwrap();
            let Some(range) = req.headers.get("range") else {
                return ResponseTemplate::new(200).set_body_bytes(bytes);
            };
            let (start, end) = range
                .to_str()
                .unwrap()
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap_or(bytes.len() - 1).min(bytes.len() - 1);
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!("bytes {start}-{end}/{}", bytes.len()),
                )
                .set_body_bytes(bytes[start..=end].to_vec())
        })
        .mount(&server)
        .await;
    (server, store)
}

fn toolkit() -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("HATCHRUN_ACTIONS_CACHE_DIR")?);
    let node = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|d| d.join("node").is_file()));
    node.then_some(dir)
}

/// A local action that is `actions/cache`: restore as the main script,
/// save as its post.
fn install(work: &Path, toolkit: &Path) {
    let act = work.join("work/repo/repo/cache");
    for name in ["restore", "save"] {
        let dist = act.join("dist").join(name);
        std::fs::create_dir_all(&dist).unwrap();
        std::fs::copy(
            toolkit.join("dist").join(name).join("index.js"),
            dist.join("index.js"),
        )
        .unwrap();
    }
    std::fs::write(
        act.join("action.yml"),
        "name: cache\ninputs:\n  path:\n    required: true\n  key:\n    required: true\n  restore-keys:\n    required: false\n  enableCrossOsArchive:\n    default: 'false'\n  fail-on-cache-miss:\n    default: 'false'\n  lookup-only:\n    default: 'false'\noutputs:\n  cache-hit:\n    description: x\nruns:\n  using: node20\n  main: dist/restore/index.js\n  post: dist/save/index.js\n  post-if: success()\n",
    )
    .unwrap();
}

fn job(server: &MockServer, run_attempt: u32, steps: Vec<PlannedStep>) -> String {
    let mut doc = make::document(steps);
    doc.run_attempt = run_attempt;
    doc.cache = Some(CacheService {
        url: format!("{}/v1/agent/cache", server.uri()),
        token: "job-token".to_owned(),
        scopes: vec!["refs/heads/main".to_owned()],
    });
    serde_json::to_string(&doc).unwrap()
}

fn exec(dir: &Path, doc: &str) -> Output {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let job = dir.join("job.json");
    std::fs::write(&job, doc).unwrap();
    let env = dir.join("environment");
    let path = std::env::var("PATH").unwrap();
    std::fs::write(&env, format!("PATH={path}\n")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_hatchrun-agent"))
        .arg("exec")
        .arg(&job)
        .arg("--work-dir")
        .arg(dir.join("work"))
        .arg("--state-dir")
        .arg(dir.canonicalize().unwrap().join("state"))
        .args(["--runner-user", ""])
        .arg("--env-file")
        .arg(&env)
        .arg("--result")
        .arg(dir.join("result.json"))
        .output()
        .unwrap()
}

fn cache_step(id: &str) -> PlannedStep {
    let mut step = make::uses(id, "./cache");
    step.with.insert("path".to_owned(), "smoke-cache".into());
    step.with.insert("key".to_owned(), "smoke-1-1".into());
    PlannedStep::Uses(step)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// What the smoke test does: one job saves with `actions/cache`, a later
/// one, on another machine, restores the same key.
#[tokio::test(flavor = "multi_thread")]
async fn a_saved_entry_restores_in_a_later_job() {
    let Some(toolkit) = toolkit() else {
        eprintln!(
            "skipping: set HATCHRUN_ACTIONS_CACHE_DIR to an actions/cache checkout and have node"
        );
        return;
    };
    let (server, store) = worker().await;

    let save = job(
        &server,
        1,
        vec![
            make::step("mkdir -p smoke-cache && echo stamp-42 > smoke-cache/stamp"),
            cache_step("c"),
        ],
    );
    let restore = job(
        &server,
        1,
        vec![
            cache_step("c"),
            make::step("echo hit=${{ steps.c.outputs.cache-hit }}; cat smoke-cache/stamp"),
        ],
    );
    let toolkit2 = toolkit.clone();
    let (first, second) = tokio::task::spawn_blocking(move || {
        let one = tempfile::tempdir().unwrap();
        install(one.path(), &toolkit);
        let first = exec(one.path(), &save);
        let two = tempfile::tempdir().unwrap();
        install(two.path(), &toolkit2);
        let second = exec(two.path(), &restore);
        (stdout(&first), stdout(&second))
    })
    .await
    .unwrap();

    assert_eq!(lock(&store).entries.len(), 1, "save:\n{first}");
    assert!(
        second.contains("hit=true"),
        "restore:\n{second}\nsave:\n{first}"
    );
    assert!(second.contains("stamp-42"), "{second}");
}

/// `BuildKit`'s `type=gha` backend, as `docker buildx build` runs it, against
/// the proxy: one job exports a layer, a later one imports it and gets the
/// same marker. Needs `HATCHRUN_DOCKER_TESTS=1`, a Docker with buildx and
/// the image `HATCHRUN_DOCKER_IMAGE` (default `redis:8-alpine`).
#[tokio::test(flavor = "multi_thread")]
async fn buildx_gha_cache_round_trips() {
    if std::env::var("HATCHRUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping: set HATCHRUN_DOCKER_TESTS=1");
        return;
    }
    let image = std::env::var("HATCHRUN_DOCKER_IMAGE").unwrap_or_else(|_| "redis:8-alpine".into());
    let (server, store) = worker().await;
    let build = |cache: &str, builder: &str| {
        format!(
            "set -e\n\
             mkdir -p ctx\n\
             printf 'FROM {image} AS m\\nRUN cat /proc/sys/kernel/random/uuid > /marker\\nFROM scratch\\nCOPY --from=m /marker /marker\\n' > ctx/Dockerfile\n\
             docker buildx create --name {builder} --driver docker-container --driver-opt network=host >/dev/null\n\
             trap 'docker buildx rm -f {builder} >/dev/null 2>&1 || true' EXIT\n\
             docker buildx build --builder {builder} {cache} --output type=local,dest=out ctx\n\
             echo marker=$(cat out/marker)\n"
        )
    };
    let export = job(
        &server,
        1,
        vec![make::step(&build(
            "--cache-to type=gha,mode=max,scope=smoke",
            "hatchrun-cache-test-1",
        ))],
    );
    let import = job(
        &server,
        1,
        vec![make::step(&build(
            "--cache-from type=gha,scope=smoke",
            "hatchrun-cache-test-2",
        ))],
    );
    let (first, second) = tokio::task::spawn_blocking(move || {
        let one = tempfile::tempdir().unwrap();
        let first = exec(one.path(), &export);
        let two = tempfile::tempdir().unwrap();
        let second = exec(two.path(), &import);
        (stdout(&first), stdout(&second))
    })
    .await
    .unwrap();

    let marker = |out: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix("marker=").map(str::to_owned))
            .unwrap_or_default()
    };
    assert!(!lock(&store).entries.is_empty(), "export:\n{first}");
    assert!(!marker(&first).is_empty(), "export:\n{first}");
    assert_eq!(marker(&first), marker(&second), "import:\n{second}");
}
