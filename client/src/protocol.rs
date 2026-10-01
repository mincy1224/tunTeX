//! The wire format, client side.
//!
//! The layout constants here must match `protocol.md` exactly; the server
//! refuses an archive whose members are not in the documented places.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Wire-format version understood by this client.
pub const PROTOCOL_VERSION: u32 = 2;

/// Where the request metadata lives inside the request archive.
pub const REQUEST_META_NAME: &str = "meta/request.json";

/// Prefix inside the request archive holding the uploaded workspace.
pub const REQUEST_WORKSPACE_PREFIX: &str = "workspace";

/// Where the result metadata lives inside the result archive.
pub const RESULT_META_NAME: &str = "meta/result.json";

/// Member names carrying the backend's raw output.
pub const RESULT_STDOUT_NAME: &str = "stdout.bin";
pub const RESULT_STDERR_NAME: &str = "stderr.bin";

/// Prefix inside the result archive holding changed files.
pub const RESULT_FILES_PREFIX: &str = "files";

/// HTTP header names used by the protocol.
pub const HEADER_PROTOCOL: &str = "X-TunTeX-Protocol";
pub const HEADER_REQUEST_ID: &str = "X-TunTeX-Request-Id";

/// The media type of both request and result bodies.
pub const CONTENT_TYPE: &str = "application/gzip";

/// `meta/request.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RequestMetadata {
    pub protocol: u32,
    pub request_id: String,
    pub engine: String,
    pub argv: Vec<String>,
    pub cwd: String,
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: u64,
    #[serde(default)]
    pub source_manifest: BTreeMap<String, String>,
}

impl RequestMetadata {
    pub fn new(
        request_id: impl Into<String>,
        engine: impl Into<String>,
        argv: Vec<String>,
        cwd: impl Into<String>,
        env: BTreeMap<String, String>,
        timeout_seconds: u64,
    ) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            request_id: request_id.into(),
            engine: engine.into(),
            argv,
            cwd: cwd.into(),
            env,
            timeout_seconds,
            source_manifest: BTreeMap::new(),
        }
    }

    /// Serialise to the bytes stored at `meta/request.json`.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self)
            .map_err(|error| Error::software(format!("could not encode request metadata: {error}")))
    }
}

/// `meta/result.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMetadata {
    #[serde(default)]
    pub remote_workspace: String,
    pub protocol: u32,
    pub request_id: String,
    pub exit_code: i32,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub changed: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
}

impl ResultMetadata {
    /// Parse `meta/result.json`.
    pub fn from_bytes(raw: &[u8]) -> Result<Self> {
        serde_json::from_slice(raw)
            .map_err(|error| Error::protocol(format!("result metadata is not valid JSON: {error}")))
    }

    /// Reject a result that does not belong to this request.
    ///
    /// The request id is the only thing tying a response to its request, so a
    /// mismatch means the response cannot be trusted -- and must not be applied.
    pub fn validate_for(&self, expected_request_id: &str) -> Result<()> {
        if self.protocol != PROTOCOL_VERSION {
            return Err(Error::protocol(format!(
                "server answered with protocol {}, this client speaks {}",
                self.protocol, PROTOCOL_VERSION
            )));
        }
        if self.request_id != expected_request_id {
            return Err(Error::protocol(format!(
                "server answered with request id {}, expected {};\n\
                 refusing to apply a result that belongs to a different request",
                self.request_id, expected_request_id
            )));
        }
        Ok(())
    }
}

/// The JSON body of an HTTP-level error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub message: String,
}

impl ErrorBody {
    pub fn parse(raw: &[u8]) -> Option<Self> {
        serde_json::from_slice(raw).ok()
    }

    /// A readable one-line summary.
    pub fn describe(&self) -> String {
        match (self.error.is_empty(), self.message.is_empty()) {
            (false, false) => format!("{}: {}", self.error, self.message),
            (false, true) => self.error.clone(),
            (true, false) => self.message.clone(),
            (true, true) => "the server reported an error without any detail".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(request_id: &str, protocol: u32) -> ResultMetadata {
        ResultMetadata {
            remote_workspace: String::new(),
            protocol,
            request_id: request_id.to_string(),
            exit_code: 0,
            timed_out: false,
            cancelled: false,
            duration_ms: 1,
            changed: Vec::new(),
            deleted: Vec::new(),
        }
    }

    #[test]
    fn request_metadata_round_trips() {
        let mut env = BTreeMap::new();
        env.insert("SOURCE_DATE_EPOCH".to_string(), "0".to_string());
        let metadata = RequestMetadata::new(
            "abc",
            "latexmk",
            vec!["-pdf".to_string(), "/workspace/main.tex".to_string()],
            "/workspace",
            env,
            120,
        );

        let bytes = metadata.to_bytes().unwrap();
        let parsed: RequestMetadata = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(parsed, metadata);
        assert_eq!(parsed.protocol, PROTOCOL_VERSION);
    }

    #[test]
    fn a_matching_result_is_accepted() {
        assert!(result("abc", PROTOCOL_VERSION).validate_for("abc").is_ok());
    }

    #[test]
    fn a_mismatched_request_id_is_refused() {
        let error = result("other", PROTOCOL_VERSION)
            .validate_for("abc")
            .unwrap_err();
        assert_eq!(error.exit_code(), 74);
        assert!(error.message().contains("different request"));
    }

    #[test]
    fn an_unsupported_protocol_is_refused() {
        let error = result("abc", 99).validate_for("abc").unwrap_err();
        assert!(error.message().contains("protocol"));
    }

    #[test]
    fn a_non_zero_exit_code_is_not_an_error() {
        let mut metadata = result("abc", PROTOCOL_VERSION);
        metadata.exit_code = 12;
        assert!(metadata.validate_for("abc").is_ok());
        assert_eq!(metadata.exit_code, 12);
    }

    #[test]
    fn malformed_result_metadata_is_a_protocol_error() {
        let error = ResultMetadata::from_bytes(b"{not json").unwrap_err();
        assert_eq!(error.exit_code(), 74);
    }

    #[test]
    fn missing_optional_fields_default_safely() {
        let raw = br#"{"protocol":1,"request_id":"abc","exit_code":0}"#;
        let metadata = ResultMetadata::from_bytes(raw).unwrap();
        assert!(!metadata.timed_out);
        assert!(!metadata.cancelled);
        assert!(metadata.changed.is_empty());
        assert!(metadata.deleted.is_empty());
    }

    #[test]
    fn error_bodies_describe_themselves() {
        let body = ErrorBody::parse(br#"{"error":"unauthorized","message":"bad token"}"#).unwrap();
        assert_eq!(body.describe(), "unauthorized: bad token");
    }

    #[test]
    fn unparseable_error_bodies_are_handled() {
        assert!(ErrorBody::parse(b"<html>").is_none());
        let empty = ErrorBody {
            error: String::new(),
            message: String::new(),
        };
        assert!(!empty.describe().is_empty());
    }
}
