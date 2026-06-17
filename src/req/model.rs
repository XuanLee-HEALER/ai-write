//! The set of DeepSeek models this crate can request.

use std::fmt;

/// A DeepSeek model usable in a chat completion request.
///
/// This enum is the crate's canonical allow-list of request-side models and is
/// **extended with each release** as DeepSeek ships new models. It is marked
/// `#[non_exhaustive]` so that adding a variant is a backward-compatible change:
/// downstream `match` expressions are required to carry a wildcard arm.
///
/// The enum is only used where the *client chooses* a model. Model identifiers
/// *observed* from the server — [`ModelInfo::id`](crate::req::types::common::ModelInfo::id)
/// and [`ChatResponse::model`](crate::req::types::response::ChatResponse::model) —
/// are kept as plain `String`, so a model the enum does not yet know about never
/// causes a deserialization failure.
///
/// # Examples
///
/// ```
/// use ai_write::req::model::Model;
///
/// assert_eq!(Model::V4Flash.as_str(), "deepseek-v4-flash");
/// assert_eq!(Model::V4Pro.to_string(), "deepseek-v4-pro");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Model {
    /// `deepseek-v4-flash` — fast and inexpensive; thinking mode on by default.
    V4Flash,
    /// `deepseek-v4-pro` — stronger and pricier; thinking mode on by default.
    V4Pro,
}

impl Model {
    /// The wire identifier sent in the request's `model` field.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Model::V4Flash => "deepseek-v4-flash",
            Model::V4Pro => "deepseek-v4-pro",
        }
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl serde::Serialize for Model {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}
