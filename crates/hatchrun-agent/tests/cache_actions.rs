// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The real cache clients against the agent's cache proxy and a stand-in
//! for the Worker's `/v1/agent/cache` service, each in two jobs: one
//! saves, a later one restores.
//!
//! - `actions/cache` runs when `HATCHRUN_ACTIONS_CACHE_DIR` names a
//!   directory with its `dist/restore/index.js` and
//!   `dist/save/index.js` (v4 or later) and `node` is on `PATH`.
//! - `docker buildx build` with `type=gha` runs when
//!   `HATCHRUN_DOCKER_TESTS=1`.
//!
//! The stand-in scopes entries as the Worker does: a job writes to the
//! first of its scopes and reads from all of them, in order.

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

const MAIN: &str = "refs/heads/main";
const PULL: &str = "refs/pull/5/merge";

/// An entry: scope, version and key.
type EntryKey = (String, String, String);

/// An upload: its entry and the parts that arrived, by number.
type Pending = (EntryKey, HashMap<u16, Vec<u8>>);

/// The Worker's cache table and S3, in memory.
#[derive(Default)]
struct Store {
    /// The scopes of each job token, write scope first.
    jobs: HashMap<String, Vec<String>>,
    /// Started uploads by id.
    uploads: HashMap<String, Pending>,
    /// Finalized entries to (object id, bytes).
    entries: HashMap<EntryKey, (String, Vec<u8>)>,
    next: u32,
}

type Shared = Arc<Mutex<Store>>;

fn lock(store: &Shared) -> std::sync::MutexGuard<'_, Store> {
    store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The scopes of the job whose token the request carries.
fn scopes_of(store: &Store, req: &wiremock::Request) -> Vec<String> {
    let token = req
        .headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or_default();
    store.jobs.get(token).cloned().unwrap_or_default()
}

