// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use super::*;
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn github(api_url: &str) -> Github {
    Github {
        repository: "o/r".into(),
        repository_owner: "o".into(),
        sha: "0".into(),
        ref_: "refs/heads/main".into(),
        ref_name: "main".into(),
        ref_type: "branch".into(),
        head_ref: String::new(),
        base_ref: String::new(),
        actor: "a".into(),
        server_url: "https://github.com".into(),
        api_url: api_url.into(),
        graphql_url: String::new(),
        token: "tok".into(),
    }
}

#[test]
fn chown_tree_walks_without_following_links() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    fs::create_dir_all(tree.join("a/b")).unwrap();
    fs::write(tree.join("a/b/f"), "x").unwrap();
    // A link that points nowhere fails if it is followed.
    std::os::unix::fs::symlink("/nonexistent/target", tree.join("dangling")).unwrap();
    std::os::unix::fs::symlink(dir.path(), tree.join("a/up")).unwrap();
    let me = (nix::unistd::getuid(), nix::unistd::getgid());
    chown_tree(&open_dir(&nix::fcntl::AT_FDCWD, &tree).unwrap(), me).unwrap();
    // Still the same tree, links intact.
    assert!(
        fs::symlink_metadata(tree.join("dangling"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(fs::read_to_string(tree.join("a/b/f")).unwrap(), "x");
}

fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, body) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(body.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, body.as_bytes()).unwrap();
    }
    let raw = b.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&raw).unwrap();
    gz.finish().unwrap()
}

fn remote(owner: &str, repo: &str, path: &str, r: &str) -> ActionRef {
    ActionRef::Remote {
        owner: owner.into(),
        repo: repo.into(),
        path: path.into(),
        git_ref: r.into(),
    }
}

/// A temp dir closed to group and others whatever the caller's
/// umask, which the stage checks in `ActionCache::open` require.
fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn cache(layout: &Layout) -> ActionCache {
    // The stage path must hold no links, which macOS temp dirs do.
    let state = layout.work.canonicalize().unwrap().join("state");
    ActionCache::open(&layout.actions, &state).unwrap()
}

fn fast() -> Limits {
    Limits {
        backoff: Duration::from_millis(1),
        ..LIMITS
    }
}

const NODE_YML: &str = "name: n\nruns:\n  using: node20\n  main: dist/index.js\n";

#[test]
fn parses_refs() {
    assert_eq!(
        parse_ref("actions/checkout@v5").unwrap(),
        remote("actions", "checkout", "", "v5")
    );
    assert_eq!(
        parse_ref("github/codeql-action/init@v3").unwrap(),
        remote("github", "codeql-action", "init", "v3")
    );
    assert_eq!(
        parse_ref("./.github/act").unwrap(),
        ActionRef::Local(PathBuf::from("./.github/act"))
    );
    let err = parse_ref("actions/checkout").unwrap_err();
    assert!(err.contains("actions/checkout"), "{err}");
    assert!(parse_ref("a/b@../x").is_err());
    assert!(parse_ref("a/b/../c@v1").is_err());
    assert!(parse_ref("a b/c@v1").is_err());
    assert!(parse_ref("lonely@v1").is_err());
    assert!(
        parse_ref("docker://alpine:3")
            .unwrap_err()
            .contains("Docker container action")
    );
}

