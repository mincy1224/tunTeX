//! Error types and the exit codes the proxy uses.
//!
//! The exit code contract is the important part: when the remote compiler
//! actually ran, its exit code is returned unchanged, and these values are used
//! *only* for the proxy's own failures.  A build system that inspects `$?`
//! must never mistake a proxy failure for a LaTeX failure.

use std::fmt;

/// Proxy-internal exit codes, in the spirit of `sysexits.h`.
pub mod exit {
    /// The remote compiler ran; its own exit code is returned instead.
    pub const OK: i32 = 0;
    /// Configuration or usage error: bad workspace, path outside the workspace.
    pub const CONFIG: i32 = 64;
    /// The remote service could not be reached.
    pub const UNAVAILABLE: i32 = 69;
    /// Internal software error.
    pub const SOFTWARE: i32 = 70;
    /// Protocol or I/O error: malformed response, archive failure.
    pub const PROTOCOL: i32 = 74;
    /// Authentication or permission failure.
    pub const AUTH: i32 = 77;
    /// The proxy itself gave up waiting.
    pub const TIMEOUT: i32 = 124;
}

/// Which class of failure occurred, and therefore which exit code to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Config,
    Unavailable,
    Software,
    Protocol,
    Auth,
    Timeout,
}

impl Kind {
    /// The process exit code for this kind of failure.
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::Config => exit::CONFIG,
            Kind::Unavailable => exit::UNAVAILABLE,
            Kind::Software => exit::SOFTWARE,
            Kind::Protocol => exit::PROTOCOL,
            Kind::Auth => exit::AUTH,
            Kind::Timeout => exit::TIMEOUT,
        }
    }

    /// A short label used when printing the error.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Config => "configuration error",
            Kind::Unavailable => "service unavailable",
            Kind::Software => "internal error",
            Kind::Protocol => "protocol error",
            Kind::Auth => "authentication error",
            Kind::Timeout => "timeout",
        }
    }
}

/// The proxy's error type.
///
/// `message` is what the user reads and should always be actionable; `source`
/// carries the underlying cause and is only surfaced in debug mode.
#[derive(Debug)]
pub struct Error {
    kind: Kind,
    message: String,
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            source: None,
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::new(Kind::Config, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(Kind::Unavailable, message)
    }

    pub fn software(message: impl Into<String>) -> Self {
        Self::new(Kind::Software, message)
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(Kind::Protocol, message)
    }

    pub fn auth(message: impl Into<String>) -> Self {
        Self::new(Kind::Auth, message)
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::new(Kind::Timeout, message)
    }

    /// Attach an underlying cause, shown only in debug mode.
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn exit_code(&self) -> i32 {
        self.kind.exit_code()
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// The cause chain, innermost last, for debug output.
    pub fn chain(&self) -> Vec<String> {
        let mut links = Vec::new();
        let mut current: Option<&(dyn std::error::Error + 'static)> = self
            .source
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn std::error::Error + 'static));
        while let Some(error) = current {
            links.push(error.to_string());
            current = error.source();
        }
        links
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|boxed| boxed.as_ref() as &(dyn std::error::Error + 'static))
    }
}

/// Convenience alias used throughout the client.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_documented_table() {
        assert_eq!(Kind::Config.exit_code(), 64);
        assert_eq!(Kind::Unavailable.exit_code(), 69);
        assert_eq!(Kind::Software.exit_code(), 70);
        assert_eq!(Kind::Protocol.exit_code(), 74);
        assert_eq!(Kind::Auth.exit_code(), 77);
        assert_eq!(Kind::Timeout.exit_code(), 124);
    }

    #[test]
    fn source_chain_is_reported_in_order() {
        #[derive(Debug)]
        struct Inner;
        impl fmt::Display for Inner {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "inner cause")
            }
        }
        impl std::error::Error for Inner {}

        let error = Error::protocol("outer message").with_source(Inner);
        assert_eq!(error.message(), "outer message");
        assert_eq!(error.chain(), vec!["inner cause".to_string()]);
    }

    #[test]
    fn errors_without_a_source_have_an_empty_chain() {
        assert!(Error::config("plain").chain().is_empty());
    }
}
