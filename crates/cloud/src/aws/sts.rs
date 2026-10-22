// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! `AssumeRoleWithWebIdentity`: trades an `oidc.hatchrun.io` token for
//! the tenant role's credentials. The call is not signed; the token is
//! the credential.

use super::{Credentials, Error, child_text, document, query};
use crate::http::{Request, Response};

/// STS API version.
const VERSION: &str = "2011-06-15";
/// The shortest session STS grants, ample for one launch.
pub const DURATION_SECONDS: u32 = 900;

/// The request for `role_arn`'s credentials. `session_name` shows in the
/// customer's `CloudTrail` next to every call made with them.
#[must_use]
pub fn assume_role_with_web_identity(
    endpoint: &str,
    role_arn: &str,
    session_name: &str,
    token: &str,
) -> Request {
    query(
        endpoint,
        "AssumeRoleWithWebIdentity",
        VERSION,
        &[
            ("RoleArn".to_owned(), role_arn.to_owned()),
            ("RoleSessionName".to_owned(), session_name.to_owned()),
            ("WebIdentityToken".to_owned(), token.to_owned()),
            ("DurationSeconds".to_owned(), DURATION_SECONDS.to_string()),
        ],
    )
}

/// Reads the credentials out of the answer.
///
/// # Errors
///
/// STS's error, or a malformed answer.
pub fn credentials(response: &Response) -> Result<Credentials, Error> {
    let doc = document(response)?;
    let root = doc.root();
    let field = |name: &str| {
        child_text(root, name)
            .map(str::to_owned)
            .ok_or_else(|| Error::malformed(&format!("no {name} in AssumeRoleWithWebIdentity")))
    };
    Ok(Credentials {
        access_key_id: field("AccessKeyId")?,
        secret_access_key: field("SecretAccessKey")?,
        session_token: field("SessionToken")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_for_the_role() {
        let request = assume_role_with_web_identity(
            "https://sts.eu-west-1.amazonaws.com/",
            "arn:aws:iam::123456789012:role/hatchrun",
            "hatchrun-job-7",
            "a.b.c",
        );
        assert_eq!(request.method, "POST");
        assert_eq!(request.url, "https://sts.eu-west-1.amazonaws.com/");
        assert_eq!(
            String::from_utf8(request.body.clone()).unwrap(),
            "Action=AssumeRoleWithWebIdentity&Version=2011-06-15\
             &RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Fhatchrun\
             &RoleSessionName=hatchrun-job-7&WebIdentityToken=a.b.c&DurationSeconds=900"
        );
        assert_eq!(request.header_value("authorization"), None);
    }

    #[test]
    fn reads_credentials() {
        let response = Response { headers: Vec::new(),
            status: 200,
            body: br#"<AssumeRoleWithWebIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <AssumeRoleWithWebIdentityResult>
    <SubjectFromWebIdentityToken>tenant:acme1234</SubjectFromWebIdentityToken>
    <Audience>sts.amazonaws.com</Audience>
    <AssumedRoleUser>
      <Arn>arn:aws:sts::123456789012:assumed-role/hatchrun/hatchrun-job-7</Arn>
      <AssumedRoleId>AROAEXAMPLE:hatchrun-job-7</AssumedRoleId>
    </AssumedRoleUser>
    <Credentials>
      <SessionToken>FwoGZXIvYXdz+/=</SessionToken>
      <SecretAccessKey>wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY</SecretAccessKey>
      <Expiration>2026-10-13T12:15:00Z</Expiration>
      <AccessKeyId>ASIAEXAMPLE</AccessKeyId>
    </Credentials>
    <Provider>oidc.hatchrun.io</Provider>
  </AssumeRoleWithWebIdentityResult>
  <ResponseMetadata><RequestId>r</RequestId></ResponseMetadata>
</AssumeRoleWithWebIdentityResponse>"#
                .to_vec(),
        };
        assert_eq!(
            credentials(&response).unwrap(),
            Credentials {
                access_key_id: "ASIAEXAMPLE".to_owned(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_owned(),
                session_token: "FwoGZXIvYXdz+/=".to_owned(),
            }
        );
    }

    #[test]
    fn missing_fields_are_errors() {
        let response = Response {
            headers: Vec::new(),
            status: 200,
            body: b"<AssumeRoleWithWebIdentityResponse/>".to_vec(),
        };
        assert_eq!(
            credentials(&response).unwrap_err().message,
            "no AccessKeyId in AssumeRoleWithWebIdentity"
        );
    }
}