#[tokio::test]
async fn downloads_and_caches() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/tarball/v1"))
        .and(header("authorization", "Bearer tok"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball(&[
            ("o-r-abc/action.yml", NODE_YML),
            ("o-r-abc/dist/index.js", "x"),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let gh = github(&server.uri());
    let r = remote("o", "r", "", "v1");

    let a = fetch_with(&r, &gh, &layout, &cache(&layout), fast())
        .await
        .unwrap();
    assert_eq!(a.dir, layout.actions.join("o/r/v1/tree"));
    assert!(a.dir.join("dist/index.js").is_file());
    assert!(matches!(a.meta.runs, Runs::Node { .. }));

    // The second fetch hits the cache (the mock expects one request).
    let b = fetch_with(&r, &gh, &layout, &cache(&layout), fast())
        .await
        .unwrap();
    assert_eq!(a, b);
    // No temp leftovers beside the cache entry.
    let names: Vec<_> = fs::read_dir(layout.actions.join("o/r"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("v1")]);
}

#[tokio::test]
async fn fetches_a_subdirectory_action_and_retries_5xx() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/tarball/v2"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/tarball/v2"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(tarball(&[("o-r-abc/init/action.yml", NODE_YML)])),
        )
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let a = fetch_with(
        &remote("o", "r", "init", "v2"),
        &github(&server.uri()),
        &layout,
        &cache(&layout),
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(a.dir, layout.actions.join("o/r/v2/tree/init"));
}

#[tokio::test]
async fn slashed_refs_get_one_component_and_stale_entries_are_replaced() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/o/r/tarball/feat/x"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tarball(&[
            ("o-r-abc/action.yml", NODE_YML),
            ("o-r-abc/complete", "shipped"),
        ])))
        .expect(1)
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    // A crashed run left a tree but no marker.
    let stale = layout.actions.join("o/r/feat%2Fx");
    fs::create_dir_all(stale.join("tree")).unwrap();
    fs::write(stale.join("tree/old"), "old").unwrap();
    let a = fetch_with(
        &remote("o", "r", "", "feat/x"),
        &github(&server.uri()),
        &layout,
        &cache(&layout),
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(a.dir, stale.join("tree"));
    assert!(!stale.join("tree/old").exists());
    // The archive's own `complete` file is inside the tree only.
    assert!(stale.join("tree/complete").is_file());
    assert!(stale.join("complete").is_file());
    assert_eq!(cache_key("a/b%"), "a%2Fb%25");
}

#[tokio::test]
async fn a_shipped_marker_does_not_make_a_failed_fetch_cached() {
    let mut b = tar::Builder::new(Vec::new());
    let mut h = tar::Header::new_gnu();
    h.set_size(7);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_data(&mut h, "o-r-abc/complete", &b"shipped"[..])
        .unwrap();
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(tar::EntryType::Link);
    h.set_size(0);
    h.set_cksum();
    b.append_link(&mut h, "o-r-abc/hard", "/etc/passwd")
        .unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&b.into_inner().unwrap()).unwrap();
    let body = gz.finish().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let r = remote("o", "r", "", "bad");
    assert!(
        fetch_with(&r, &github(&server.uri()), &layout, &cache(&layout), fast())
            .await
            .is_err()
    );
    assert!(!layout.actions.join("o/r/bad").exists());
    let leftovers: Vec<_> = fs::read_dir(layout.actions.join("o/r")).unwrap().collect();
    assert!(leftovers.is_empty(), "temp dir cleaned up");
}

#[tokio::test]
async fn a_404_fails_without_caching() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let err = fetch_with(
        &remote("o", "r", "", "nope"),
        &github(&server.uri()),
        &layout,
        &cache(&layout),
        fast(),
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("404"), "{err:#}");
    assert!(!layout.actions.join("o/r/nope").exists());
}

#[tokio::test]
async fn stops_at_the_compressed_cap() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1u8; 4096]))
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let limits = Limits {
        compressed: 100,
        ..fast()
    };
    let err = fetch_with(
        &remote("o", "r", "", "big"),
        &github(&server.uri()),
        &layout,
        &cache(&layout),
        limits,
    )
    .await
    .unwrap_err();
    assert!(format!("{err:#}").contains("100 byte limit"), "{err:#}");
    assert!(!layout.actions.join("o/r/big").exists());
}

async fn planted_link_fetch(link_at: &str) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tarball(&[("o-r-abc/action.yml", NODE_YML)])),
        )
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), "keep").unwrap();
    fs::create_dir_all(layout.actions.join(link_at).parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(outside.path(), layout.actions.join(link_at)).unwrap();
    let a = fetch_with(
        &remote("o", "r", "", "v1"),
        &github(&server.uri()),
        &layout,
        &cache(&layout),
        fast(),
    )
    .await;
    // The fetch may fail or replace the link, but nothing lands behind it.
    let names: Vec<_> = fs::read_dir(outside.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, vec![std::ffi::OsString::from("keep")], "{a:?}");
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"keep");
    assert!(a.is_ok(), "{a:?}");
    assert!(
        !fs::symlink_metadata(layout.actions.join(link_at))
            .unwrap()
            .is_symlink()
    );
}

