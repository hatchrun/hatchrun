// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! GitHub-style `${{ }}` expressions.

use hatchrun_workflow::expr::check;
use hatchrun_workflow::{
    ExprError, Functions, JobStatus, evaluate, interpolate, job_condition, truthy,
};
use serde_json::{Value, json};

struct NoFiles;

impl Functions for NoFiles {
    fn hash_files(&self, _patterns: &[String]) -> Result<String, ExprError> {
        Err(ExprError {
            message: "hashFiles runs on the runner".into(),
            offset: 0,
        })
    }

    fn status(&self) -> JobStatus {
        JobStatus::Success
    }
}

struct Files(JobStatus);

impl Functions for Files {
    fn hash_files(&self, patterns: &[String]) -> Result<String, ExprError> {
        Ok(format!("hash:{}", patterns.join("|")))
    }

    fn status(&self) -> JobStatus {
        self.0
    }
}

fn eval(e: &str) -> Value {
    evaluate(e, &json!({}), &NoFiles).unwrap()
}

fn err(e: &str) -> ExprError {
    evaluate(e, &json!({}), &NoFiles).unwrap_err()
}

#[test]
fn loose_equality_matches_github() {
    for (e, want) in [
        ("'1' == 1", true),
        ("null == ''", true),
        ("null == 0", true),
        ("'ABC' == 'abc'", true),
        ("true == 1", true),
        ("'a' == 0", false),
        ("1 < '2'", true),
        ("'x' != 'X'", false),
    ] {
        assert_eq!(
            evaluate(e, &json!({}), &NoFiles).unwrap(),
            json!(want),
            "{e}"
        );
    }
}

#[test]
fn and_or_return_operands() {
    assert_eq!(
        evaluate("'' || 'fallback'", &json!({}), &NoFiles).unwrap(),
        json!("fallback")
    );
    assert_eq!(
        evaluate(
            "github.ref == 'refs/heads/main' && 'prod'",
            &json!({"github": {"ref": "refs/heads/main"}}),
            &NoFiles
        )
        .unwrap(),
        json!("prod")
    );
}

#[test]
fn contexts_and_filters() {
    let ctx = json!({"needs": {"a": {"outputs": {"v": "1"}}, "b": {"outputs": {"v": "2"}}}});
    assert_eq!(
        evaluate("needs.*.outputs.v", &ctx, &NoFiles).unwrap(),
        json!(["1", "2"])
    );
    assert_eq!(
        evaluate("needs['a'].outputs.v", &ctx, &NoFiles).unwrap(),
        json!("1")
    );
    assert_eq!(
        evaluate("needs.missing.outputs", &ctx, &NoFiles).unwrap(),
        json!(null)
    );
}

#[test]
fn functions() {
    assert_eq!(
        evaluate("format('{0}-{{x}}-{1}', 'a', 2)", &json!({}), &NoFiles).unwrap(),
        json!("a-{x}-2")
    );
    assert_eq!(
        evaluate("contains(fromJSON('[\"A\"]'), 'a')", &json!({}), &NoFiles).unwrap(),
        json!(true)
    );
    assert_eq!(
        evaluate("join(fromJSON('[1,2]'), '+')", &json!({}), &NoFiles).unwrap(),
        json!("1+2")
    );
}

#[test]
fn interpolates_and_reports_offsets() {
    assert_eq!(
        interpolate("v${{ 1 + }}", &json!({}), &NoFiles)
            .unwrap_err()
            .offset,
        7
    );
    assert_eq!(
        interpolate(
            "deps-${{ matrix.os }}",
            &json!({"matrix": {"os": "arm"}}),
            &NoFiles
        )
        .unwrap(),
        "deps-arm"
    );
}

