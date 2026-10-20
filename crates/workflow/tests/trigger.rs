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
