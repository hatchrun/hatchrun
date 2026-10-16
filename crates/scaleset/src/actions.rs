// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! Calls to the Actions service.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use http::{Method, Request, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::{
    Error, JitConfig, Operation, Runner, RunnerGroup, ScaleSet, Secret, Session, decode, expect,
};

const API_VERSION: &str = "6.0-preview";
const SCALE_SETS: &str = "_apis/runtime/runnerscalesets";
const RUNNER_GROUPS: &str = "_apis/runtime/runnergroups/";
const RUNNERS: &str = "_apis/distributedtask/pools/0/agents";

/// Renew the connection this many seconds before its token expires.
const EXPIRY_MARGIN: u64 = 60;

/// A connection to the Actions service, from
/// [`crate::github::admin_connection`].
#[derive(Clone, Debug)]
pub struct Actions {
    url: Url,
    token: Secret,
    expires_at: u64,
}

impl Actions {
    /// Builds a connection from the service URL and admin token. The token
    /// is a JWT whose `exp` claim says when to renew the connection.
    ///
    /// # Errors
    ///
    /// [`Error::Url`] for a bad URL and [`Error::AdminToken`] for a token
    /// without a readable `exp` claim.
    pub fn new(url: &str, token: Secret) -> Result<Self, Error> {
        let expires_at = jwt_expiry(token.expose())?;
        Ok(Self {
            url: Url::parse(url)?,
            token,
            expires_at,
        })
    }

    /// Returns when the token expires, in seconds since the Unix epoch.
    #[must_use]
    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    /// Returns whether the connection needs renewing at `now`, in seconds
    /// since the Unix epoch. The crate does not read the clock, which does
    /// not exist on every target.
    #[must_use]
    pub fn expires_soon(&self, now: u64) -> bool {
        now.saturating_add(EXPIRY_MARGIN) >= self.expires_at
    }