#[test]
fn literals() {
    assert_eq!(eval("null"), json!(null));
    assert_eq!(eval("true"), json!(true));
    assert_eq!(eval("false"), json!(false));
    assert_eq!(eval("42"), json!(42));
    assert_eq!(eval("-3"), json!(-3));
    assert_eq!(eval("1.5"), json!(1.5));
    assert_eq!(eval("0xff"), json!(255));
    assert_eq!(eval("2e3"), json!(2000));
    assert_eq!(eval("'it''s'"), json!("it's"));
    assert_eq!(eval("''"), json!(""));
}

#[test]
fn operators() {
    for (e, want) in [
        ("!true", json!(false)),
        ("!''", json!(true)),
        ("!0", json!(true)),
        ("!!'x'", json!(true)),
        ("1 <= 1", json!(true)),
        ("2 > 1", json!(true)),
        ("1 >= 2", json!(false)),
        ("'a' < 'B'", json!(true)),
        ("'a' < 1", json!(false)),
        ("'a' == 'a' && 1 == 2 || 'x'", json!("x")),
        ("(1 == 1)", json!(true)),
        ("!(1 == 2) && 3", json!(3)),
        ("0 && 'x'", json!(0)),
        ("null || 0", json!(0)),
        ("' 1 ' == 1", json!(true)),
        ("'0x10' == 16", json!(true)),
        ("false == ''", json!(true)),
        ("fromJSON('{}') == fromJSON('{}')", json!(false)),
        ("null == null", json!(true)),
        ("'a' != 0", json!(true)),
    ] {
        assert_eq!(eval(e), want, "{e}");
    }
}

#[test]
fn short_circuit_skips_the_right_side() {
    assert_eq!(eval("false && hashFiles('x')"), json!(false));
    assert_eq!(eval("true || hashFiles('x')"), json!(true));
}

#[test]
fn property_access() {
    let ctx = json!({
        "github": {"event": {"commits": [{"id": "a"}, {"id": "b"}, {"x": 1}]}},
        "steps": {"my-step": {"outputs": {"out": "v"}}},
        "matrix": {"list": [10, 20]},
    });
    let e = |s: &str| evaluate(s, &ctx, &NoFiles).unwrap();
    assert_eq!(e("steps.my-step.outputs.out"), json!("v"));
    assert_eq!(e("STEPS.MY-STEP.OUTPUTS.OUT"), json!("v"));
    assert_eq!(e("github.event.commits.*.id"), json!(["a", "b"]));
    assert_eq!(e("github.event.commits[1].id"), json!("b"));
    assert_eq!(e("github.event.commits[5]"), json!(null));
    assert_eq!(e("matrix.list[*]"), json!([10, 20]));
    assert_eq!(e("matrix['list'][0]"), json!(10));
    assert_eq!(e("matrix.list.length"), json!(null));
    assert_eq!(e("github.event.nope.deeper"), json!(null));
    assert_eq!(e("env"), json!(null));
    assert_eq!(e("contains(github.event.commits.*.id, 'B')"), json!(true));
}

#[test]
fn string_functions() {
    for (e, want) in [
        ("contains('Hello', 'ELL')", json!(true)),
        ("contains('Hello', 'z')", json!(false)),
        ("contains(fromJSON('[1, 2]'), '2')", json!(true)),
        ("startsWith('Hello', 'he')", json!(true)),
        ("endsWith('Hello', 'LO')", json!(true)),
        ("endsWith('Hello', 'x')", json!(false)),
        ("format('{0}{1}{0}', 'a', true)", json!("atruea")),
        ("format('{{0}}', 1)", json!("{0}")),
        ("format('{0}', null)", json!("")),
        ("format('{0}', 1.5)", json!("1.5")),
        ("join('abc')", json!("abc")),
        ("join(fromJSON('[\"a\", null, true]'))", json!("a,,true")),
        ("FORMAT('{0}', 'x')", json!("x")),
    ] {
        assert_eq!(eval(e), want, "{e}");
    }
}

