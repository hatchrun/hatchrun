// SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
//
// SPDX-License-Identifier: AGPL-3.0-only

//! The agent's calls to the control plane: claim the job, send event
//! batches and report the result (see [`hatchrun_workflow::agent`]).

use std::fmt;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hatchrun_workflow::agent::{Batch, BatchReply, CLAIM_PATH, EVENTS_PATH, RESULT_PATH};
use hatchrun_workflow::{JobDocument, JobResult};
use reqwest::{StatusCode, Url};
use tracing::{debug, warn};

use crate::broker::https_url;

/// What a claim answers.
#[derive(Debug)]
pub enum Claim {
    /// The job to run.
    Job(Box<JobDocument>),
    /// The job already ended or was replaced: there is nothing to run.
    Ended,
}

/// The control plane refused the call for good: the token is not
/// accepted (`401`), the job ended or was reset (`409`), or the request
/// can never succeed. The agent stops sending.
#[derive(Debug)]
pub struct Stopped(pub StatusCode);

impl fmt::Display for Stopped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the control plane answered {}", self.0)
    }
}

impl std::error::Error for Stopped {}

/// A client for one job, holding the job token.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    token: String,
    /// How long a claim keeps asking while the job is not ready.
    pub claim_timeout: Duration,
    /// The pause between claim and result attempts.
    pub retry_pause: Duration,
    /// How long a result is retried.
    pub result_deadline: Duration,
}

/// How a response sorts for the retry loops.
enum Answer {
    Ok(reqwest::Response),
    /// `401`, `409` or any other answer that retrying cannot change.
    Stopped(StatusCode),
    /// A `5xx`, `408` or `429`, or no answer at all.
    Retry(String),
}

impl Client {
    /// A client for the control plane at `url`. The token travels in
    /// every request, so the URL must be HTTPS.
    pub fn new(url: &str, token: String) -> Result<Self> {
        let base = https_url(url, "control plane")?;
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .context("build control plane client")?;
        Ok(Self {
            http,
            base,
            token,
            claim_timeout: Duration::from_secs(600),
            retry_pause: Duration::from_secs(1),
            result_deadline: Duration::from_secs(300),
        })
    }

