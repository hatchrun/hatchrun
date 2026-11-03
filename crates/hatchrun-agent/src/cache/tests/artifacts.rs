// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The artifact service, against a control plane on wiremock.

use super::*;

const ARTIFACT_RPC: &str = "twirp/github.actions.results.api.v1.ArtifactService/";

/// A proxy that serves artifacts only, from `server`.
async fn artifact_proxy(server: &MockServer) -> Proxy {
    Proxy::start_with(
        Services {
            cache: None,
            artifacts: Some(ArtifactConfig {
                service: source::Cache {
                    url: format!("{}/v1/agent/artifacts", server.uri()),
                    token: "job.token".into(),
                },
                run_id: "12".into(),
                job_id: "build".into(),
            }),
        },
        SocketAddr::from(([127, 0, 0, 1], 0)),
        &server.uri(),
    )
    .await
    .unwrap()
}

async fn artifact_rpc(proxy: &Proxy, name: &str, body: Value) -> (u16, Value) {
    let token = proxy.runtime_token().unwrap();
    let response = reqwest::Client::new()
        .post(format!("{}{ARTIFACT_RPC}{name}", proxy.url()))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

/// A control plane that hands out part URLs on itself, and an S3 that
/// answers each part with the `ETag` `"e<number>"`.
async fn control_plane() -> MockServer {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/agent/artifacts/uploads/[^/]+/parts$"))
        .respond_with(move |req: &wiremock::Request| {
            let numbers: Value = serde_json::from_slice(&req.body).unwrap();
            let parts: Vec<Value> = numbers["numbers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| json!({ "number": n, "url": format!("{uri}/s3/part/{n}") }))
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "parts": parts }))
        })
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/s3/part/\d+$"))
        .respond_with(|req: &wiremock::Request| {
            let n = req.url.path().rsplit('/').next().unwrap().to_owned();
            ResponseTemplate::new(200).insert_header("ETag", format!("\"e{n}\""))
        })
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn uploads_an_artifact_through_s3() {
    let server = control_plane().await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/uploads"))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer job.token",
        ))
        .and(body_json(json!({ "name": "dist" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "uploadId": "7" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/uploads/7/complete"))
        .and(body_json(json!({
            "parts": [{ "number": 1, "etag": "\"e1\"" }],
            "size": 5,
            "digest": "sha256:ab"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 9 })))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = artifact_proxy(&server).await;

    let (status, body) = artifact_rpc(
        &proxy,
        "CreateArtifact",
        json!({
            "workflow_run_backend_id": "12", "workflow_job_run_backend_id": "build",
            "name": "dist", "version": 4
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    let url = body["signed_upload_url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("{}cache/upload/", proxy.url())),
        "{url}"
    );
    assert_eq!(azure_put(url, "", b"hello").await.status(), 201);

    let (status, body) = artifact_rpc(
        &proxy,
        "FinalizeArtifact",
        json!({ "name": "dist", "size": "5", "hash": "sha256:ab" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "ok": true, "artifact_id": "9" }));
}

#[tokio::test]
async fn a_taken_name_is_already_exists() {
    let server = control_plane().await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/uploads"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    let proxy = artifact_proxy(&server).await;
    let (status, body) = artifact_rpc(&proxy, "CreateArtifact", json!({ "name": "dist" })).await;
    assert_eq!(status, 409);
    assert_eq!(body["code"], "already_exists");

    let (status, body) = artifact_rpc(&proxy, "CreateArtifact", json!({ "name": "a/b" })).await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], "invalid_argument");
}

#[tokio::test]
async fn a_lost_name_race_finalizes_not_ok() {
    let server = control_plane().await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/uploads"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "uploadId": "7" })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/uploads/7/complete"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    let proxy = artifact_proxy(&server).await;
    let (_, body) = artifact_rpc(&proxy, "CreateArtifact", json!({ "name": "dist" })).await;
    let url = body["signed_upload_url"].as_str().unwrap();
    assert_eq!(azure_put(url, "", b"x").await.status(), 201);
    let (status, body) = artifact_rpc(
        &proxy,
        "FinalizeArtifact",
        json!({ "name": "dist", "size": 1 }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);
}

#[tokio::test]
async fn lists_and_downloads_artifacts() {
    let server = control_plane().await;
    s3(&server).await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/list"))
        .and(body_json(json!({ "name": "dist", "id": null })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "artifacts": [{ "id": 9, "name": "dist", "size": 10, "createdAt": 1_700_000_000_123_u64 }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/url"))
        .and(body_json(json!({ "name": "dist" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "url": format!("{}/s3/entry?X-Amz-Signature=abc", server.uri()), "size": 10
        })))
        .mount(&server)
        .await;
    let proxy = artifact_proxy(&server).await;

    let (status, body) = artifact_rpc(
        &proxy,
        "ListArtifacts",
        json!({
            "workflowRunBackendId": "12", "workflowJobRunBackendId": "build",
            "nameFilter": "dist"
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["artifacts"][0],
        json!({
            "workflow_run_backend_id": "12", "workflow_job_run_backend_id": "build",
            "database_id": "9", "name": "dist", "size": "10",
            "created_at": "2023-11-14T22:13:20.123Z"
        })
    );

    let (status, body) =
        artifact_rpc(&proxy, "GetSignedArtifactURL", json!({ "name": "dist" })).await;
    assert_eq!(status, 200, "{body}");
    let url = body["signed_url"].as_str().unwrap();
    let response = no_redirects().get(url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], "10");
    assert_eq!(response.bytes().await.unwrap(), ENTRY);
}

#[tokio::test]
async fn a_missing_artifact_is_not_found() {
    let server = control_plane().await;
    for route in ["url", "delete"] {
        Mock::given(method("POST"))
            .and(path(format!("/v1/agent/artifacts/{route}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
    }
    let proxy = artifact_proxy(&server).await;
    for rpc in ["GetSignedArtifactURL", "DeleteArtifact"] {
        let (status, body) = artifact_rpc(&proxy, rpc, json!({ "name": "nope" })).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (404, Some("not_found")),
            "{rpc}"
        );
    }
}

#[tokio::test]
async fn deletes_an_artifact() {
    let server = control_plane().await;
    Mock::given(method("POST"))
        .and(path("/v1/agent/artifacts/delete"))
        .and(body_json(json!({ "name": "dist" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 9 })))
        .mount(&server)
        .await;
    let proxy = artifact_proxy(&server).await;
    let (status, body) = artifact_rpc(&proxy, "DeleteArtifact", json!({ "name": "dist" })).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({ "ok": true, "artifact_id": "9" }));
}

#[tokio::test]
async fn artifact_calls_need_the_runtime_token() {
    let server = control_plane().await;
    let proxy = artifact_proxy(&server).await;
    let url = format!("{}{ARTIFACT_RPC}ListArtifacts", proxy.url());
    for auth in [None, Some("Bearer job.token"), Some("Bearer nope")] {
        let mut request = reqwest::Client::new().post(&url).json(&json!({}));
        if let Some(auth) = auth {
            request = request.header("Authorization", auth);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 401, "{auth:?}");
    }
    // The control plane was never called.
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn the_runtime_token_names_the_run_and_job() {
    let server = control_plane().await;
    let proxy = artifact_proxy(&server).await;
    let token = proxy.runtime_token().unwrap();
    let payload = token.split('.').nth(1).unwrap();
    let claims: Value = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(claims["scp"], "Actions.Results:12:build");
}

#[tokio::test]
async fn an_artifact_only_proxy_answers_the_cache_as_empty() {
    let server = control_plane().await;
    let proxy = artifact_proxy(&server).await;
    proxy.check().await.unwrap();
    let (status, body) = rpc(
        &proxy,
        "GetCacheEntryDownloadURL",
        json!({ "key": "k", "version": "v1" }),
    )
    .await;
    assert_eq!((status, &body["ok"]), (200, &json!(false)));
    let (status, body) = rpc(
        &proxy,
        "CreateCacheEntry",
        json!({ "key": "k", "version": "v1" }),
    )
    .await;
    assert_eq!((status, body["code"].as_str()), (503, Some("unavailable")));
}
