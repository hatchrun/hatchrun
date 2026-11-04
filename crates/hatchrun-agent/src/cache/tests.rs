// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;

mod artifacts;

const RPC: &str = "/twirp/github.actions.results.api.v1.CacheService/";

/// A cloud that hands out part URLs on itself, and an S3 that
/// answers each part with the `ETag` `"e<number>"`.
async fn cloud() -> MockServer {
    let server = MockServer::start().await;
    let uri = server.uri();
    Mock::given(method("POST"))
        .and(path_regex(r"^/v1/cache/uploads/[^/]+/parts$"))
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

async fn proxy(server: &MockServer) -> Proxy {
    Proxy::start_on(
        source::Cache {
            url: format!("{}/v1/cache", server.uri()),
            token: "t.o.k".into(),
        },
        SocketAddr::from(([127, 0, 0, 1], 0)),
        &server.uri(),
    )
    .await
    .unwrap()
}

async fn rpc(proxy: &Proxy, name: &str, body: Value) -> (u16, Value) {
    let response = reqwest::Client::new()
        .post(format!("{}{}{name}", proxy.url(), &RPC[1..]))
        .header("Authorization", "Bearer runtime-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

async fn expect_upload(server: &MockServer, key: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads"))
        .and(body_json(json!({ "key": key, "version": "v1" })))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "uploadId": "up-1" })))
        .mount(server)
        .await;
}

async fn expect_complete(server: &MockServer, parts: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads/up-1/complete"))
        .and(body_json(json!({ "parts": parts })))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(server)
        .await;
}

