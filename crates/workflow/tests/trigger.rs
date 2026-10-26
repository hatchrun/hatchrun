// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Trigger matching with GitHub filter rules.

use hatchrun_workflow::{Event, Filter, Triggers, triggered};

fn f(patterns: &[&str]) -> Filter {
    Filter::new(patterns.iter().map(|p| (*p).to_owned()).collect())
}

fn on(yaml: &str) -> Triggers {
    serde_saphyr::from_str(yaml).unwrap()
}

fn push(r: &str, changed: &[&str]) -> Event {
    Event::Push {
        r#ref: r.into(),
        changed: changed.iter().map(|c| (*c).to_owned()).collect(),
    }
}

fn pr(action: &str, base: &str, changed: &[&str]) -> Event {
    Event::PullRequest {
        action: action.into(),
        base_ref: base.into(),
        changed: changed.iter().map(|c| (*c).to_owned()).collect(),
    }
}

#[test]
fn star_does_not_cross_slash() {
    let g = f(&["feature/*"]);
    assert!(g.matches("feature/a") && !g.matches("feature/a/b"));
    assert!(f(&["feature/**"]).matches("feature/a/b"));
}

#[test]
fn negation_order_matters() {
    let g = f(&["releases/**", "!releases/**-alpha"]);
    assert!(g.matches("releases/1.0") && !g.matches("releases/1.0-alpha"));
    let h = f(&["!releases/**-alpha", "releases/**"]);
    assert!(h.matches("releases/1.0-alpha"));
}

#[test]
fn only_negative_patterns_match_nothing() {
    assert!(!f(&["!main"]).matches("dev"));
}

#[test]
fn question_plus_and_classes() {
    let g = f(&["v1.?"]);
    assert!(g.matches("v1") && g.matches("v1.") && !g.matches("v1.x"));
    let col = f(&["colou?r"]);
    assert!(col.matches("color") && col.matches("colour") && !col.matches("colouur"));
    assert!(f(&["?a"]).matches("?a") && f(&["a*?"]).matches("a?"));
    let h = f(&["v1+.0"]);
    assert!(h.matches("v1.0") && h.matches("v111.0") && !h.matches("v.0"));
    let c = f(&["v[0-9]", "!v[3-4]"]);
    assert!(c.matches("v2") && !c.matches("v3") && !c.matches("vx"));
    let n = f(&["[!a]x"]);
    assert!(n.matches("bx") && !n.matches("ax"));
    assert!(f(&["a.b"]).matches("a.b") && !f(&["a.b"]).matches("axb"));
    assert!(f(&["a\\*"]).matches("a*") && !f(&["a\\*"]).matches("ab"));
}

#[test]
fn star_edge_cases() {
    assert!(f(&["*"]).matches("main") && !f(&["*"]).matches("a/b"));
    assert!(f(&["**"]).matches("a/b/c"));
    assert!(f(&["*.md"]).matches("README.md") && !f(&["*.md"]).matches("docs/a.md"));
    assert!(f(&["**/*.md"]).matches("docs/a/b.md"));
}

#[test]
fn push_branch_tag_and_paths() {
    let t = on("push: { branches: [main], paths: ['src/**'] }");
    assert!(triggered(&t, &push("refs/heads/main", &["src/a.rs"])));
    assert!(!triggered(&t, &push("refs/heads/main", &["README.md"])));
    assert!(!triggered(&t, &push("refs/tags/v1", &["src/a.rs"])));
}

#[test]
fn push_without_ref_filters_matches_branch_and_tag() {
    let t = on("push:");
    assert!(triggered(&t, &push("refs/heads/x", &[])));
    assert!(triggered(&t, &push("refs/tags/v1", &[])));
    assert!(!triggered(&t, &push("refs/pull/1/merge", &[])));
}

#[test]
fn push_tag_filters() {
    let t = on("push: { tags: ['v*'] }");
    assert!(triggered(&t, &push("refs/tags/v1", &[])));
    assert!(!triggered(&t, &push("refs/tags/x1", &[])));
    assert!(!triggered(&t, &push("refs/heads/v1", &[])));
    let i = on("push: { tags-ignore: ['v*'] }");
    assert!(!triggered(&i, &push("refs/tags/v1", &[])));
    assert!(triggered(&i, &push("refs/tags/x1", &[])));
    assert!(!triggered(&i, &push("refs/heads/main", &[])));
}

#[test]
fn push_branches_ignore_and_both_is_no_match() {
    let t = on("push: { branches-ignore: ['wip/**'] }");
    assert!(triggered(&t, &push("refs/heads/main", &[])));
    assert!(!triggered(&t, &push("refs/heads/wip/a", &[])));
    let both = on("push: { branches: [main], branches-ignore: [dev] }");
    assert!(!triggered(&both, &push("refs/heads/main", &[])));
}

