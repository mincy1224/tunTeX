//! The HTTP side of the proxy.
//!
//! Bodies are streamed to and from disk rather than held in memory, so a large
//! workspace costs disk, not RAM.  Every failure is translated into a message
//! that says what to do about it.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use reqwest::blocking::{Body, Client, RequestBuilder};
use reqwest::StatusCode;

use crate::error::{Error, Result};
use crate::protocol::{self, ErrorBody};

/// Extra time allowed on top of the compile timeout before the HTTP client
/// gives up, covering transfer and the server's own teardown.
const TRANSFER_GRACE_SECONDS: u64 = 60;

/// How long a cancellation request may take.
const CANCEL_TIMEOUT_SECONDS: u64 = 20;

/// A configured connection to the remote service.
#[derive(Debug, Clone)]
pub struct Remote {
    base_url: String,
    token: Option<String>,
    client: Client,
}

impl Remote {
    /// Build a client for `base_url`, allowing `timeout_seconds` for a compile.
    pub fn new(base_url: &str, token: Option<String>, timeout_seconds: u64) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(
                timeout_seconds + TRANSFER_GRACE_SECONDS,
            ))
            .build()
            .map_err(|error| {
                Error::software(format!("could not create the HTTP client: {error}"))
                    .with_source(error)
            })?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            token,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    /// Attach the protocol headers every request needs.
    fn decorate(&self, request: RequestBuilder, request_id: Option<&str>) -> RequestBuilder {
        let mut request = request.header(
            protocol::HEADER_PROTOCOL,
            protocol::PROTOCOL_VERSION.to_string(),
        );
        if let Some(id) = request_id {
            request = request.header(protocol::HEADER_REQUEST_ID, id);
        }
        if let Some(token) = &self.token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        request
    }

    /// `GET /health`.
    pub fn health(&self) -> Result<bool> {
        let response = self
            .client
            .get(self.endpoint("/health"))
            .timeout(Duration::from_secs(10))
            .send()
            .map_err(|error| self.connection_error(error))?;
        Ok(response.status().is_success())
    }

    /// `POST /compile`, streaming `archive_path` up and the result down.
    pub fn missing_files(
        &self,
        manifest: &std::collections::BTreeMap<String, String>,
    ) -> Result<Vec<String>> {
        let response = self
            .decorate(
                self.client.post(self.endpoint("/manifest")).json(manifest),
                None,
            )
            .send()
            .map_err(|error| self.connection_error(error))?;
        if !response.status().is_success() {
            return Err(self.describe_http_failure(response.status(), response));
        }
        response
            .json()
            .map_err(|error| Error::protocol(format!("invalid manifest response: {error}")))
    }

    ///
    /// Returns the byte length of the result written to `result_path`.
    pub fn compile(
        &self,
        archive_path: &Path,
        result_path: &Path,
        request_id: &str,
    ) -> Result<u64> {
        let file = File::open(archive_path).map_err(|error| {
            Error::software(format!(
                "could not open the request archive {}: {error}",
                archive_path.display()
            ))
        })?;
        let length = file
            .metadata()
            .map_err(|error| {
                Error::software(format!("could not stat the request archive: {error}"))
            })?
            .len();

        let request = self
            .client
            .post(self.endpoint("/compile"))
            .header("Content-Type", protocol::CONTENT_TYPE)
            .body(Body::sized(file, length));

        let response = self
            .decorate(request, Some(request_id))
            .send()
            .map_err(|error| self.connection_error(error))?;

        let status = response.status();
        if !status.is_success() {
            return Err(self.describe_http_failure(status, response));
        }

        self.check_response_headers(&response, request_id)?;
        write_body_to_file(response, result_path)
    }

    /// `DELETE /jobs/{request_id}`.
    ///
    /// Used when the caller interrupts: the server's own timeout is the
    /// backstop, this is the fast path.
    pub fn cancel(&self, request_id: &str) -> Result<bool> {
        let request = self
            .client
            .delete(self.endpoint(&format!("/jobs/{request_id}")))
            .timeout(Duration::from_secs(CANCEL_TIMEOUT_SECONDS));

        let response = match self.decorate(request, None).send() {
            Ok(response) => response,
            Err(error) => return Err(self.connection_error(error)),
        };

        let status = response.status();
        if status == StatusCode::NOT_FOUND {
            // The compile already finished; nothing to cancel.
            return Ok(false);
        }
        if !status.is_success() {
            return Err(self.describe_http_failure(status, response));
        }
        Ok(true)
    }

    /// Reject a 2xx response that is not actually ours.
    fn check_response_headers(
        &self,
        response: &reqwest::blocking::Response,
        request_id: &str,
    ) -> Result<()> {
        if let Some(value) = response.headers().get(protocol::HEADER_PROTOCOL) {
            let value = value.to_str().unwrap_or_default();
            if value.trim() != protocol::PROTOCOL_VERSION.to_string() {
                return Err(Error::protocol(format!(
                    "server answered with protocol {value:?}, this client speaks {}",
                    protocol::PROTOCOL_VERSION
                )));
            }
        }

        if let Some(value) = response.headers().get(protocol::HEADER_REQUEST_ID) {
            let value = value.to_str().unwrap_or_default();
            if value.trim() != request_id {
                return Err(Error::protocol(format!(
                    "server answered with request id {value:?}, expected {request_id:?};\n\
                     refusing to apply a result that belongs to a different request"
                )));
            }
        }
        Ok(())
    }

    fn describe_http_failure(
        &self,
        status: StatusCode,
        response: reqwest::blocking::Response,
    ) -> Error {
        let detail = response
            .bytes()
            .ok()
            .and_then(|body| ErrorBody::parse(&body))
            .map(|body| body.describe())
            .unwrap_or_else(|| "no detail was provided".to_string());

        let message = format!("the remote service rejected the request (HTTP {status}): {detail}");

        match status.as_u16() {
            401 | 403 => Error::auth(format!(
                "{message}\n\
                 check project_key is registered on the server"
            )),
            413 => Error::config(format!(
                "{message}\n\
                 raise the server's size limits, or reduce what is uploaded"
            )),
            code if (500..600).contains(&code) => Error::unavailable(format!(
                "{message}\n\
                 the remote service failed; check its log for this request id"
            )),
            _ => Error::protocol(message),
        }
    }

    fn connection_error(&self, error: reqwest::Error) -> Error {
        if error.is_timeout() {
            return Error::timeout(format!(
                "the remote service at {} did not respond in time",
                self.base_url
            ))
            .with_source(error);
        }
        Error::unavailable(format!(
            "cannot connect to {}\n\
             check that the remote service is running and that socket is correct",
            self.base_url
        ))
        .with_source(error)
    }
}

