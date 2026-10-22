// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The S3 calls behind the job cache (see [`crate::cache`]): listing
//! entries by prefix, multipart uploads, and presigned URLs that let a
//! runner download an entry or upload a part without credentials of its
//! own.
//!
//! Requests address the bucket by virtual host in its region, or by
//! path under `AWS_ENDPOINT_URL` in development. S3 signs the path as
//! sent, so keys are encoded once and never normalized.

use std::fmt::Write;
use std::time::Duration;

use super::{Credentials, Error, Signing, child_text, instructions};
use crate::http::{Request, Response, encode};
use aws_sigv4::http_request::{
    PayloadChecksumKind, PercentEncodingMode, SignableBody, SignatureLocation, SigningSettings,
    UriPathNormalizationMode,
};

/// The URL of `key` in `bucket`, or of the bucket itself for an empty
/// key.
#[must_use]
pub fn object_url(bucket: &str, region: &str, override_url: Option<&str>, key: &str) -> String {
    let path = key.split('/').map(encode).collect::<Vec<_>>().join("/");
    match override_url {
        Some(url) if !url.is_empty() => format!("{}/{bucket}/{path}", url.trim_end_matches('/')),
        _ => format!("https://{bucket}.s3.{region}.amazonaws.com/{path}"),
    }
}

fn settings() -> SigningSettings {
    let mut settings = SigningSettings::default();
    settings.percent_encoding_mode = PercentEncodingMode::Single;
    settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
    settings
}

/// Signs `request` for S3 at `now_ms`, with the payload hash S3 wants in
/// `x-amz-content-sha256`.
///
/// # Errors
///
/// When the URL or a header cannot be signed.
pub fn sign(
    request: &mut Request,
    credentials: &Credentials,
    region: &str,
    now_ms: u64,
) -> Result<(), String> {
    let mut settings = settings();
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let body = request.body.clone();
    let (headers, _) = instructions(
        request,
        credentials,
        Signing {
            region,
            service: "s3",
            now_ms,
            settings,
            body: SignableBody::Bytes(&body),
        },
    )?;
    request.headers.extend(headers);
    Ok(())
}

/// `request`'s URL with a signature in its query that holds for
/// `expires`. Whoever has the URL may send that request, and no other,
/// until then.
///
/// # Errors
///
/// When the URL cannot be signed.
pub fn presign(
    request: &Request,
    credentials: &Credentials,
    region: &str,
    now_ms: u64,
    expires: Duration,
) -> Result<String, String> {
    let mut settings = settings();
    settings.signature_location = SignatureLocation::QueryParams;
    settings.expires_in = Some(expires);
    let (_, params) = instructions(
        request,
        credentials,
        Signing {
            region,
            service: "s3",
            now_ms,
            settings,
            body: SignableBody::UnsignedPayload,
        },
    )?;
    let separator = if request.url.contains('?') { '&' } else { '?' };
    let query = params
        .iter()
        .map(|(name, value)| format!("{}={}", encode(name), encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    Ok(format!("{}{separator}{query}", request.url))
}

/// Downloads the object at `url`.
#[must_use]
pub fn get_object(url: &str) -> Request {
    Request::new("GET", url)
}

/// Lists up to 1,000 keys in the bucket at `bucket_url` that start with
/// `prefix`, after `continuation` when set.
#[must_use]
pub fn list_objects(bucket_url: &str, prefix: &str, continuation: Option<&str>) -> Request {
    let mut url = format!("{bucket_url}?list-type=2&prefix={}", encode(prefix));
    if let Some(token) = continuation {
        url.push_str("&continuation-token=");
        url.push_str(&encode(token));
    }
    Request::new("GET", url)
}

/// One key from a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// The full key.
    pub key: String,
    /// When it was written, as S3 gives it: ISO 8601 in UTC, so later
    /// times sort later as text.
    pub last_modified: String,
    /// Bytes.
    pub size: u64,
}

/// A page of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The keys on this page, in key order.
    pub objects: Vec<Object>,
    /// Where the next page starts, when there is one.
    pub continuation: Option<String>,
}

/// Reads a `ListObjectsV2` answer.
///
/// # Errors
///
/// S3's error, or an answer that does not parse.
pub fn listing(response: &Response) -> Result<Listing, Error> {
    let doc = document(response)?;
    let root = doc.root_element();
    let objects = root
        .children()
        .filter(|node| node.has_tag_name("Contents"))
        .map(|node| {
            Ok(Object {
                key: child_text(node, "Key")
                    .ok_or_else(|| Error::malformed("Contents without Key"))?
                    .to_owned(),
                last_modified: child_text(node, "LastModified")
                    .unwrap_or_default()
                    .to_owned(),
                size: child_text(node, "Size")
                    .and_then(|size| size.parse().ok())
                    .unwrap_or(0),
            })
        })
        .collect::<Result<_, Error>>()?;
    let truncated = child_text(root, "IsTruncated") == Some("true");
    Ok(Listing {
        objects,
        continuation: child_text(root, "NextContinuationToken")
            .filter(|_| truncated)
            .map(str::to_owned),
    })
}

