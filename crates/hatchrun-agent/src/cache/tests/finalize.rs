// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Finalizes that are retried, repeated or overlap, against a control
//! plane on wiremock.

use std::time::Duration;

use super::artifacts::{artifact_proxy, artifact_rpc, control_plane};
use super::*;

/// A cache upload `up-1` for `k` with its block put.
async fn saved_blob(proxy: &Proxy) {
    let url = create(proxy, "k").await;
    assert_eq!(azure_put(&url, "", b"hello").await.status(), 201);
}

async fn raw_finalize(proxy: &Proxy) -> (u16, Value) {
    rpc(
        proxy,
        "FinalizeCacheEntryUpload",
        json!({ "key": "k", "version": "v1", "size_bytes": "5" }),
    )
    .await
}

#[tokio::test]
async fn a_retry_after_a_transient_error_completes() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(503).insert_header("Retry-After", "2"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let (status, body) = raw_finalize(&proxy).await;
    assert_eq!(status, 503, "{body}");
    assert_eq!(body["code"], "unavailable");
    let (status, body) = raw_finalize(&proxy).await;
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");
}

#[tokio::test]
async fn a_repeat_after_success_answers_the_same() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let first = finalize(&proxy, "k").await;
    assert_eq!(first["ok"], true);
    assert_eq!(finalize(&proxy, "k").await, first);
}

#[tokio::test]
async fn a_lost_race_answers_the_same_again() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(409))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let first = finalize(&proxy, "k").await;
    assert_eq!(first["ok"], false);
    assert_eq!(finalize(&proxy, "k").await, first);
}

#[tokio::test]
async fn an_upload_gone_from_the_control_plane_is_not_saved() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let body = finalize(&proxy, "k").await;
    assert_eq!(body["ok"], false);
    let message = body["message"].as_str().unwrap();
    assert!(message.contains("the upload is gone"), "{message}");
}

#[tokio::test]
async fn overlapping_finalizes_complete_once() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_millis(300)))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let (a, b) = tokio::join!(finalize(&proxy, "k"), finalize(&proxy, "k"));
    assert_eq!(a["ok"], true);
    assert_eq!(a, b);
}

/// Polls `done` until it holds, and panics naming `what` after 10 s.
async fn until<F, Fut>(what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    tokio::time::timeout(Duration::from_secs(10), async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// How many requests with the HTTP method `verb` and a path ending in
/// `tail` the control plane has received.
async fn requests(server: &MockServer, verb: &str, tail: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|req| req.method.as_str() == verb && req.url.path().ends_with(tail))
        .count()
}

/// Completes of `up-1` received.
async fn completes(server: &MockServer) -> usize {
    requests(server, "POST", "/up-1/complete").await
}

#[tokio::test]
async fn a_dropped_finalize_leaves_the_upload_for_a_retry() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    // The first complete blocks; the retry's gets the plain answer.
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_secs(60)))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    // Dropping the request future closes the connection.
    tokio::select! {
        _ = raw_finalize(&proxy) => panic!("the finalize answered"),
        () = until("the first complete", || async { completes(&server).await == 1 }) => {}
    }
    let (status, body) = tokio::time::timeout(Duration::from_secs(10), raw_finalize(&proxy))
        .await
        .expect("the dropped finalize released the upload");
    assert_eq!((status, &body["ok"]), (200, &json!(true)), "{body}");
    assert_eq!(completes(&server).await, 2);
}

#[tokio::test]
async fn a_create_while_a_finalize_waits_does_not_abandon_it() {
    let server = cloud().await;
    expect_two_uploads(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(204).set_delay(Duration::from_millis(500)))
        .expect(1)
        .mount(&server)
        .await;
    expect_delete(&server, "up-1", 0).await;
    expect_delete(&server, "up-2", 1).await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let again = async {
        until("the complete", || async { completes(&server).await == 1 }).await;
        create(&proxy, "k").await;
    };
    let (finalized, ()) = tokio::join!(raw_finalize(&proxy), again);
    assert_eq!(finalized.1["ok"], true);
    proxy.stop().await;
}

#[tokio::test]
async fn a_replaced_upload_with_no_answer_is_aborted_by_stop() {
    let server = cloud().await;
    expect_two_uploads(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_millis(300)))
        .mount(&server)
        .await;
    expect_delete(&server, "up-1", 1).await;
    expect_delete(&server, "up-2", 1).await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;

    let again = async {
        until("the complete", || async { completes(&server).await == 1 }).await;
        create(&proxy, "k").await;
    };
    let (finalized, ()) = tokio::join!(raw_finalize(&proxy), again);
    assert_eq!(finalized.0, 503);
    // Claimed and replaced, it was left alone; now it is idle.
    assert_eq!(requests(&server, "DELETE", "/up-1").await, 0);
    proxy.stop().await;
}