    /// Looks up a runner group by name. `None` when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn runner_group(&self, name: &str) -> Result<Operation<Option<RunnerGroup>>, Error> {
        let request = self.request(Method::GET, RUNNER_GROUPS, &[("groupName", name)], None)?;
        Ok(Operation::new(request, |response| {
            single(expect(response, StatusCode::OK)?, "runner group")
        }))
    }

    /// Looks up a scale set by runner group and name. `None` when there is
    /// none.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn scale_set(
        &self,
        runner_group_id: u64,
        name: &str,
    ) -> Result<Operation<Option<ScaleSet>>, Error> {
        let group = runner_group_id.to_string();
        let query = [("runnerGroupId", group.as_str()), ("name", name)];
        let request = self.request(Method::GET, SCALE_SETS, &query, None)?;
        Ok(Operation::new(request, |response| {
            single(expect(response, StatusCode::OK)?, "scale set")
        }))
    }

    /// Gets a scale set by id.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn scale_set_by_id(&self, id: u64) -> Result<Operation<ScaleSet>, Error> {
        let request = self.request(Method::GET, &format!("{SCALE_SETS}/{id}"), &[], None)?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "scale set")
        }))
    }

    /// Creates a scale set. Names are unique within a runner group.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn create_scale_set(
        &self,
        scale_set: &crate::NewScaleSet,
    ) -> Result<Operation<ScaleSet>, Error> {
        let request = self.request(Method::POST, SCALE_SETS, &[], Some(&scale_set.body()))?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "scale set")
        }))
    }

    /// Replaces a scale set's name, group, labels and settings.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn update_scale_set(
        &self,
        id: u64,
        scale_set: &crate::NewScaleSet,
    ) -> Result<Operation<ScaleSet>, Error> {
        let path = format!("{SCALE_SETS}/{id}");
        let request = self.request(Method::PATCH, &path, &[], Some(&scale_set.body()))?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "scale set")
        }))
    }

    /// Deletes a scale set.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn delete_scale_set(&self, id: u64) -> Result<Operation<()>, Error> {
        let request = self.request(Method::DELETE, &format!("{SCALE_SETS}/{id}"), &[], None)?;
        Ok(Operation::new(request, no_content))
    }

    /// Registers a runner in the scale set and returns its single-use
    /// config.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn generate_jit_config(
        &self,
        scale_set_id: u64,
        runner_name: &str,
        work_folder: &str,
    ) -> Result<Operation<JitConfig>, Error> {
        let path = format!("{SCALE_SETS}/{scale_set_id}/generatejitconfig");
        let body = serde_json::json!({ "name": runner_name, "workFolder": work_folder });
        let request = self.request(Method::POST, &path, &[], Some(&body))?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "jit config")
        }))
    }

    /// Gets a runner by id.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn runner(&self, id: u64) -> Result<Operation<Runner>, Error> {
        let request = self.request(Method::GET, &format!("{RUNNERS}/{id}"), &[], None)?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "runner")
        }))
    }

    /// Looks up a runner by name. `None` when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn runner_by_name(&self, name: &str) -> Result<Operation<Option<Runner>>, Error> {
        let request = self.request(Method::GET, RUNNERS, &[("agentName", name)], None)?;
        Ok(Operation::new(request, |response| {
            single(expect(response, StatusCode::OK)?, "runner")
        }))
    }

    /// Removes a runner. Fails with [`crate::ErrorKind::JobStillRunning`]
    /// while the runner has a job.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn remove_runner(&self, id: u64) -> Result<Operation<()>, Error> {
        let request = self.request(Method::DELETE, &format!("{RUNNERS}/{id}"), &[], None)?;
        Ok(Operation::new(request, no_content))
    }

    /// Opens a message session on the scale set. A scale set has one
    /// session at a time; a second listener gets
    /// [`crate::ErrorKind::Conflict`].
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn create_session(
        &self,
        scale_set_id: u64,
        owner: &str,
    ) -> Result<Operation<Session>, Error> {
        let path = format!("{SCALE_SETS}/{scale_set_id}/sessions");
        let body = serde_json::json!({ "ownerName": owner });
        let request = self.request(Method::POST, &path, &[], Some(&body))?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "session")
        }))
    }

    /// Renews a session's message queue token.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn refresh_session(
        &self,
        scale_set_id: u64,
        session: &Session,
    ) -> Result<Operation<Session>, Error> {
        let path = format!("{SCALE_SETS}/{scale_set_id}/sessions/{}", session.id);
        let request = self.request(Method::PATCH, &path, &[], None)?;
        Ok(Operation::new(request, |response| {
            decode(expect(response, StatusCode::OK)?, "session")
        }))
    }

    /// Closes a session, so another listener can open one.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn delete_session(
        &self,
        scale_set_id: u64,
        session: &Session,
    ) -> Result<Operation<()>, Error> {
        let path = format!("{SCALE_SETS}/{scale_set_id}/sessions/{}", session.id);
        let request = self.request(Method::DELETE, &path, &[], None)?;
        Ok(Operation::new(request, no_content))
    }

    /// Acquires available jobs for the scale set and returns the request
    /// ids it got. Uses the session token, so an
    /// [`crate::ErrorKind::Unauthorized`] error means the session needs a
    /// refresh.
    ///
    /// # Errors
    ///
    /// [`Error::Request`] when the request cannot be built.
    pub fn acquire_jobs(
        &self,
        scale_set_id: u64,
        session: &Session,
        runner_request_ids: &[i64],
    ) -> Result<Operation<Vec<i64>>, Error> {
        let path = format!("{SCALE_SETS}/{scale_set_id}/acquirejobs");
        let body = serde_json::to_value(runner_request_ids).map_err(|source| Error::Decode {
            what: "job ids",
            source,
        })?;
        let mut request = self.request(Method::POST, &path, &[], Some(&body))?;
        request.headers_mut().insert(
            AUTHORIZATION,
            bearer(session.message_queue_access_token.expose())?,
        );
        Ok(Operation::new(request, |response| {
            let list: List<i64> = decode(expect(response, StatusCode::OK)?, "acquired jobs")?;
            Ok(list.value)
        }))
    }

    fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> Result<Request<Vec<u8>>, Error> {
        let mut url = Url::parse(&format!(
            "{}/{path}",
            self.url.as_str().trim_end_matches('/')
        ))?;
        url.query_pairs_mut()
            .extend_pairs(query)
            .append_pair("api-version", API_VERSION);
        let body = body.map(serde_json::Value::to_string).unwrap_or_default();
        Ok(Request::builder()
            .method(method)
            .uri(url.as_str())
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, bearer(self.token.expose())?)
            .header(USER_AGENT, crate::USER_AGENT)
            .body(body.into_bytes())?)
    }
}