/// Starts a multipart upload to `object_url`.
#[must_use]
pub fn create_multipart_upload(object_url: &str) -> Request {
    Request::new("POST", format!("{object_url}?uploads"))
}

/// The `UploadId` S3 gave a new multipart upload.
///
/// # Errors
///
/// S3's error, or an answer that does not parse.
pub fn upload_id(response: &Response) -> Result<String, Error> {
    let doc = document(response)?;
    child_text(doc.root_element(), "UploadId")
        .map(str::to_owned)
        .ok_or_else(|| Error::malformed("no UploadId in CreateMultipartUpload"))
}

/// Uploads part `number` of `upload_id`. Sent by the runner, through
/// [`presign`].
#[must_use]
pub fn upload_part(object_url: &str, upload_id: &str, number: u16) -> Request {
    Request::new(
        "PUT",
        format!(
            "{object_url}?partNumber={number}&uploadId={}",
            encode(upload_id)
        ),
    )
}

/// An uploaded part: its number and the `ETag` S3 answered the upload
/// with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// 1 to 10,000.
    pub number: u16,
    /// As S3 gave it, quotes included.
    pub etag: String,
}

/// Completes `upload_id` from `parts`, in order. `If-None-Match: *`
/// makes S3 refuse when the key already exists, so of two jobs saving
/// the same key the first one wins.
#[must_use]
pub fn complete_multipart_upload(object_url: &str, upload_id: &str, parts: &[Part]) -> Request {
    let mut body = String::from("<CompleteMultipartUpload>");
    for part in parts {
        let _ = write!(
            body,
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            part.number,
            xml_escape(&part.etag)
        );
    }
    body.push_str("</CompleteMultipartUpload>");
    Request::new(
        "POST",
        format!("{object_url}?uploadId={}", encode(upload_id)),
    )
    .header("Content-Type", "application/xml")
    .header("If-None-Match", "*")
    .body(body)
}

/// How a completed upload ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completed {
    /// The object exists now.
    Written,
    /// Another upload wrote the key first.
    Exists,
}

/// Reads a `CompleteMultipartUpload` answer. S3 can answer 200 and
/// still fail, with an `Error` document in the body.
///
/// # Errors
///
/// S3's error, or an answer that does not parse.
pub fn completed(response: &Response) -> Result<Completed, Error> {
    if response.status == 412 {
        return Ok(Completed::Exists);
    }
    let doc = document(response)?;
    let root = doc.root_element();
    if root.has_tag_name("Error") {
        let code = child_text(root, "Code").unwrap_or_default();
        if code == "PreconditionFailed" {
            return Ok(Completed::Exists);
        }
        return Err(Error {
            status: response.status,
            code: code.to_owned(),
            message: child_text(root, "Message").unwrap_or_default().to_owned(),
        });
    }
    Ok(Completed::Written)
}

/// Abandons `upload_id` and the parts uploaded so far.
#[must_use]
pub fn abort_multipart_upload(object_url: &str, upload_id: &str) -> Request {
    Request::new(
        "DELETE",
        format!("{object_url}?uploadId={}", encode(upload_id)),
    )
}

/// Checks that an answer without a body to read succeeded.
///
/// # Errors
///
/// S3's error.
pub fn ok(response: &Response) -> Result<(), Error> {
    if response.ok() {
        return Ok(());
    }
    document(response).map(|_| ())
}