    fn url(&self, path: &str) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| anyhow!("control plane URL {} cannot have a path", self.base))?
            .pop_if_empty()
            .extend(path.trim_start_matches('/').split('/'));
        Ok(url)
    }

    async fn post(&self, path: &str, body: Option<Vec<u8>>) -> Result<Answer> {
        let mut request = self.http.post(self.url(path)?).bearer_auth(&self.token);
        if let Some(body) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) if err.is_connect() || err.is_timeout() || err.is_request() => {
                return Ok(Answer::Retry(err.without_url().to_string()));
            }
            Err(err) => return Err(err.without_url()).with_context(|| format!("POST {path}")),
        };
        let status = response.status();
        Ok(if status.is_success() {
            Answer::Ok(response)
        } else if status.is_server_error()
            || status == StatusCode::TOO_MANY_REQUESTS
            || status == StatusCode::REQUEST_TIMEOUT
        {
            Answer::Retry(format!("answered {status}"))
        } else {
            Answer::Stopped(status)
        })
    }

    /// Asks for the job until the control plane has it ready. `503` and
    /// connection errors are retried every [`Client::retry_pause`] for up
    /// to [`Client::claim_timeout`]. A repeated claim returns the same
    /// document.
    pub async fn claim(&self) -> Result<Claim> {
        let deadline = tokio::time::Instant::now() + self.claim_timeout;
        loop {
            match self.post(CLAIM_PATH, None).await? {
                Answer::Ok(response) => {
                    let doc = response.json().await.context("read the job document")?;
                    return Ok(Claim::Job(Box::new(doc)));
                }
                Answer::Stopped(StatusCode::CONFLICT) => return Ok(Claim::Ended),
                Answer::Stopped(status) => bail!("claim the job: {}", Stopped(status)),
                Answer::Retry(why) => debug!("job not ready: {why}"),
            }
            if tokio::time::Instant::now() + self.retry_pause > deadline {
                bail!("the job was not ready after {:?}", self.claim_timeout);
            }
            tokio::time::sleep(self.retry_pause).await;
        }
    }

    /// Sends one batch. One try: the caller keeps the batch and sends it
    /// again on an error. A [`Stopped`] error means stop sending.
    pub async fn events(&self, batch: &Batch) -> Result<BatchReply> {
        let body = serde_json::to_vec(batch).context("serialize the batch")?;
        match self.post(EVENTS_PATH, Some(body)).await? {
            Answer::Ok(response) => response.json().await.context("read the batch reply"),
            Answer::Stopped(status) => Err(Stopped(status).into()),
            Answer::Retry(why) => bail!("send events: {why}"),
        }
    }

    /// Reports the result, retrying for up to [`Client::result_deadline`].
    /// A job that ended or was reset (`409`) no longer wants one, which
    /// is not an error.
    pub async fn result(&self, result: &JobResult) -> Result<()> {
        let body = serde_json::to_vec(result).context("serialize the result")?;
        let deadline = tokio::time::Instant::now() + self.result_deadline;
        loop {
            let why = match self.post(RESULT_PATH, Some(body.clone())).await? {
                Answer::Ok(_) => return Ok(()),
                Answer::Stopped(StatusCode::CONFLICT) => {
                    warn!("the job ended or was reset, the result is not wanted");
                    return Ok(());
                }
                Answer::Stopped(status) => bail!("report the result: {}", Stopped(status)),
                Answer::Retry(why) => why,
            };
            if tokio::time::Instant::now() + self.retry_pause > deadline {
                bail!(
                    "report the result: {why}, gave up after {:?}",
                    self.result_deadline
                );
            }
            warn!("report the result: {why}, trying again");
            tokio::time::sleep(self.retry_pause).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    use std::collections::BTreeMap;

    use super::*;

    /// Answers 503 `failures` times, then `then`.
    struct Busy {
        failures: usize,
        seen: AtomicUsize,
        then: ResponseTemplate,
    }

    impl Respond for Busy {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            if self.seen.fetch_add(1, Ordering::SeqCst) < self.failures {
                ResponseTemplate::new(503).insert_header("Retry-After", "1")
            } else {
                self.then.clone()
            }
        }
    }

    fn client(server: &MockServer) -> Client {
        let mut client = Client::new(&server.uri(), "tok".into()).unwrap();
        client.retry_pause = Duration::from_millis(5);
        client.claim_timeout = Duration::from_secs(5);
        client.result_deadline = Duration::from_secs(5);
        client
    }

    pub(crate) fn job_json() -> serde_json::Value {
        use hatchrun_workflow::{Github, JobKind, PlannedJob, template};
        let doc = JobDocument {
            run_id: "7".into(),
            run_number: 3,
            run_attempt: 1,
            workflow: "ci".into(),
            job: PlannedJob {
                id: "build".into(),
                name: "build".into(),
                requires: Vec::new(),
                kind: JobKind::Run {
                    template: template("ubuntu-latest").unwrap(),
                },
                matrix: BTreeMap::new(),
                node: None,
                env: BTreeMap::new(),
                if_: None,
                timeout_minutes: 10,
                outputs: BTreeMap::new(),
                steps: Vec::new(),
            },
            event_name: "push".into(),
            event: serde_json::json!({}),
            github: Github {
                repository: "acme/repo".into(),
                repository_owner: "acme".into(),
                sha: "abc".into(),
                ref_: "refs/heads/main".into(),
                ref_name: "main".into(),
                ref_type: "branch".into(),
                head_ref: String::new(),
                base_ref: String::new(),
                actor: "octo".into(),
                server_url: "https://github.com".into(),
                api_url: "https://api.github.com".into(),
                graphql_url: "https://api.github.com/graphql".into(),
                token: "tok".into(),
            },
            needs: BTreeMap::new(),
            secrets: BTreeMap::new(),
            vars: BTreeMap::new(),
            timings: BTreeMap::new(),
            cache: None,
        };
        serde_json::to_value(doc).unwrap()
    }

    #[tokio::test]
    async fn claim_retries_until_ready() {
        let server = MockServer::start().await;
        job_doc(&server).await;
        let claim = client(&server).claim().await.unwrap();
        assert!(matches!(claim, Claim::Job(job) if job.run_id == "7"));
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    async fn job_doc(server: &MockServer) {
        let doc = job_json();
        Mock::given(method("POST"))
            .and(path(CLAIM_PATH))
            .and(header("authorization", "Bearer tok"))
            .respond_with(Busy {
                failures: 2,
                seen: AtomicUsize::new(0),
                then: ResponseTemplate::new(200).set_body_json(&doc),
            })
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn ended_job_is_not_run() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(CLAIM_PATH))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        assert!(matches!(
            client(&server).claim().await.unwrap(),
            Claim::Ended
        ));
    }

    #[tokio::test]
    async fn claim_gives_up_at_the_timeout() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let mut client = client(&server);
        client.claim_timeout = Duration::from_millis(50);
        let err = client.claim().await.unwrap_err();
        assert!(err.to_string().contains("not ready"), "{err:#}");
    }

    #[tokio::test]
    async fn claim_refused_with_401_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        assert!(client(&server).claim().await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn events_sort_the_answers() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(EVENTS_PATH))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"cancel": 1})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(EVENTS_PATH))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(EVENTS_PATH))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let client = client(&server);
        let batch = Batch {
            seq: 1,
            events: Vec::new(),
        };
        assert_eq!(
            client.events(&batch).await.unwrap(),
            BatchReply { cancel: 1 }
        );
        let err = client.events(&batch).await.unwrap_err();
        assert!(err.downcast_ref::<Stopped>().is_none(), "{err:#}");
        let err = client.events(&batch).await.unwrap_err();
        assert_eq!(
            err.downcast_ref::<Stopped>().unwrap().0,
            StatusCode::CONFLICT
        );
    }

    fn result() -> JobResult {
        JobResult {
            conclusion: hatchrun_workflow::Conclusion::Success,
            outputs: BTreeMap::new(),
            steps: Vec::new(),
            annotations: Vec::new(),
        }
    }

    #[tokio::test]
    async fn result_retries_transient_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(RESULT_PATH))
            .respond_with(Busy {
                failures: 2,
                seen: AtomicUsize::new(0),
                then: ResponseTemplate::new(200),
            })
            .mount(&server)
            .await;
        client(&server).result(&result()).await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn result_for_an_ended_job_is_fine_and_a_401_is_not() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(409))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = client(&server);
        client.result(&result()).await.unwrap();
        assert!(client.result(&result()).await.is_err());
    }

    #[test]
    fn plain_http_is_refused() {
        assert!(Client::new("http://ci.example.com", "t".into()).is_err());
    }
}