#[test]
fn json_functions() {
    assert_eq!(eval("toJSON('a')"), json!("\"a\""));
    assert_eq!(
        eval("toJSON(fromJSON('{\"a\":[1]}'))"),
        json!("{\n  \"a\": [\n    1\n  ]\n}")
    );
    assert_eq!(eval("fromJSON('{\"a\": 1}').a"), json!(1));
    assert_eq!(eval("fromJSON('true')"), json!(true));
    let e = err("fromJSON('{')");
    assert_eq!(e.offset, 0);
}

#[test]
fn status_functions() {
    for (status, success, failure, cancelled) in [
        (JobStatus::Success, true, false, false),
        (JobStatus::Failure, false, true, false),
        (JobStatus::Cancelled, false, false, true),
    ] {
        let f = Files(status);
        let e = |s: &str| evaluate(s, &json!({}), &f).unwrap();
        assert_eq!(e("success()"), json!(success));
        assert_eq!(e("failure()"), json!(failure));
        assert_eq!(e("cancelled()"), json!(cancelled));
        assert_eq!(e("always()"), json!(true));
    }
}

#[test]
fn hash_files_goes_to_the_caller() {
    let f = Files(JobStatus::Success);
    assert_eq!(
        evaluate("hashFiles('a/**', 'b')", &json!({}), &f).unwrap(),
        json!("hash:a/**|b")
    );
    let e = interpolate("k-${{ hashFiles('x') }}", &json!({}), &NoFiles).unwrap_err();
    assert_eq!(e.message, "hashFiles runs on the runner");
    assert_eq!(e.offset, 6);
}

#[test]
fn interpolation() {
    let ctx = json!({"matrix": {"n": 2, "f": 1.25, "o": {"a": 1}, "b": false}});
    let i = |s: &str| interpolate(s, &ctx, &NoFiles).unwrap();
    assert_eq!(i("no expressions"), "no expressions");
    assert_eq!(i("${{matrix.n}}x${{ matrix.f }}"), "2x1.25");
    assert_eq!(i("${{ matrix.missing }}|${{ matrix.b }}"), "|false");
    assert_eq!(i("${{ matrix.o }}"), "Object");
    assert_eq!(i("${{ format('}}') }}"), "}");
    assert_eq!(i("a $ { b"), "a $ { b");
}

#[test]
fn errors_point_at_the_token() {
    for (e, offset) in [
        ("", 0),
        ("   ", 3),
        ("1 &&", 2),
        ("1 == ", 2),
        ("!", 0),
        ("1 2", 2),
        ("'abc", 0),
        ("(1", 0),
        ("a.b", 0),
        ("nope(1)", 0),
        ("contains('a')", 0),
        ("success(1)", 0),
        ("1 # 2", 2),
        ("format('{0}')", 0),
        ("format('{x}', 1)", 0),
        ("format('}', 1)", 0),
        ("github.", 6),
        ("github[0", 6),
        ("  matrix.x == )", 11),
    ] {
        assert_eq!(err(e).offset, offset, "{e:?}: {}", err(e).message);
    }
    let e = interpolate("ab${{ 1", &json!({}), &NoFiles).unwrap_err();
    assert_eq!(e.offset, 2);
}

#[test]
fn truthiness() {
    for (v, want) in [
        (json!(null), false),
        (json!(false), false),
        (json!(true), true),
        (json!(0), false),
        (json!(0.0), false),
        (json!(-1), true),
        (json!(""), false),
        (json!("0"), true),
        (json!([]), true),
        (json!({}), true),
    ] {
        assert_eq!(truthy(&v), want, "{v}");
    }
}

/// Runs `f` on a thread with a stack of `kb` KB, standing in for the
/// small wasm stack of a Worker.
fn small_stack<T: Send + 'static>(kb: usize, f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(kb * 1024)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap()
}

fn nested(open: &str, n: usize, close: &str) -> String {
    format!("{}1{}", open.repeat(n), close.repeat(n))
}

