//! Crate-wide error type and exit code mapping.

use std::fmt;

/// The crate-wide error type. Every fallible operation in this crate
/// resolves to one of these variants, which map directly to process exit
/// codes via [`Error::exit_code`].
#[derive(Debug)]
pub enum Error {
    /// A general, unstructured error (exit code 1).
    General(String),
    /// A subrange could not be allocated within after exhausting retries
    /// (exit code 2).
    SubrangeExhausted,
    /// The configured pool has no room left for a new subrange (exit code
    /// 3).
    PoolExhausted,
    /// Acquiring the registry lock timed out (exit code 4).
    LockTimeout,
    /// `portool exec` could not find the requested command (exit code
    /// 127, spec v0.4 §9).
    CommandNotFound(String),
    /// `portool exec` found the requested command but could not execute
    /// it, e.g. missing execute permission (exit code 126, spec v0.4 §9).
    CommandNotExecutable(String),
}

impl Error {
    /// Maps this error to the process exit code defined by the spec.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::General(_) => 1,
            Error::SubrangeExhausted => 2,
            Error::PoolExhausted => 3,
            Error::LockTimeout => 4,
            Error::CommandNotExecutable(_) => 126,
            Error::CommandNotFound(_) => 127,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::General(msg) => write!(f, "{msg}"),
            Error::SubrangeExhausted => write!(f, "subrange exhausted"),
            Error::PoolExhausted => write!(f, "pool exhausted"),
            Error::LockTimeout => write!(f, "lock timeout"),
            Error::CommandNotFound(cmd) => write!(f, "command not found: {cmd}"),
            Error::CommandNotExecutable(cmd) => write!(f, "cannot execute: {cmd}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::General(err.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::General(err.to_string())
    }
}

impl From<toml::de::Error> for Error {
    fn from(err: toml::de::Error) -> Self {
        Error::General(err.to_string())
    }
}

/// Convenience alias for `Result<T, Error>`.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_mapping() {
        assert_eq!(Error::General("x".into()).exit_code(), 1);
        assert_eq!(Error::SubrangeExhausted.exit_code(), 2);
        assert_eq!(Error::PoolExhausted.exit_code(), 3);
        assert_eq!(Error::LockTimeout.exit_code(), 4);
        assert_eq!(Error::CommandNotExecutable("x".into()).exit_code(), 126);
        assert_eq!(Error::CommandNotFound("x".into()).exit_code(), 127);
    }

    #[test]
    fn display_messages() {
        assert_eq!(Error::General("boom".into()).to_string(), "boom");
        assert_eq!(Error::SubrangeExhausted.to_string(), "subrange exhausted");
        assert_eq!(Error::PoolExhausted.to_string(), "pool exhausted");
        assert_eq!(Error::LockTimeout.to_string(), "lock timeout");
        assert_eq!(
            Error::CommandNotFound("nope".into()).to_string(),
            "command not found: nope"
        );
        assert_eq!(
            Error::CommandNotExecutable("nope".into()).to_string(),
            "cannot execute: nope"
        );
    }

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let err: Error = io_err.into();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("missing"));
    }

    #[test]
    fn from_serde_json_error() {
        let json_err = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
        let err: Error = json_err.into();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn from_toml_error() {
        let toml_err = "not = [valid".parse::<toml::Value>().unwrap_err();
        let err: Error = toml_err.into();
        assert_eq!(err.exit_code(), 1);
    }
}