#[tokio::test]
async fn a_put_in_flight_when_the_upload_settles_gets_a_404() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    // The part of the put blocks, so the finalize settles meanwhile.
    Mock::given(method("PUT"))
        .and(path("/s3/part/1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("ETag", "\"e1\"")
                .set_delay(Duration::from_millis(500)),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    expect_delete(&server, "up-1", 1).await;
    let proxy = proxy(&server).await;
    let url = create(&proxy, "k").await;
    let upload = lock(&proxy.state.uploads).by_id["up-1"].clone();

    let settle = async {
        until("the part", || async {
            requests(&server, "PUT", "/s3/part/1").await == 1
        })
        .await;
        finalize(&proxy, "k").await
    };
    let (put, finalized) = tokio::join!(azure_put(&url, "", b"hello"), settle);
    assert_eq!(finalized["ok"], false);
    assert_eq!(put.status(), 404);
    assert!(upload.lock().committed.is_none());
}

#[tokio::test]
async fn creating_an_artifact_again_during_a_finalize_keeps_the_upload() {
    let server = control_plane().await;
    for id in ["7", "8"] {
        Mock::given(method("POST"))
            .and(path("/v1/agent/artifacts/uploads"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "uploadId": id })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/uploads/7/complete"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "id": 9 }))
                .set_delay(Duration::from_millis(400)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/agent/artifacts/uploads/7"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .named("abort of the finalizing upload")
        .mount(&server)
        .await;
    let proxy = artifact_proxy(&server).await;
    let created = artifact_rpc(&proxy, "CreateArtifact", json!({ "name": "dist" })).await;
    let url = created.1["signed_upload_url"].as_str().unwrap().to_owned();
    assert_eq!(azure_put(&url, "", b"hello").await.status(), 201);

    let finalizing = artifact_rpc(
        &proxy,
        "FinalizeArtifact",
        json!({ "name": "dist", "size": "5" }),
    );
    let again = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        artifact_rpc(&proxy, "CreateArtifact", json!({ "name": "dist" })).await
    };
    let (finalized, again) = tokio::join!(finalizing, again);
    assert_eq!(finalized.1, json!({ "ok": true, "artifact_id": "9" }));
    assert_eq!(again.1["ok"], true);
    proxy.stop().await;
}

#[tokio::test]
async fn stop_aborts_an_upload_whose_finalize_failed_but_not_a_saved_one() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/cache/uploads/up-1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;
    assert_eq!(raw_finalize(&proxy).await.0, 503);
    proxy.stop().await;
}

#[tokio::test]
async fn stop_leaves_a_saved_upload() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(204))
        .expect(0)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;
    assert_eq!(finalize(&proxy, "k").await["ok"], true);
    proxy.stop().await;
}

/// Two creates of `k`, which get the upload ids `up-1` and `up-2`.
async fn expect_two_uploads(server: &MockServer) {
    for id in ["up-1", "up-2"] {
        Mock::given(method("POST"))
            .and(path("/v1/cache/uploads"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "uploadId": id })))
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
}

async fn expect_delete(server: &MockServer, id: &str, times: u64) {
    Mock::given(method("DELETE"))
        .and(path(format!("/v1/cache/uploads/{id}")))
        .respond_with(ResponseTemplate::new(204))
        .expect(times)
        .mount(server)
        .await;
}

#[tokio::test]
async fn creating_again_abandons_an_idle_upload() {
    let server = cloud().await;
    expect_two_uploads(&server).await;
    expect_delete(&server, "up-1", 1).await;
    expect_delete(&server, "up-2", 1).await;
    let proxy = proxy(&server).await;
    create(&proxy, "k").await;
    create(&proxy, "k").await;
    proxy.stop().await;
}

#[tokio::test]
async fn creating_again_keeps_a_settled_upload_for_stop() {
    let server = cloud().await;
    expect_two_uploads(&server).await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    expect_delete(&server, "up-1", 0).await;
    expect_delete(&server, "up-2", 1).await;
    let proxy = proxy(&server).await;
    saved_blob(&proxy).await;
    assert_eq!(finalize(&proxy, "k").await["ok"], true);
    create(&proxy, "k").await;
    proxy.stop().await;
}

#[tokio::test]
async fn a_settled_upload_takes_no_more_data() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    let proxy = proxy(&server).await;
    let url = create(&proxy, "k").await;
    assert_eq!(azure_put(&url, "", b"hello").await.status(), 201);
    assert_eq!(finalize(&proxy, "k").await["ok"], true);
    assert_eq!(azure_put(&url, "", b"again").await.status(), 404);
    assert_eq!(
        azure_put(&url, "comp=block&blockid=YQ==", b"x")
            .await
            .status(),
        404
    );
}