#[test]
fn nesting_is_capped_at_64() {
    // 64 levels take under 64 KB of stack in a release build. Debug
    // frames are several times larger.
    let kb = if cfg!(debug_assertions) { 512 } else { 64 };
    let (ok, deep) = small_stack(kb, || {
        (
            [
                evaluate(&nested("(", 64, ")"), &json!({}), &NoFiles),
                evaluate(&nested("!", 64, ""), &json!({}), &NoFiles),
                evaluate(&nested("format('{0}', ", 64, ")"), &json!({}), &NoFiles),
                evaluate(
                    &format!("fromJSON('[1]'){}", "[0".repeat(64) + &"]".repeat(64)),
                    &json!({}),
                    &NoFiles,
                ),
            ],
            [
                evaluate(&nested("(", 65, ")"), &json!({}), &NoFiles),
                evaluate(&nested("!", 65, ""), &json!({}), &NoFiles),
                evaluate(&nested("format('{0}', ", 65, ")"), &json!({}), &NoFiles),
            ],
        )
    });
    assert_eq!(ok[0].as_ref().unwrap(), &json!(1));
    assert_eq!(ok[1].as_ref().unwrap(), &json!(true));
    assert_eq!(ok[2].as_ref().unwrap(), &json!("1"));
    assert_eq!(ok[3].as_ref().unwrap(), &json!(null));
    for (e, offset) in deep.iter().zip([64, 64, 64 * 14 + 6]) {
        let e = e.as_ref().unwrap_err();
        assert!(e.message.contains("64"), "{}", e.message);
        assert_eq!(e.offset, offset, "{}", e.message);
    }
}

#[test]
fn length_is_capped_at_4096_bytes() {
    let long = format!("'{}'", "a".repeat(4094));
    assert_eq!(long.len(), 4096);
    assert_eq!(eval(&long), json!("a".repeat(4094)));
    let too_long = format!("'{}'", "a".repeat(4095));
    let e = err(&too_long);
    assert!(e.message.contains("4096"), "{}", e.message);
    assert_eq!(e.offset, 0);
    let e = interpolate(&format!("x ${{{{{too_long}}}}}"), &json!({}), &NoFiles).unwrap_err();
    assert!(e.message.contains("4096"), "{}", e.message);
    assert_eq!(e.offset, 5);
}

#[test]
fn long_chains_fit_a_small_stack() {
    let results = small_stack(256, || {
        let chain = |unit: &str, last: &str| {
            let n = (4096 - last.len()) / unit.len();
            let e = format!("{}{last}", unit.repeat(n));
            assert!(e.len() > 4096 - unit.len() && e.len() <= 4096);
            evaluate(&e, &json!({"matrix": {}}), &NoFiles)
        };
        [
            chain("null || ", "1"),
            chain("1 && ", "'x'"),
            chain("1 == ", "1"),
            chain("1 < ", "2"),
            evaluate(
                &format!("matrix{}", "[0]".repeat(4090 / 3)),
                &json!({"matrix": {}}),
                &NoFiles,
            ),
            evaluate(
                &format!("matrix{}", ".a".repeat(4090 / 2)),
                &json!({"matrix": {}}),
                &NoFiles,
            ),
        ]
    });
    let want = [
        json!(1),
        json!("x"),
        json!(true),
        json!(true),
        json!(null),
        json!(null),
    ];
    for (got, want) in results.iter().zip(want) {
        assert_eq!(got.as_ref().unwrap(), &want);
    }
}

#[test]
fn many_nots_fail_cleanly() {
    let e = small_stack(256, || {
        evaluate(&nested("!", 10_000, ""), &json!({}), &NoFiles)
    });
    assert!(e.is_err());
}

/// `format('{0}...{0}', x)` nested `levels` deep around `'aaaaaaaaaaaaaaaa'`,
/// each level repeating its argument `copies` times.
fn blowup(levels: usize, copies: usize) -> String {
    let fmt = format!("format('{}', ", "{0}".repeat(copies));
    format!(
        "{}'{}'{}",
        fmt.repeat(levels),
        "a".repeat(16),
        ")".repeat(levels)
    )
}