#[tokio::test]
async fn a_swapped_cache_directory_does_not_redirect_a_fetch() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_bytes(tarball(&[("o-r-abc/action.yml", NODE_YML)])),
        )
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let held = cache(&layout);
    // A step moves the cache away and puts a link in its place.
    let outside = tempfile::tempdir().unwrap();
    let moved = work.path().join("moved");
    fs::rename(&layout.actions, &moved).unwrap();
    std::os::unix::fs::symlink(outside.path(), &layout.actions).unwrap();
    fetch_with(
        &remote("o", "r", "", "v1"),
        &github(&server.uri()),
        &layout,
        &held,
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    assert!(moved.join("o/r/v1/tree/action.yml").is_file());
    // Nothing is left in the stage.
    assert_eq!(
        fs::read_dir(work.path().join("state/stage"))
            .unwrap()
            .count(),
        0
    );
}

#[tokio::test]
async fn a_dropped_fetch_cleans_the_stage() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(tarball(&[("o-r-abc/action.yml", NODE_YML)]))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let held = cache(&layout);
    let gh = github(&server.uri());
    let r = remote("o", "r", "", "v1");
    let fetch = fetch_with(&r, &gh, &layout, &held, fast());
    assert!(
        tokio::time::timeout(Duration::from_millis(300), fetch)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_dir(work.path().join("state/stage"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn copy_tree_copies_by_descriptor_and_keeps_links() {
    let t = tempfile::tempdir().unwrap();
    let (a, b) = (t.path().join("a"), t.path().join("b"));
    fs::create_dir_all(a.join("d")).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("d/f"), "x").unwrap();
    fs::set_permissions(a.join("d/f"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("/nonexistent", a.join("link")).unwrap();
    let open = |p: &Path| open_dir(&nix::fcntl::AT_FDCWD, p).unwrap();
    copy_tree(&open(&a), &open(&b)).unwrap();
    assert_eq!(fs::read_to_string(b.join("d/f")).unwrap(), "x");
    assert_eq!(
        fs::metadata(b.join("d/f")).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_link(b.join("link")).unwrap(),
        Path::new("/nonexistent")
    );
}

#[tokio::test]
async fn a_planted_owner_link_is_not_followed() {
    planted_link_fetch("o").await;
}

#[tokio::test]
async fn a_planted_repo_link_is_not_followed() {
    planted_link_fetch("o/r").await;
}

#[tokio::test]
async fn a_planted_key_link_is_not_followed() {
    planted_link_fetch("o/r/v1").await;
}

#[tokio::test]
async fn local_actions_stay_inside_the_workspace() {
    let work = private_tempdir();
    let layout = Layout::new(work.path(), "o/r", Path::new("/tc"));
    let act = layout.workspace.join(".github/act");
    fs::create_dir_all(&act).unwrap();
    fs::write(act.join("action.yml"), NODE_YML).unwrap();
    let gh = github("http://unused");
    let a = fetch_with(
        &ActionRef::Local("./.github/act".into()),
        &gh,
        &layout,
        &cache(&layout),
        fast(),
    )
    .await
    .unwrap();
    assert_eq!(a.dir, act.canonicalize().unwrap());
    let err = fetch_with(
        &ActionRef::Local("./../../..".into()),
        &gh,
        &layout,
        &cache(&layout),
        fast(),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("outside the workspace"), "{err}");
}

#[test]
fn reads_action_yaml_or_yml() {
    for name in ["action.yml", "action.yaml"] {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(name), NODE_YML).unwrap();
        let m = read_metadata(d.path()).unwrap();
        assert_eq!(m.name, "n");
        assert_eq!(
            m.runs,
            Runs::Node {
                version: "node20".into(),
                main: "dist/index.js".into(),
                pre: None,
                pre_if: None,
                post: None,
                post_if: None,
            }
        );
    }
    let d = tempfile::tempdir().unwrap();
    let err = read_metadata(d.path()).unwrap_err().to_string();
    assert!(
        err.contains("action.yml") && err.contains("action.yaml"),
        "{err}"
    );
}

#[test]
fn a_linked_or_fifo_action_file_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let elsewhere = d.path().join("elsewhere.yml");
    fs::write(&elsewhere, NODE_YML).unwrap();
    let act = d.path().join("act");
    fs::create_dir(&act).unwrap();
    std::os::unix::fs::symlink(&elsewhere, act.join("action.yml")).unwrap();
    let err = read_metadata(&act).unwrap_err().to_string();
    assert!(err.contains("symbolic link"), "{err}");

    let fifo = d.path().join("fifo");
    fs::create_dir(&fifo).unwrap();
    nix::unistd::mkfifo(
        &fifo.join("action.yml"),
        nix::sys::stat::Mode::from_bits_truncate(0o600),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_metadata(&fifo).map(|_| ()).map_err(|e| e.to_string()));
    });
    let done = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("reading a FIFO must not block");
    assert!(done.unwrap_err().contains("not a regular file"));
}

