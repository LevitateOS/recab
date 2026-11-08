//! Error classes and the CLI's stable exit codes. With `--json` a failed
//! command prints [`ErrorPayload`] on stderr, so front ends read the class
//! and code, never the message text.

use serde::Serialize;

pub const EXIT_CODE_GENERIC: u8 = 1;
pub const EXIT_CODE_PERMISSION: u8 = 13;
pub const EXIT_CODE_BACKEND_UNAVAILABLE: u8 = 20;
pub const EXIT_CODE_STATE_OR_DETECTION: u8 = 21;
pub const EXIT_CODE_PARTIAL_UPDATE: u8 = 22;
pub const EXIT_CODE_TRIAL_FAILED: u8 = 23;

/// The schema of [`ErrorPayload`].
pub const ERROR_SCHEMA_VERSION: u32 = 1;

#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Permission,
    BackendUnavailable,
    StateOrDetection,
    PartialUpdate,
    /// A trial boot was not healthy and was not committed.
    TrialFailed,
}

impl ErrorClass {
    pub const ALL: &'static [ErrorClass] = &[
        ErrorClass::Permission,
        ErrorClass::BackendUnavailable,
        ErrorClass::StateOrDetection,
        ErrorClass::PartialUpdate,
        ErrorClass::TrialFailed,
    ];

    /// The exit status of an error of this class.
    pub fn code(self) -> u8 {
        match self {
            ErrorClass::Permission => EXIT_CODE_PERMISSION,
            ErrorClass::BackendUnavailable => EXIT_CODE_BACKEND_UNAVAILABLE,
            ErrorClass::StateOrDetection => EXIT_CODE_STATE_OR_DETECTION,
            ErrorClass::PartialUpdate => EXIT_CODE_PARTIAL_UPDATE,
            ErrorClass::TrialFailed => EXIT_CODE_TRIAL_FAILED,
        }
    }

    /// The tag the text message starts with (`[RECAB_ERR_STATE]`).
    pub fn tag(self) -> &'static str {
        match self {
            ErrorClass::Permission => "RECAB_ERR_PERMISSION",
            ErrorClass::BackendUnavailable => "RECAB_ERR_BACKEND",
            ErrorClass::StateOrDetection => "RECAB_ERR_STATE",
            ErrorClass::PartialUpdate => "RECAB_ERR_PARTIAL_UPDATE",
            ErrorClass::TrialFailed => "RECAB_ERR_TRIAL_FAILED",
        }
    }
}

/// An error with a stable class. The class travels as a typed value in the
/// error chain, so the exit status never depends on message wording.
#[derive(Debug)]
struct ClassifiedError {
    class: ErrorClass,
    message: String,
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.class.tag(), self.message)
    }
}

impl std::error::Error for ClassifiedError {}

pub(crate) fn tagged_err(class: ErrorClass, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError {
        class,
        message: message.into(),
    })
}

/// The outermost classified error's class: a partial update caused by a
/// backend failure is a partial update.
pub fn error_class(err: &anyhow::Error) -> Option<ErrorClass> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedError>())
        .map(|classified| classified.class)
}

/// Stable exit-code mapping for recab CLI and callers.
pub fn stable_exit_code(err: &anyhow::Error) -> u8 {
    error_class(err).map_or(EXIT_CODE_GENERIC, ErrorClass::code)
}

/// A failed command's error, as `--json` prints it on stderr.
#[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
#[derive(Debug, Clone, Serialize)]
pub struct ErrorPayload {
    pub schema_version: u32,
    /// `None` for an error without a class (exit status 1).
    pub class: Option<ErrorClass>,
    /// The exit status.
    pub code: u8,
    pub message: String,
}

impl ErrorPayload {
    pub fn of(err: &anyhow::Error) -> Self {
        Self {
            schema_version: ERROR_SCHEMA_VERSION,
            class: error_class(err),
            code: stable_exit_code(err),
            message: format!("{err:#}"),
        }
    }
}