#[test]
fn produced_strings_are_capped_at_one_mib() {
    // 16 bytes times 8^12 would be 1 TiB; the cap stops it at the first
    // level past 1 MiB, before that string is made.
    let e = blowup(12, 8);
    assert!(e.len() < 500, "{}", e.len());
    let err = evaluate(&e, &json!({}), &NoFiles).unwrap_err();
    assert!(err.message.contains("format()"), "{}", err.message);
    assert!(err.message.contains("1048576"), "{}", err.message);
    // The innermost level that overflows: 16 * 8^6 = 4 MiB, the 6th
    // call from the inside, whose `format` starts at its offset.
    let level = 12 - 6;
    assert_eq!(
        err.offset,
        level * "format('{0}{0}{0}{0}{0}{0}{0}{0}', ".len()
    );

    // Exactly 1 MiB passes.
    let mib = blowup(16, 2);
    assert_eq!(
        evaluate(&mib, &json!({}), &NoFiles).unwrap(),
        json!("a".repeat(1 << 20))
    );

    let big = json!({"x": {"s": "a".repeat(600 << 10)}});
    let join = evaluate("join(fromJSON('[1,2]'), x.s)", &big, &NoFiles);
    assert_eq!(join.unwrap(), json!(format!("1{}2", "a".repeat(600 << 10))));
    let two = json!({"x": ["a".repeat(600 << 10), "b".repeat(600 << 10)]});
    let err = evaluate("join(x, '')", &two, &NoFiles).unwrap_err();
    assert!(err.message.contains("join()"), "{}", err.message);
    let err = evaluate("toJSON(x)", &two, &NoFiles).unwrap_err();
    assert!(err.message.contains("toJSON()"), "{}", err.message);
    assert_eq!(err.offset, 0);

    let err = interpolate("ab ${{ x[0] }}${{ x[1] }}", &two, &NoFiles).unwrap_err();
    assert!(err.message.contains("1048576"), "{}", err.message);
    assert_eq!(err.offset, 14);
}

/// A format chain of `'0,'` repeated `4^levels` times, wrapped in
/// `[ ... 0]`: a JSON array of `4^levels + 1` zeros, as text.
fn zeros(levels: usize) -> String {
    let mut x = "'0,'".to_owned();
    for _ in 0..levels {
        x = format!("format('{{0}}{{0}}{{0}}{{0}}', {x})");
    }
    format!("format('[{{0}}0]', {x})")
}

#[test]
fn from_json_is_charged_to_one_budget_per_call() {
    // 512 KiB of text is 256 Ki zeros, 8 MiB and more as values: the
    // first `fromJSON` fails before the array is all built.
    let x = zeros(9);
    assert!(x.len() < 256, "{}", x.len());
    let args = vec![format!("fromJSON({x})"); 14].join(", ");
    let e = format!("format('a', {args})");
    let err = evaluate(&e, &json!({}), &NoFiles).unwrap_err();
    assert!(err.message.contains("fromJSON()"), "{}", err.message);
    assert!(err.message.contains("8388608"), "{}", err.message);
    assert_eq!(err.offset, "format('a', ".len());

    // 16 Ki zeros (512 KiB as values) fit alone, and two of them, but
    // every argument of a call and every call share the budget.
    let x = zeros(7);
    let one = format!("fromJSON({x})[0]");
    assert_eq!(evaluate(&one, &json!({}), &NoFiles).unwrap(), json!(0));
    let calls = |n: usize| {
        let args = vec![format!("fromJSON({x})"); n].join(", ");
        format!("format('{{0}}', {args})")
    };
    assert_eq!(
        evaluate(&calls(2), &json!({}), &NoFiles).unwrap(),
        json!("Array")
    );
    let err = evaluate(&calls(16), &json!({}), &NoFiles).unwrap_err();
    assert!(err.message.contains("memory limit"), "{}", err.message);

    // One `interpolate` shares the budget across its expressions.
    let one = format!("${{{{ {one} }}}}");
    assert_eq!(interpolate(&one, &json!({}), &NoFiles).unwrap(), "0");
    let err = interpolate(&one.repeat(16), &json!({}), &NoFiles).unwrap_err();
    assert!(err.message.contains("8388608"), "{}", err.message);
    assert!(err.offset > 0);

    // Strings count too: a 1 MiB string made by doubling spends 2 MiB,
    // so the fifth one is over.
    let mib = blowup(16, 2);
    let e = format!("format('{{0}}', {})", vec![mib; 5].join(", "));
    let err = evaluate(&e, &json!({}), &NoFiles).unwrap_err();
    assert!(err.message.contains("format()"), "{}", err.message);
    assert!(err.message.contains("8388608"), "{}", err.message);
}