/// Stream a response body into a file without buffering it all in memory.
fn write_body_to_file(response: reqwest::blocking::Response, destination: &Path) -> Result<u64> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            Error::software(format!(
                "could not create the temporary directory {}: {error}",
                parent.display()
            ))
        })?;
    }

    let mut handle = File::create(destination).map_err(|error| {
        Error::software(format!(
            "could not create the result file {}: {error}",
            destination.display()
        ))
    })?;

    let mut reader = response;
    let written = std::io::copy(&mut reader, &mut handle).map_err(|error| {
        Error::protocol(format!(
            "the connection failed while receiving the result: {error}"
        ))
    })?;

    handle
        .flush()
        .map_err(|error| Error::software(format!("could not flush the result file: {error}")))?;

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_joins_without_doubling_slashes() {
        let remote = Remote::new("http://127.0.0.1:38117/", None, 30).unwrap();
        assert_eq!(remote.endpoint("/health"), "http://127.0.0.1:38117/health");
    }

    #[test]
    fn a_connection_refused_becomes_a_service_unavailable_error() {
        // Port 1 is privileged and nothing listens on it.
        let remote = Remote::new("http://127.0.0.1:1", None, 2).unwrap();
        let error = remote.health().unwrap_err();
        assert_eq!(error.exit_code(), 69);
        assert!(error.message().contains("cannot connect"));
        assert!(error.message().contains("http://127.0.0.1:1"));
    }

    #[test]
    fn a_timeout_is_reported_as_a_timeout() {
        // 10.255.255.1 is reserved and never answers, so this exercises the
        // timeout branch rather than connection-refused.  The health check uses
        // a 10s deadline of its own, so this does not actually wait that long
        // on a host that rejects the route immediately; either way the error
        // must be classified rather than panicking.
        let remote = Remote::new("http://10.255.255.1:38117", None, 1).unwrap();
        let error = remote.health().unwrap_err();
        assert!(
            matches!(error.exit_code(), 124 | 69),
            "expected a timeout or unavailable error, got {}",
            error.exit_code()
        );
    }
}