#[test]
fn a_linked_directory_on_the_way_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let real = d.path().join("real");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("action.yml"), NODE_YML).unwrap();
    let root = d.path().join("root");
    fs::create_dir(&root).unwrap();
    std::os::unix::fs::symlink(&real, root.join("sub")).unwrap();
    assert!(read_metadata_in(&root, Path::new("sub")).is_err());
    fs::create_dir(root.join("plain")).unwrap();
    fs::write(root.join("plain/action.yml"), NODE_YML).unwrap();
    assert!(read_metadata_in(&root, Path::new("plain")).is_ok());
    assert!(read_metadata_in(&root, Path::new("../real")).is_err());
}

#[test]
fn caps_the_action_file() {
    let d = tempfile::tempdir().unwrap();
    let mut big = NODE_YML.to_string();
    big.push_str(&"#".repeat(2 << 20));
    fs::write(d.path().join("action.yml"), big).unwrap();
    let err = read_metadata(d.path()).unwrap_err().to_string();
    assert!(err.contains("1 MiB"), "{err}");
}

fn parse(yml: &str) -> Result<Metadata> {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("action.yml"), yml).unwrap();
    read_metadata(d.path())
}

#[test]
fn docker_action_is_runs_docker() {
    let m = parse("name: d\nruns:\n  using: docker\n  image: Dockerfile\n").unwrap();
    assert_eq!(m.runs, Runs::Docker);
}

#[test]
fn maps_every_node_version_and_rejects_others() {
    for v in ["node12", "node16", "node20", "node24"] {
        let m = parse(&format!("name: n\nruns:\n  using: {v}\n  main: m.js\n  pre: p.js\n  pre-if: always()\n  post: q.js\n  post-if: failure()\n")).unwrap();
        assert_eq!(
            m.runs,
            Runs::Node {
                version: v.into(),
                main: "m.js".into(),
                pre: Some("p.js".into()),
                pre_if: Some("always()".into()),
                post: Some("q.js".into()),
                post_if: Some("failure()".into()),
            }
        );
    }
    let err = parse("name: n\nruns:\n  using: python3\n  main: m.py\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("python3"), "{err}");
}

#[test]
fn keeps_inputs_outputs_and_composite_steps_raw() {
    let m = parse(
        r"
name: c
inputs:
  a:
    description: first
    required: true
  b:
    default: ${{ github.token }}
  n:
    default: 5
  t:
    default: true
outputs:
  o:
    description: out
    value: ${{ steps.s.outputs.x }}
runs:
  using: composite
  steps:
    - id: s
      name: say
      if: ${{ inputs.a == 'x' }}
      run: echo hi
      shell: bash
      working-directory: sub
      continue-on-error: true
      env:
        K: v
        N: 3
    - uses: actions/checkout@v5
      with:
        fetch-depth: 0
",
    )
    .unwrap();
    assert!(m.inputs["a"].required);
    assert_eq!(m.inputs["a"].description, "first");
    assert_eq!(
        m.inputs["b"].default.as_deref(),
        Some("${{ github.token }}")
    );
    assert_eq!(m.inputs["n"].default.as_deref(), Some("5"));
    assert_eq!(m.inputs["t"].default.as_deref(), Some("true"));
    assert_eq!(
        m.outputs["o"].value.as_deref(),
        Some("${{ steps.s.outputs.x }}")
    );
    let Runs::Composite { steps } = m.runs else {
        panic!("composite");
    };
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].id.as_deref(), Some("s"));
    assert_eq!(steps[0].if_.as_deref(), Some("${{ inputs.a == 'x' }}"));
    assert_eq!(steps[0].run.as_deref(), Some("echo hi"));
    assert_eq!(steps[0].shell.as_deref(), Some("bash"));
    assert_eq!(steps[0].working_directory.as_deref(), Some("sub"));
    assert_eq!(steps[0].continue_on_error.as_deref(), Some("true"));
    assert_eq!(steps[0].env["N"], "3");
    assert_eq!(steps[1].uses.as_deref(), Some("actions/checkout@v5"));
    assert_eq!(steps[1].with["fetch-depth"], "0");
}