#[test]
fn paths_ignore_passes_when_any_path_is_not_ignored() {
    let t = on("push: { paths-ignore: ['docs/**'] }");
    assert!(!triggered(&t, &push("refs/heads/m", &["docs/a.md"])));
    assert!(triggered(
        &t,
        &push("refs/heads/m", &["docs/a.md", "src/a.rs"])
    ));
}

#[test]
fn pull_request_default_types() {
    let t = on("pull_request: {}");
    assert!(triggered(&t, &pr("synchronize", "main", &[])));
    assert!(triggered(&t, &pr("opened", "main", &[])));
    assert!(!triggered(&t, &pr("closed", "main", &[])));
}

#[test]
fn pull_request_types_take_one_string() {
    let t = on("pull_request: { types: closed }");
    assert!(triggered(&t, &pr("closed", "main", &[])));
    assert!(!triggered(&t, &pr("opened", "main", &[])));
}

#[test]
fn pull_request_filters() {
    let t = on("pull_request: { types: [closed], branches: [main], paths: ['src/**'] }");
    assert!(triggered(&t, &pr("closed", "main", &["src/a"])));
    assert!(!triggered(&t, &pr("closed", "dev", &["src/a"])));
    assert!(!triggered(&t, &pr("closed", "main", &["x"])));
    assert!(!triggered(&t, &pr("opened", "main", &["src/a"])));
}

#[test]
fn schedule_and_dispatch() {
    let t = on("schedule: [{ cron: '0 3 * * *' }]\nworkflow_dispatch:");
    assert!(triggered(
        &t,
        &Event::Schedule {
            cron: "0 3 * * *".into()
        }
    ));
    assert!(!triggered(
        &t,
        &Event::Schedule {
            cron: "0 4 * * *".into()
        }
    ));
    assert!(triggered(&t, &Event::Dispatch));
    let none = on("push:");
    assert!(!triggered(&none, &Event::Dispatch));
    assert!(!triggered(
        &none,
        &Event::Schedule {
            cron: "0 3 * * *".into()
        }
    ));
    assert!(!triggered(&t, &push("refs/heads/main", &[])));
}

#[test]
fn empty_changed_passes_path_filters() {
    let t = on("push: { paths: ['src/**'] }\npull_request: { paths-ignore: ['docs/**'] }");
    assert!(triggered(&t, &push("refs/heads/main", &[])));
    assert!(triggered(&t, &pr("opened", "main", &[])));
}

#[test]
fn tag_pushes_skip_path_filters() {
    let t = on("push: { tags: ['v*'], paths: ['src/**'] }");
    assert!(triggered(&t, &push("refs/tags/v1", &["README.md"])));
    assert!(!triggered(&t, &push("refs/heads/main", &["README.md"])));
}

#[test]
fn hostile_and_malformed_patterns() {
    let long = format!("{}-x", "a/".repeat(2500));
    assert!(f(&["**/**/**/**-x"]).matches(&long));
    assert!(!f(&["**/**/**/**-y"]).matches(&long));
    assert!(f(&["a[b"]).matches("a[b") && !f(&["a[b"]).matches("ab"));
    assert!(f(&["a\\"]).matches("a\\"));
}

#[test]
fn only_the_first_300_changed_paths_count() {
    let on = on("push: { paths: ['docs/**'] }\npull_request: { paths-ignore: ['src/**'] }");
    let mut changed: Vec<String> = (0..299).map(|i| format!("src/f{i}.rs")).collect();
    changed.push("docs/a.md".to_owned());
    let push_with = |changed: &[String]| Event::Push {
        r#ref: "refs/heads/main".into(),
        changed: changed.to_vec(),
    };
    let pr_with = |changed: &[String]| Event::PullRequest {
        action: "opened".into(),
        base_ref: "main".into(),
        changed: changed.to_vec(),
    };
    // The 300th path counts.
    assert!(triggered(&on, &push_with(&changed)));
    assert!(triggered(&on, &pr_with(&changed)));
    // The 301st does not.
    changed.insert(0, "src/first.rs".to_owned());
    assert_eq!(changed.len(), 301);
    assert!(!triggered(&on, &push_with(&changed)));
    assert!(!triggered(&on, &pr_with(&changed)));
}