async fn create(proxy: &Proxy, key: &str) -> String {
    let (status, body) = rpc(
        proxy,
        "CreateCacheEntry",
        json!({ "key": key, "version": "v1" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["ok"], true);
    body["signed_upload_url"].as_str().unwrap().to_owned()
}

async fn finalize(proxy: &Proxy, key: &str) -> Value {
    let (status, body) = rpc(
        proxy,
        "FinalizeCacheEntryUpload",
        json!({ "key": key, "version": "v1", "size_bytes": "5" }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    body
}

async fn azure_put(url: &str, query: &str, body: &'static [u8]) -> reqwest::Response {
    let url = if query.is_empty() {
        url.to_owned()
    } else {
        format!("{url}?{query}")
    };
    reqwest::Client::new()
        .put(url)
        .header("x-ms-blob-type", "BlockBlob")
        .header("x-ms-version", AZURE_VERSION)
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn saves_a_blob_put_in_one_go() {
    let server = cloud().await;
    expect_upload(&server, "npm-abc").await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    let proxy = proxy(&server).await;

    let url = create(&proxy, "npm-abc").await;
    assert!(
        url.starts_with(&format!("{}cache/upload/", proxy.url())),
        "{url}"
    );
    let response = azure_put(&url, "", b"hello").await;
    assert_eq!(response.status(), 201);
    assert!(response.headers().contains_key("x-ms-request-id"));
    assert!(response.headers().contains_key("last-modified"));

    let body = finalize(&proxy, "npm-abc").await;
    assert_eq!(body["ok"], true, "{body}");
    assert!(body["entry_id"].is_string());
}

#[tokio::test]
async fn saves_toolkit_blocks_sent_out_of_order() {
    let server = cloud().await;
    expect_upload(&server, "big").await;
    expect_complete(
        &server,
        json!([{ "number": 1, "etag": "\"e1\"" }, { "number": 2, "etag": "\"e2\"" }]),
    )
    .await;
    let proxy = proxy(&server).await;
    let url = create(&proxy, "big").await;

    let id = |i: u32| STANDARD.encode(format!("{:>36}{i:012}", "prefix"));
    for (i, body) in [(1, &b"second"[..]), (0, &b"first"[..])] {
        let query = format!("comp=block&blockid={}", urlencode(&id(i)));
        assert_eq!(azure_put(&url, &query, body).await.status(), 201);
    }
    let list = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList><Latest>{}</Latest><Latest>{}</Latest></BlockList>",
        id(0),
        id(1)
    );
    let response = reqwest::Client::new()
        .put(format!("{url}?comp=blocklist"))
        .body(list)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    assert_eq!(finalize(&proxy, "big").await["ok"], true);
}

#[tokio::test]
async fn saves_buildkit_blocks_as_one_part() {
    let server = cloud().await;
    expect_upload(&server, "buildkit-blob-1-sha256:ab").await;
    expect_complete(&server, json!([{ "number": 1, "etag": "\"e1\"" }])).await;
    let proxy = proxy(&server).await;
    let url = create(&proxy, "buildkit-blob-1-sha256:ab").await;

    let id = |i: u32| {
        let mut raw = [0u8; 64];
        raw[16..20].copy_from_slice(&i.to_be_bytes());
        STANDARD.encode(raw)
    };
    for i in 0..3 {
        let query = format!("comp=block&blockid={}", urlencode(&id(i)));
        assert_eq!(azure_put(&url, &query, b"layer").await.status(), 201);
    }
    let list: String = (0..3)
        .map(|i| ["<Latest>", &id(i), "</Latest>"].concat())
        .collect();
    let response = reqwest::Client::new()
        .put(format!("{url}?comp=blocklist"))
        .body(format!("<BlockList>{list}</BlockList>"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    assert_eq!(
        finalize(&proxy, "buildkit-blob-1-sha256:ab").await["ok"],
        true
    );

    let received = server.received_requests().await.unwrap();
    let part = received
        .iter()
        .find(|req| req.url.path() == "/s3/part/1")
        .unwrap();
    assert_eq!(part.body, b"layerlayerlayer");
}

#[tokio::test]
async fn an_existing_entry_is_already_exists() {
    let server = cloud().await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads"))
        .respond_with(ResponseTemplate::new(409))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;

    let (status, body) = rpc(
        &proxy,
        "CreateCacheEntry",
        json!({ "key": "k", "version": "v1" }),
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(body["code"], "already_exists");
}

#[tokio::test]
async fn a_failed_save_is_an_error_not_already_exists() {
    let server = cloud().await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/uploads"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;

    let (status, body) = rpc(
        &proxy,
        "CreateCacheEntry",
        json!({ "key": "k", "version": "v1" }),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(body["code"], "unavailable");
}

#[tokio::test]
async fn finalizing_an_unfinished_upload_abandons_it() {
    let server = cloud().await;
    expect_upload(&server, "k").await;
    Mock::given(method("DELETE"))
        .and(path("/v1/cache/uploads/up-1"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;
    create(&proxy, "k").await;

    assert_eq!(finalize(&proxy, "k").await["ok"], false);
}

const ENTRY: &[u8] = b"0123456789";

/// An S3 that serves [`ENTRY`] at `/s3/entry`, whole or by `Range`,
/// and refuses a request that is not signed.
async fn s3(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/s3/entry"))
        .respond_with(|req: &wiremock::Request| {
            if !req.url.query().unwrap_or("").contains("X-Amz-Signature=") {
                return ResponseTemplate::new(403).set_body_string("<Error/>");
            }
            let Some(range) = req.headers.get("range") else {
                return ResponseTemplate::new(200).set_body_bytes(ENTRY.to_vec());
            };
            let (start, end) = range
                .to_str()
                .unwrap()
                .strip_prefix("bytes=")
                .unwrap()
                .split_once('-')
                .unwrap();
            let start: usize = start.parse().unwrap();
            let end: usize = end.parse().unwrap_or(ENTRY.len() - 1);
            ResponseTemplate::new(206)
                .insert_header(
                    "Content-Range",
                    format!("bytes {start}-{end}/{}", ENTRY.len()),
                )
                .set_body_bytes(ENTRY[start..=end].to_vec())
        })
        .mount(server)
        .await;
}

/// A cloud whose lookups find [`ENTRY`] behind `entry`.
async fn found(server: &MockServer, entry: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/cache/lookup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "npm-old", "url": entry, "size": ENTRY.len()
        })))
        .mount(server)
        .await;
}

async fn look_up(proxy: &Proxy) -> String {
    let (status, body) = rpc(
        proxy,
        "GetCacheEntryDownloadURL",
        json!({ "key": "npm-abc", "restore_keys": ["npm-"], "version": "v1" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true, "{body}");
    body["signed_download_url"].as_str().unwrap().to_owned()
}

/// The client the toolkit downloads with is built for the proxy's
/// `http:` URL and cannot follow a redirect to S3.
fn no_redirects() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn restores_from_s3() {
    let server = cloud().await;
    s3(&server).await;
    let entry = format!("{}/s3/entry?X-Amz-Signature=abc", server.uri());
    Mock::given(method("POST"))
        .and(path("/v1/cache/lookup"))
        .and(body_json(json!({
            "key": "npm-abc", "restoreKeys": ["npm-"], "version": "v1"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "npm-old", "url": entry, "size": 10
        })))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;

    let (status, body) = rpc(
        &proxy,
        "GetCacheEntryDownloadURL",
        json!({ "key": "npm-abc", "restore_keys": ["npm-"], "version": "v1" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], true);
    assert_eq!(body["matched_key"], "npm-old");
    let url = body["signed_download_url"].as_str().unwrap();
    assert!(
        url.starts_with(&format!("{}cache/download/", proxy.url())),
        "{url}"
    );

    // The toolkit's single GET, which checks Content-Length.
    let response = no_redirects().get(url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], "10");
    assert!(response.headers().contains_key("x-ms-request-id"));
    assert_eq!(response.bytes().await.unwrap(), ENTRY);

    let received = server.received_requests().await.unwrap();
    let get = received
        .iter()
        .find(|req| req.url.path() == "/s3/entry")
        .unwrap();
    assert!(!get.headers.contains_key("range"));
}

#[tokio::test]
async fn passes_ranges_through() {
    let server = cloud().await;
    s3(&server).await;
    found(
        &server,
        &format!("{}/s3/entry?X-Amz-Signature=abc", server.uri()),
    )
    .await;
    let proxy = proxy(&server).await;
    let url = look_up(&proxy).await;
    let http = no_redirects();

    // The toolkit's concurrent download: HEAD for the length, then
    // ranged GETs at once.
    let response = http.head(&url).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-length"], "10");
    assert_eq!(response.headers()["accept-ranges"], "bytes");
    let get = |range: &'static str| http.get(&url).header("Range", range).send();
    let (first, second) = tokio::join!(get("bytes=0-4"), get("bytes=5-9"));
    let mut body = Vec::new();
    for (response, range) in [(first, "bytes 0-4/10"), (second, "bytes 5-9/10")] {
        let response = response.unwrap();
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["content-range"], range);
        assert_eq!(response.headers()["content-length"], "5");
        body.extend_from_slice(&response.bytes().await.unwrap());
    }
    assert_eq!(body, ENTRY);

    // BuildKit's Go Azure SDK, which S3 would ignore.
    let response = http
        .get(&url)
        .header("x-ms-range", "bytes=4-")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 206);
    assert_eq!(response.headers()["content-range"], "bytes 4-9/10");
    assert_eq!(response.headers()["x-ms-blob-type"], "BlockBlob");
    assert_eq!(response.bytes().await.unwrap(), &b"456789"[..]);
}

#[tokio::test]
async fn download_urls_are_not_guessable() {
    let server = cloud().await;
    s3(&server).await;
    found(
        &server,
        &format!("{}/s3/entry?X-Amz-Signature=abc", server.uri()),
    )
    .await;
    let proxy = proxy(&server).await;

    let first = look_up(&proxy).await;
    let second = look_up(&proxy).await;
    assert_ne!(first, second);
    let base = format!("{}cache/download/", proxy.url());
    for url in [&first, &second] {
        let token = url.strip_prefix(&base).unwrap();
        assert_eq!(token.len(), 32, "{token}");
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    }

    for guess in ["1", "2", "3", "0000"] {
        let url = format!("{base}{guess}");
        assert_eq!(reqwest::get(&url).await.unwrap().status(), 404);
        let response = reqwest::Client::new().head(&url).send().await.unwrap();
        assert_eq!(response.status(), 404);
    }
}

#[tokio::test]
async fn an_s3_refusal_passes_through() {
    let server = cloud().await;
    s3(&server).await;
    found(&server, &format!("{}/s3/entry", server.uri())).await;
    let proxy = proxy(&server).await;
    let url = look_up(&proxy).await;

    let response = no_redirects().get(&url).send().await.unwrap();
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn a_failed_lookup_is_a_miss() {
    let server = cloud().await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/lookup"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;

    let (status, body) = rpc(
        &proxy,
        "GetCacheEntryDownloadURL",
        json!({ "key": "k", "version": "v1" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["ok"], false);
}

#[tokio::test]
async fn health_needs_the_cloud_to_take_the_token() {
    let server = cloud().await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/lookup"))
        .respond_with(ResponseTemplate::new(404))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/cache/lookup"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;

    proxy.check().await.unwrap();
    let err = proxy.check().await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "{err:#}");
}

#[tokio::test]
async fn malformed_rpcs_are_twirp_errors() {
    let server = cloud().await;
    let proxy = proxy(&server).await;
    let (status, body) = rpc(&proxy, "CreateCacheEntry", json!({ "version": "v1" })).await;
    assert_eq!(status, 400);
    assert_eq!(body["code"], "malformed");
}

#[tokio::test]
async fn passes_other_twirp_calls_to_github() {
    let server = cloud().await;
    Mock::given(method("POST"))
        .and(path(
            "/twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
        ))
        .and(wiremock::matchers::header(
            "authorization",
            "Bearer runtime-token",
        ))
        .and(body_json(json!({ "name": "dist" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "ok": true, "signed_upload_url": "https://x.blob.core.windows.net/a"
        })))
        .mount(&server)
        .await;
    let proxy = proxy(&server).await;

    let response = reqwest::Client::new()
        .post(format!(
            "{}twirp/github.actions.results.api.v1.ArtifactService/CreateArtifact",
            proxy.url()
        ))
        .header("Authorization", "Bearer runtime-token")
        .json(&json!({ "name": "dist" }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["signed_upload_url"],
        "https://x.blob.core.windows.net/a"
    );

    // Unknown to the proxy, so wiremock's 404 rather than the
    // proxy's bad_route.
    let response = reqwest::Client::new()
        .post(format!("{}{}ListCacheEntries", proxy.url(), &RPC[1..]))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(response.text().await.unwrap(), "");
}

fn urlencode(text: &str) -> String {
    text.replace('+', "%2B")
        .replace('/', "%2F")
        .replace('=', "%3D")
}