#[test]
fn check_parses_without_evaluating() {
    for ok in [
        "github.ref == 'refs/heads/main'",
        "vars.REGION",
        "inputs.debug && success()",
        "hashFiles('**/Cargo.lock')",
        "parameters.x",
        "needs.build.outputs.v",
    ] {
        assert_eq!(check(ok), Ok(()), "{ok}");
    }
    for (bad, words, offset) in [
        ("github.ref ==", "after", 11),
        ("nosuch.x", "unknown context 'nosuch'", 0),
        (
            "false && strategy.job-index",
            "unknown context 'strategy'",
            9,
        ),
        ("nosuch(1)", "unknown function", 0),
        ("contains('a')", "takes 2", 0),
    ] {
        let e = check(bad).unwrap_err();
        assert!(e.message.contains(words), "{bad}: {}", e.message);
        assert_eq!(e.offset, offset, "{bad}");
    }
}

#[test]
fn checks_status_finds_status_calls_in_the_parsed_expression() {
    use hatchrun_workflow::checks_status;
    for (cond, want) in [
        ("success()", true),
        ("github.event_name == 'push' || FAILURE()", true),
        ("!cancelled()", true),
        ("${{ always() }}", true),
        ("x ${{ github.sha }} ${{ cancelled() }}", true),
        ("contains(format('{0}', failure()), 'x')", true),
        ("github.event.commits[success() && 0]", true),
        ("github.ref == 'success()'", false),
        ("steps.failure.outcome == 'always'", false),
        ("${{ github.event.head_commit.message }} always()", false),
        ("github.event_name == 'push'", false),
    ] {
        assert_eq!(checks_status(cond), Ok(want), "{cond}");
    }
    assert!(checks_status("${{ 1 + }}").is_err());
    assert!(checks_status("${{ success()").is_err());
}

#[test]
fn job_condition_defaults_to_success() {
    let ctx = json!({});
    assert!(job_condition(None, &ctx, &Files(JobStatus::Success)).unwrap());
    assert!(!job_condition(None, &ctx, &Files(JobStatus::Failure)).unwrap());
    assert!(!job_condition(None, &ctx, &Files(JobStatus::Cancelled)).unwrap());
}

#[test]
fn job_condition_wraps_without_status_calls() {
    let ctx = json!({ "github": { "ref": "refs/heads/main" } });
    let ok = Files(JobStatus::Success);
    let failed = Files(JobStatus::Failure);
    assert!(job_condition(Some("github.ref == 'refs/heads/main'"), &ctx, &ok).unwrap());
    assert!(!job_condition(Some("github.ref == 'refs/heads/main'"), &ctx, &failed).unwrap());
    assert!(job_condition(Some("${{ always() }}"), &ctx, &failed).unwrap());
    assert!(
        job_condition(
            Some("failure() && github.ref == 'refs/heads/main'"),
            &ctx,
            &failed
        )
        .unwrap()
    );
    assert!(!job_condition(Some("cancelled()"), &ctx, &ok).unwrap());
    assert!(job_condition(Some("${{ 1 == 1 }}"), &ctx, &ok).unwrap());
    assert!(job_condition(Some("(("), &ctx, &ok).is_err());
}