impl Store {
    /// The entry for `key`, else the first of `restore_keys` that is a
    /// prefix of one, in the first of `scopes` that has any.
    fn find(
        &self,
        scopes: &[String],
        version: &str,
        key: &str,
        restore_keys: &[String],
    ) -> Option<(String, &String, &Vec<u8>)> {
        for scope in scopes {
            let exact = (scope.clone(), version.to_owned(), key.to_owned());
            if let Some((id, bytes)) = self.entries.get(&exact) {
                return Some((key.to_owned(), id, bytes));
            }
            for prefix in restore_keys {
                let hit = self
                    .entries
                    .iter()
                    .find(|((s, v, k), _)| s == scope && v == version && k.starts_with(prefix));
                if let Some(((_, _, k), (id, bytes))) = hit {
                    return Some((k.clone(), id, bytes));
                }
            }
        }
        None
    }
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
            let version = body["version"].as_str().unwrap();
            let key = body["key"].as_str().unwrap();
            let restore: Vec<String> = body["restoreKeys"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|k| k.as_str().unwrap().to_owned())
                .collect();
            let store = lock(&st);
            let scopes = scopes_of(&store, req);
            match store.find(&scopes, version, key, &restore) {
                Some((key, id, bytes)) => ResponseTemplate::new(200).set_body_json(json!({
                    "key": key,
                    "url": format!("{origin}/s3/object/{id}?X-Amz-Signature=x"),
                    "size": bytes.len(),
                })),
                None => ResponseTemplate::new(404).set_body_json(json!({ "error": "no entry" })),
            }
        })
        .mount(&server)
        .await;

    let st = store.clone();
    Mock::given(method("POST"))
        .and(path("/v1/agent/cache/uploads"))
        .respond_with(move |req: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            let mut store = lock(&st);
            let scope = scopes_of(&store, req).remove(0);
            let entry = (
                scope,
                body["version"].as_str().unwrap().to_owned(),
                body["key"].as_str().unwrap().to_owned(),
            );
            if store.entries.contains_key(&entry) {
                return ResponseTemplate::new(409).set_body_json(json!({ "error": "exists" }));
            }
            store.next += 1;
            let id = store.next.to_string();
            store.uploads.insert(id.clone(), (entry, HashMap::new()));
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
                .1
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
            let Some((entry, parts)) = store.uploads.remove(&id) else {
                return ResponseTemplate::new(404);
            };
            let mut numbers: Vec<u16> = parts.keys().copied().collect();
            numbers.sort_unstable();
            let bytes: Vec<u8> = numbers.iter().flat_map(|n| parts[n].clone()).collect();
            store.entries.insert(entry, (id, bytes));
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

/// A job document for a job that saves to `scopes[0]` and reads all of
/// `scopes`. Its token names the scopes to the stand-in.
fn job(server: &MockServer, store: &Shared, scopes: &[&str], steps: Vec<PlannedStep>) -> String {
    let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
    let mut doc = make::document(steps);
    let token = format!("job-token-{}", lock(store).jobs.len());
    lock(store).jobs.insert(token.clone(), scopes.clone());
    doc.github.ref_.clone_from(&scopes[0]);
    doc.cache = Some(CacheService {
        url: format!("{}/v1/agent/cache", server.uri()),
        token,
        scopes,
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

/// The proxy listens on a fixed port, so one test runs jobs at a time.
static PROXY_PORT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Runs the documents in order, each in a fresh work directory.
async fn run_jobs(docs: Vec<String>, toolkit: Option<PathBuf>) -> Vec<String> {
    let _port = PROXY_PORT.lock().await;
    tokio::task::spawn_blocking(move || {
        docs.iter()
            .map(|doc| {
                let dir = tempfile::tempdir().unwrap();
                if let Some(toolkit) = &toolkit {
                    install(dir.path(), toolkit);
                }
                stdout(&exec(dir.path(), doc))
            })
            .collect()
    })
    .await
    .unwrap()
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
        &store,
        &[MAIN],
        vec![
            make::step("mkdir -p smoke-cache && echo stamp-42 > smoke-cache/stamp"),
            cache_step("c"),
        ],
    );
    let restore = job(
        &server,
        &store,
        &[MAIN],
        vec![
            cache_step("c"),
            make::step("echo hit=${{ steps.c.outputs.cache-hit }}; cat smoke-cache/stamp"),
        ],
    );
    let out = run_jobs(vec![save, restore], Some(toolkit)).await;

    assert_eq!(lock(&store).entries.len(), 1, "save:\n{}", out[0]);
    assert!(out[1].contains("hit=true"), "restore:\n{}", out[1]);
    assert!(out[1].contains("stamp-42"), "{}", out[1]);
}

/// Builders made so far, for unique names.
static BUILDERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// A shell script that builds a Dockerfile whose layer holds a random
/// marker with `cache`, in a builder of its own, and prints the marker.
/// The builder goes away on exit, and its name is unique to this run.
fn build_script(cache: &str, builder: &str) -> String {
    let image = std::env::var("HATCHRUN_DOCKER_IMAGE").unwrap_or_else(|_| "redis:8-alpine".into());
    let n = BUILDERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let builder = format!("{builder}-{}-{n}", std::process::id());
    format!(
        "set -e\n\
         mkdir -p ctx\n\
         printf 'FROM {image} AS m\\nRUN cat /proc/sys/kernel/random/uuid > /marker\\nFROM scratch\\nCOPY --from=m /marker /marker\\n' > ctx/Dockerfile\n\
         trap 'docker buildx rm -f {builder} >/dev/null 2>&1 || true' EXIT\n\
         docker buildx rm -f {builder} >/dev/null 2>&1 || true\n\
         docker buildx create --name {builder} --driver docker-container --driver-opt network=host >/dev/null\n\
         docker buildx build --builder {builder} {cache} --output type=local,dest=out ctx\n\
         echo marker=$(cat out/marker)\n"
    )
}

fn marker(out: &str) -> String {
    out.lines()
        .find_map(|l| l.strip_prefix("marker=").map(str::to_owned))
        .unwrap_or_default()
}

fn docker_tests() -> bool {
    let on = std::env::var("HATCHRUN_DOCKER_TESTS").as_deref() == Ok("1");
    if !on {
        eprintln!("skipping: set HATCHRUN_DOCKER_TESTS=1");
    }
    on
}

/// Exports with a job of scopes `export_scopes` and imports with a job
/// of `import_scopes`. Returns the markers each build printed and the
/// scopes the exported entries went to.
async fn buildx_round(
    export_scopes: &[&str],
    import_scopes: &[&str],
) -> (String, String, Vec<String>) {
    let (server, store) = worker().await;
    let export = job(
        &server,
        &store,
        export_scopes,
        vec![make::step(&build_script(
            "--cache-to type=gha,mode=max,scope=smoke",
            "hatchrun-cache-test-export",
        ))],
    );
    let import = job(
        &server,
        &store,
        import_scopes,
        vec![make::step(&build_script(
            "--cache-from type=gha,scope=smoke",
            "hatchrun-cache-test-import",
        ))],
    );
    let out = run_jobs(vec![export, import], None).await;
    let mut written: Vec<String> = lock(&store)
        .entries
        .keys()
        .map(|(scope, _, _)| scope.clone())
        .collect();
    written.sort();
    written.dedup();
    assert!(!marker(&out[0]).is_empty(), "export:\n{}", out[0]);
    (marker(&out[0]), marker(&out[1]), written)
}

/// `BuildKit`'s `type=gha` backend, as `docker buildx build` runs it, against
/// the proxy: one job exports a layer, a later one imports it and gets the
/// same marker. Needs a Docker with buildx and the image
/// `HATCHRUN_DOCKER_IMAGE` (default `redis:8-alpine`).
#[tokio::test(flavor = "multi_thread")]
async fn buildx_gha_cache_round_trips() {
    if !docker_tests() {
        return;
    }
    let (first, second, written) = buildx_round(&[MAIN], &[MAIN]).await;
    assert_eq!(first, second);
    assert_eq!(written, [MAIN]);
}

/// A pull request writes to its own scope only, so the default branch
/// does not see what it exported.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_request_export_is_not_read_on_the_default_branch() {
    if !docker_tests() {
        return;
    }
    let (first, second, written) = buildx_round(&[PULL, MAIN], &[MAIN]).await;
    assert_eq!(written, [PULL], "the write went to the first scope only");
    assert_ne!(first, second, "main imported a pull request's layer");
}

/// A pull request reads the default branch's entries.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_request_imports_what_the_default_branch_exported() {
    if !docker_tests() {
        return;
    }
    let (first, second, written) = buildx_round(&[MAIN], &[PULL, MAIN]).await;
    assert_eq!(written, [MAIN]);
    assert_eq!(first, second, "the pull request missed main's layer");
}
