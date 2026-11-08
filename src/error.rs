//! Error classes and the CLI's stable exit codes. With `--json` a failed
//! command prints [`ErrorPayload`] on stderr, so front ends read the class
//! and code, never the message text.
//!
//! Every module classifies its errors with [`classified`] or [`refuse`] from
//! here; an error without a class exits 1.

use serde::Serialize;

/// The exit status of an error without a class.
const EXIT_CODE_GENERIC: u8 = 1;

/// The schema of [`ErrorPayload`].
pub(crate) const ERROR_SCHEMA_VERSION: u32 = 1;

/// Each class once: its name (in JSON), exit status and text tag.
macro_rules! classes {
    ($($(#[$doc:meta])* $class:ident => $name:literal, $code:literal, $tag:literal;)+) => {
        #[cfg_attr(feature = "contracts", derive(schemars::JsonSchema, ts_rs::TS))]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        pub enum ErrorClass {
            $($(#[$doc])* #[serde(rename = $name)] $class,)+
        }

        impl ErrorClass {
            pub const ALL: &'static [ErrorClass] = &[$(ErrorClass::$class),+];

            /// The class as JSON spells it.
            pub fn name(self) -> &'static str {
                match self { $(ErrorClass::$class => $name,)+ }
            }

            /// The exit status of an error of this class.
            pub fn code(self) -> u8 {
                match self { $(ErrorClass::$class => $code,)+ }
            }

            /// The tag the text message starts with (`[RECAB_ERR_STATE]`).
            pub fn tag(self) -> &'static str {
                match self { $(ErrorClass::$class => $tag,)+ }
            }
        }
    };
}

classes! {
    /// Needs root.
    Permission => "permission", 13, "RECAB_ERR_PERMISSION";
    /// No UEFI or efivars, efivars not writable, or the running os-release
    /// has no distro profile.
    BackendUnavailable => "backend_unavailable", 20, "RECAB_ERR_BACKEND";
    /// Refused, or the slots or state could not be read.
    StateOrDetection => "state_or_detection", 21, "RECAB_ERR_STATE";
    /// Something changed and something did not; the message says what.
    PartialUpdate => "partial_update", 22, "RECAB_ERR_PARTIAL_UPDATE";
    /// A trial boot was not healthy and was not committed.
    TrialFailed => "trial_failed", 23, "RECAB_ERR_TRIAL_FAILED";
}

/// An error with a stable class. The class travels as a typed value in the
/// error chain, so the exit status never depends on message wording; the
/// message itself carries no tag.
#[derive(Debug)]
struct ClassifiedError {
    class: ErrorClass,
    message: String,
}

impl std::fmt::Display for ClassifiedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ClassifiedError {}

/// An error of `class`.
pub(crate) fn classified(class: ErrorClass, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ClassifiedError { class, message: message.into() })
}

/// A request recab refuses (exit 21): nothing was changed.
pub(crate) fn refuse(message: impl Into<String>) -> anyhow::Error {
    classified(ErrorClass::StateOrDetection, message)
}

/// `err` as it is when it already has a class, else given `class` (keeping
/// its whole chain as the message).
pub(crate) fn or_class(err: anyhow::Error, class: ErrorClass) -> anyhow::Error {
    if error_class(&err).is_some() {
        err
    } else {
        classified(class, format!("{err:#}"))
    }
}

/// The outermost classified error's class: a partial update caused by a
/// backend failure is a partial update.
pub(crate) fn error_class(err: &anyhow::Error) -> Option<ErrorClass> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<ClassifiedError>())
        .map(|classified| classified.class)
}

/// Stable exit-code mapping for recab CLI and callers.
pub fn stable_exit_code(err: &anyhow::Error) -> u8 {
    error_class(err).map_or(EXIT_CODE_GENERIC, ErrorClass::code)
}

/// A failed command's error as text: `[TAG] message` for a classified one.
pub fn error_text(err: &anyhow::Error) -> String {
    match error_class(err) {
        Some(class) => format!("[{}] {err:#}", class.tag()),
        None => format!("{err:#}"),
    }
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

    /// The payload as one JSON line.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("an ErrorPayload is plain data and always serializes")
    }
}