#[test]
fn a_path_filter_that_needs_too_much_work_passes() {
    // Crafted patterns that keep every position reachable, against long
    // paths none of them match: deciding would take seconds, so the
    // filter passes, as when the changed paths are unknown.
    let patterns: Vec<String> = (0..256)
        .map(|i| format!("{}{}b", i % 10, "*a".repeat(100)))
        .collect();
    let on = Triggers {
        push: Some(hatchrun_workflow::PushTrigger {
            paths: Some(Filter::new(patterns)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let changed: Vec<String> = (0..300)
        .map(|i| format!("{}{}", i % 10, "a".repeat(400)))
        .collect();
    let event = Event::Push {
        r#ref: "refs/heads/main".into(),
        changed,
    };
    assert!(triggered(&on, &event));
    // A few such paths are decided: none matches.
    let event = Event::Push {
        r#ref: "refs/heads/main".into(),
        changed: vec![format!("0{}", "a".repeat(40))],
    };
    assert!(!triggered(&on, &event));
}

/// `n` patterns `apps/appNN/**/*.tsx` on pushes, and 300 changed `.ts`
/// paths of about 110 characters under those apps.
fn apps(n: usize) -> (Triggers, Vec<String>) {
    let patterns = (0..n).map(|i| format!("apps/app{i:02}/**/*.tsx")).collect();
    let on = Triggers {
        push: Some(hatchrun_workflow::PushTrigger {
            paths: Some(Filter::new(patterns)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let changed = (0..300)
        .map(|i| {
            format!(
                "apps/app{:02}/src/components/feature/subfeature/deeper/nested/folder/structure/here/Component{i}.test.ts",
                i % n
            )
        })
        .collect();
    (on, changed)
}

#[test]
fn ordinary_path_filters_decide_within_the_work_cap() {
    for n in [100, 256] {
        let (on, mut changed) = apps(n);
        let event = |changed: &[String]| Event::Push {
            r#ref: "refs/heads/main".into(),
            changed: changed.to_vec(),
        };
        assert!(!triggered(&on, &event(&changed)), "{n} patterns");
        changed[299] = changed[299].replace(".test.ts", ".tsx");
        assert!(triggered(&on, &event(&changed)), "{n} patterns");
    }
}

fn run(workflow: &str, branch: &str) -> Event {
    Event::WorkflowRun {
        action: "completed".into(),
        workflow: workflow.into(),
        branch: branch.into(),
    }
}

fn release(action: &str, tag: &str) -> Event {
    Event::Release {
        action: action.into(),
        tag: tag.into(),
    }
}

#[test]
fn workflow_run_matches_the_named_workflow_and_branch() {
    let t = on("workflow_run: { workflows: [ci, lint], branches: [main] }");
    assert_eq!(t.workflow_run.as_ref().unwrap().types, ["completed"]);
    assert!(triggered(&t, &run("ci", "main")));
    assert!(triggered(&t, &run("lint", "main")));
    assert!(!triggered(&t, &run("docs", "main")));
    assert!(!triggered(&t, &run("ci", "feature/a")));
    let requested = Event::WorkflowRun {
        action: "requested".into(),
        workflow: "ci".into(),
        branch: "main".into(),
    };
    assert!(!triggered(&t, &requested));
    let t = on("workflow_run: { workflows: ci, branches-ignore: ['feature/**'] }");
    assert!(triggered(&t, &run("ci", "main")) && !triggered(&t, &run("ci", "feature/a")));
    // Other events do not start it, and it does not start on them.
    assert!(!triggered(&t, &Event::Dispatch));
    assert!(!triggered(&on("push: {}"), &run("ci", "main")));
}

#[test]
fn workflow_run_needs_its_workflows() {
    assert!(serde_saphyr::from_str::<Triggers>("workflow_run: {}").is_err());
    assert!(serde_saphyr::from_str::<Triggers>("workflow_run:").is_err());
}

#[test]
fn release_matches_types_and_tags() {
    let t = on("release:");
    assert_eq!(t.release.as_ref().unwrap().types, ["published"]);
    assert!(triggered(&t, &release("published", "v1.0.0")));
    assert!(!triggered(&t, &release("created", "v1.0.0")));
    let t = on("release: { types: [created, prereleased], tags: ['v*'] }");
    assert!(triggered(&t, &release("created", "v1.0.0")));
    assert!(triggered(&t, &release("prereleased", "v2.0.0-rc.1")));
    assert!(!triggered(&t, &release("published", "v1.0.0")));
    assert!(!triggered(&t, &release("created", "nightly")));
    let t = on("release: { types: published, tags-ignore: ['*-rc*'] }");
    assert!(triggered(&t, &release("published", "v1.0.0")));
    assert!(!triggered(&t, &release("published", "v1.0.0-rc1")));
    assert!(!triggered(
        &on("push: { tags: ['v*'] }"),
        &release("published", "v1")
    ));
}

#[test]
fn new_triggers_round_trip() {
    for yaml in [
        "workflow_run: { workflows: [ci], types: [completed], branches: [main] }",
        "workflow_run: { workflows: [ci, lint], branches-ignore: [wip] }",
        "release: { types: [published, released], tags: ['v*'] }",
        "release:",
    ] {
        let t = on(yaml);
        let written = serde_saphyr::to_string(&t).unwrap();
        assert_eq!(on(&written), t, "{yaml}\n{written}");
    }
    // The default type is left out when written.
    let written = serde_saphyr::to_string(&on("workflow_run: { workflows: [ci] }")).unwrap();
    assert!(!written.contains("types"), "{written}");
}