/// Parses an S3 answer, returning S3's error for anything but a 2xx.
/// S3 answers errors as a bare `<Error>`.
fn document(response: &Response) -> Result<roxmltree::Document<'_>, Error> {
    let text = std::str::from_utf8(&response.body).map_err(|_| Error::malformed("not UTF-8"))?;
    let parsed = roxmltree::Document::parse(text);
    if response.ok() {
        return parsed.map_err(|err| Error::malformed(&format!("not XML: {err}")));
    }
    let found = parsed.ok().and_then(|doc| {
        let root = doc.root_element();
        root.has_tag_name("Error").then(|| {
            (
                child_text(root, "Code").unwrap_or_default().to_owned(),
                child_text(root, "Message").unwrap_or_default().to_owned(),
            )
        })
    });
    let (code, message) =
        found.unwrap_or_else(|| (format!("HTTP{}", response.status), response.text()));
    Err(Error {
        status: response.status,
        code,
        message,
    })
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> Response {
        Response {
            headers: Vec::new(),
            status,
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn addresses_objects_by_virtual_host_or_override() {
        assert_eq!(
            object_url("b", "ap-south-1", None, "1/b/ab/v1/npm key+x"),
            "https://b.s3.ap-south-1.amazonaws.com/1/b/ab/v1/npm%20key%2Bx"
        );
        assert_eq!(
            object_url("b", "ap-south-1", Some("http://localhost:4566/"), "k"),
            "http://localhost:4566/b/k"
        );
        assert_eq!(
            object_url("b", "ap-south-1", None, ""),
            "https://b.s3.ap-south-1.amazonaws.com/"
        );
    }

    #[test]
    fn presigns_like_the_s3_documentation() {
        // "Example: Presigned URL" in Authenticating Requests: Using Query
        // Parameters (AWS Signature Version 4).
        let credentials = Credentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: String::new(),
        };
        let url = presign(
            &get_object("https://examplebucket.s3.amazonaws.com/test.txt"),
            &credentials,
            "us-east-1",
            1_369_353_600_000,
            Duration::from_hours(24),
        )
        .unwrap();
        assert!(url.starts_with("https://examplebucket.s3.amazonaws.com/test.txt?"));
        assert!(url.contains("X-Amz-Expires=86400"));
        assert!(url.contains("X-Amz-SignedHeaders=host"));
        assert!(url.ends_with(
            "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        ));
    }

    #[test]
    fn signs_with_the_payload_hash() {
        let credentials = Credentials {
            access_key_id: "AKID".to_owned(),
            secret_access_key: "secret".to_owned(),
            session_token: "token".to_owned(),
        };
        let mut request = list_objects("https://b.s3.ap-south-1.amazonaws.com/", "1/b/x", None);
        sign(&mut request, &credentials, "ap-south-1", 1_000).unwrap();
        assert_eq!(
            request.header_value("x-amz-content-sha256"),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert_eq!(request.header_value("x-amz-security-token"), Some("token"));
        assert!(request.header_value("authorization").is_some());
    }

    #[test]
    fn lists_by_prefix_and_reads_pages() {
        assert_eq!(
            list_objects("https://b.s3.x.amazonaws.com/", "1/b/a b/", Some("t/1")).url,
            "https://b.s3.x.amazonaws.com/?list-type=2&prefix=1%2Fb%2Fa%20b%2F&continuation-token=t%2F1"
        );
        let page = listing(&response(
            200,
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
              <Name>b</Name><IsTruncated>true</IsTruncated>
              <NextContinuationToken>next</NextContinuationToken>
              <Contents><Key>1/b/a/v/k</Key><LastModified>2026-10-15T10:00:00.000Z</LastModified><Size>42</Size></Contents>
            </ListBucketResult>"#,
        ))
        .unwrap();
        assert_eq!(
            page,
            Listing {
                objects: vec![Object {
                    key: "1/b/a/v/k".to_owned(),
                    last_modified: "2026-10-15T10:00:00.000Z".to_owned(),
                    size: 42,
                }],
                continuation: Some("next".to_owned()),
            }
        );
        let denied = listing(&response(
            403,
            "<Error><Code>AccessDenied</Code><Message>no</Message></Error>",
        ))
        .unwrap_err();
        assert_eq!((denied.status, denied.code.as_str()), (403, "AccessDenied"));
    }

    #[test]
    fn runs_a_multipart_upload() {
        let url = "https://b.s3.x.amazonaws.com/k";
        assert_eq!(create_multipart_upload(url).url, format!("{url}?uploads"));
        assert_eq!(
            upload_id(&response(
                200,
                "<InitiateMultipartUploadResult><Bucket>b</Bucket><Key>k</Key>\
                 <UploadId>u/1</UploadId></InitiateMultipartUploadResult>",
            ))
            .unwrap(),
            "u/1"
        );
        assert_eq!(
            upload_part(url, "u/1", 3).url,
            format!("{url}?partNumber=3&uploadId=u%2F1")
        );
        let request = complete_multipart_upload(
            url,
            "u/1",
            &[Part {
                number: 1,
                etag: "\"e1\"".to_owned(),
            }],
        );
        assert_eq!(request.header_value("If-None-Match"), Some("*"));
        assert_eq!(
            String::from_utf8(request.body).unwrap(),
            "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber>\
             <ETag>&quot;e1&quot;</ETag></Part></CompleteMultipartUpload>"
        );
        assert_eq!(
            completed(&response(200, "<CompleteMultipartUploadResult/>")).unwrap(),
            Completed::Written
        );
        assert_eq!(completed(&response(412, "")).unwrap(), Completed::Exists);
        // S3 can fail a complete after answering 200.
        let failed = completed(&response(
            200,
            "<Error><Code>InternalError</Code><Message>retry</Message></Error>",
        ))
        .unwrap_err();
        assert_eq!(failed.code, "InternalError");
        assert_eq!(abort_multipart_upload(url, "u/1").method, "DELETE");
    }
}
