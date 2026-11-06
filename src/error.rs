//! Error classes and the CLI's stable exit codes.

pub const EXIT_CODE_GENERIC: u8 = 1;
pub const EXIT_CODE_PERMISSION: u8 = 13;
pub const EXIT_CODE_BACKEND_UNAVAILABLE: u8 = 20;
pub const EXIT_CODE_STATE_OR_DETECTION: u8 = 21;
pub const EXIT_CODE_PARTIAL_UPDATE: u8 = 22;
pub const EXIT_CODE_TRIAL_FAILED: u8 = 23;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorClass {
    Permission,
    BackendUnavailable,
    StateOrDetection,
    PartialUpdate,
    /// A trial boot was not healthy and was not committed.
    TrialFailed,
}

impl ErrorClass {
    fn code(self) -> u8 {
        match self {
            ErrorClass::Permission => EXIT_CODE_PERMISSION,
            ErrorClass::BackendUnavailable => EXIT_CODE_BACKEND_UNAVAILABLE,
            ErrorClass::StateOrDetection => EXIT_CODE_STATE_OR_DETECTION,
            ErrorClass::PartialUpdate => EXIT_CODE_PARTIAL_UPDATE,
            ErrorClass::TrialFailed => EXIT_CODE_TRIAL_FAILED,
        }
    }

    fn tag(self) -> &'static str {
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

/// Stable exit-code mapping for recab CLI and callers: the outermost classified
/// error decides, so a partial update caused by a backend failure reports the
/// partial update.
pub fn stable_exit_code(err: &anyhow::Error) -> u8 {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedError>())
        .map(|classified| classified.class.code())
        .unwrap_or(EXIT_CODE_GENERIC)
}