pub(crate) fn bearer(token: &str) -> Result<http::HeaderValue, Error> {
    let mut value = http::HeaderValue::try_from(format!("Bearer {token}"))
        .map_err(|err| Error::Request(err.into()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// The Actions service wraps lists as `{"count": n, "value": [...]}`.
#[derive(Deserialize)]
struct List<T> {
    #[serde(default = "Vec::new")]
    value: Vec<T>,
}

fn single<T: DeserializeOwned>(body: &[u8], what: &'static str) -> Result<Option<T>, Error> {
    let mut list: List<T> = decode(body, what)?;
    match list.value.len() {
        0 => Ok(None),
        1 => Ok(list.value.pop()),
        _ => Err(Error::Ambiguous(what)),
    }
}

fn no_content(response: &http::Response<Vec<u8>>) -> Result<(), Error> {
    expect(response, StatusCode::NO_CONTENT).map(|_| ())
}

/// Reads the `exp` claim without checking the signature. The token is only
/// passed back to the service that signed it.
fn jwt_expiry(token: &str) -> Result<u64, Error> {
    #[derive(Deserialize)]
    struct Claims {
        exp: u64,
    }
    let payload = token
        .split('.')
        .nth(1)
        .ok_or(Error::AdminToken("not a JWT"))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| Error::AdminToken("payload is not base64url"))?;
    serde_json::from_slice::<Claims>(&payload)
        .map(|claims| claims.exp)
        .map_err(|_| Error::AdminToken("no exp claim"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tests::response;
    use crate::{ErrorKind, NewScaleSet};

    /// A JWT with `{"exp":1900000000}` as its payload.
    pub(crate) const ADMIN_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJleHAiOjE5MDAwMDAwMDB9.c2ln";

    pub(crate) fn actions() -> Actions {
        Actions::new(
            "https://pipelines.actions.githubusercontent.com/tenant/",
            Secret::new(ADMIN_TOKEN),
        )
        .unwrap()
    }

    pub(crate) fn session() -> Session {
        serde_json::from_str(
            r#"{"sessionId":"5f6e","ownerName":"scaler","messageQueueUrl":"https://broker.actions.githubusercontent.com/message","messageQueueAccessToken":"queue-token"}"#,
        )
        .unwrap()
    }

    fn header(request: &Request<Vec<u8>>, name: http::HeaderName) -> &str {
        request.headers()[name].to_str().unwrap()
    }

    #[test]
    fn reads_token_expiry() {
        let actions = actions();
        assert_eq!(actions.expires_at(), 1_900_000_000);
        assert!(!actions.expires_soon(1_899_999_000));
        assert!(actions.expires_soon(1_899_999_940));
    }

    #[test]
    fn rejects_tokens_without_expiry() {
        for token in ["opaque", "a.e30.c", "a.!!!.c"] {
            assert!(
                matches!(
                    Actions::new("https://x.example", Secret::new(token)),
                    Err(Error::AdminToken(_))
                ),
                "{token}"
            );
        }
    }

    #[test]
    fn requests_carry_api_version_and_admin_token() {
        let op = actions().scale_set(3, "linux x64").unwrap();
        assert_eq!(
            op.request.uri().to_string(),
            "https://pipelines.actions.githubusercontent.com/tenant/_apis/runtime/runnerscalesets?runnerGroupId=3&name=linux+x64&api-version=6.0-preview"
        );
        assert_eq!(
            header(&op.request, AUTHORIZATION),
            format!("Bearer {ADMIN_TOKEN}")
        );
        assert!(op.request.headers()[AUTHORIZATION].is_sensitive());
        assert_eq!(header(&op.request, USER_AGENT), crate::USER_AGENT);
    }

    #[test]
    fn scale_set_lookup() {
        let parser = actions().scale_set(1, "x").unwrap().parser;
        assert!(
            parser
                .parse(&response(200, r#"{"count":0,"value":[]}"#))
                .unwrap()
                .is_none()
        );
        let set = parser
            .parse(&response(
                200,
                r#"{"count":1,"value":[{"id":9,"name":"x","runnerGroupId":1,"labels":[{"name":"x","type":"System"}]}]}"#,
            ))
            .unwrap()
            .unwrap();
        assert_eq!((set.id, set.labels.len()), (9, 1));
        assert!(matches!(
            parser.parse(&response(200, r#"{"count":2,"value":[{},{}]}"#)),
            Err(Error::Ambiguous("scale set"))
        ));
    }

    #[test]
    fn create_scale_set_body() {
        let op = actions()
            .create_scale_set(&NewScaleSet {
                name: "x".into(),
                runner_group_id: 1,
                labels: vec!["linux".into()],
                disable_update: false,
            })
            .unwrap();
        assert_eq!(op.request.method(), Method::POST);
        let body: serde_json::Value = serde_json::from_slice(op.request.body()).unwrap();
        assert_eq!(body["labels"][0]["name"], "linux");
    }

    #[test]
    fn jit_config() {
        let op = actions()
            .generate_jit_config(9, "runner-1", "_work")
            .unwrap();
        assert_eq!(
            op.request.uri().path(),
            "/tenant/_apis/runtime/runnerscalesets/9/generatejitconfig"
        );
        let config = op
            .parser
            .parse(&response(
                200,
                r#"{"runner":{"id":4,"name":"runner-1","runnerScaleSetId":9},"encodedJITConfig":"abc"}"#,
            ))
            .unwrap();
        assert_eq!(config.runner.id, 4);
        assert_eq!(config.encoded_jit_config.expose(), "abc");
        assert!(!format!("{config:?}").contains("abc"));
    }

    #[test]
    fn remove_runner_reports_running_job() {
        let parser = actions().remove_runner(4).unwrap().parser;
        parser.parse(&response(204, "")).unwrap();
        let err = parser
            .parse(&response(
                400,
                r#"{"typeName":"JobStillRunningException","message":"busy"}"#,
            ))
            .unwrap_err();
        assert_eq!(err.kind(), Some(ErrorKind::JobStillRunning));
    }

    #[test]
    fn sessions() {
        let actions = actions();
        let op = actions.create_session(9, "scaler").unwrap();
        assert_eq!(op.request.body(), br#"{"ownerName":"scaler"}"#);
        let created = op
            .parser
            .parse(&response(
                200,
                r#"{"sessionId":"5f6e","messageQueueUrl":"https://q","messageQueueAccessToken":"t","statistics":{"totalAssignedJobs":2}}"#,
            ))
            .unwrap();
        assert_eq!(created.statistics.unwrap().total_assigned_jobs, 2);

        let refresh = actions.refresh_session(9, &created).unwrap();
        assert_eq!(refresh.request.method(), Method::PATCH);
        assert!(refresh.request.uri().path().ends_with("/9/sessions/5f6e"));

        let delete = actions.delete_session(9, &created).unwrap();
        assert_eq!(delete.request.method(), Method::DELETE);
        assert!(matches!(
            delete.parser.parse(&response(409, "")).unwrap_err().kind(),
            Some(ErrorKind::Conflict)
        ));
    }

    #[test]
    fn acquire_jobs_uses_session_token() {
        let op = actions().acquire_jobs(9, &session(), &[11, 12]).unwrap();
        assert_eq!(header(&op.request, AUTHORIZATION), "Bearer queue-token");
        assert_eq!(op.request.body(), b"[11,12]");
        assert_eq!(
            op.parser
                .parse(&response(200, r#"{"count":1,"value":[11]}"#))
                .unwrap(),
            [11]
        );
        assert_eq!(
            op.parser.parse(&response(401, "")).unwrap_err().kind(),
            Some(ErrorKind::Unauthorized)
        );
    }

    #[test]
    fn runner_group_lookup() {
        let op = actions().runner_group("Default").unwrap();
        assert_eq!(
            op.request.uri().path(),
            "/tenant/_apis/runtime/runnergroups/"
        );
        let group = op
            .parser
            .parse(&response(
                200,
                r#"{"count":1,"value":[{"id":1,"name":"Default","isDefaultGroup":true}]}"#,
            ))
            .unwrap()
            .unwrap();
        assert!(group.is_default);
    }
}
