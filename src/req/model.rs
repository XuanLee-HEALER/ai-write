//! The set of DeepSeek models this crate can request.

use std::fmt;
use std::str::FromStr;

/// A DeepSeek model usable in a chat completion request.
///
/// This enum is the crate's canonical allow-list of request-side models and is
/// **extended with each release** as DeepSeek ships new models. It is marked
/// `#[non_exhaustive]` so that adding a variant is a backward-compatible change:
/// downstream `match` expressions are required to carry a wildcard arm.
///
/// # Family variants vs. pinned snapshots
///
/// The named variants ([`Model::V4Flash`], [`Model::V4Pro`]) are *family*
/// aliases: they name a model line but not a dated snapshot, so the server is
/// free to roll them forward. Kernel §9 requires that authorship and
/// reproducibility be pinned to an **exact dated snapshot id** (a bare family
/// name "is meaningless for reproduction and attribution"). The
/// [`Model::Pinned`] escape hatch carries such an explicit id verbatim (e.g.
/// `"deepseek-v4-pro-2026-05-01"`), so a caller can pin a snapshot without this
/// crate having to mint a new named variant for every dated release. The pinned
/// id is what flows through to the writer identity, the git author, and the
/// file's contributor list, so the provenance names the precise model that
/// produced the text.
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
///
/// // A dated snapshot pinned for reproducibility / attribution (kernel §9).
/// let pinned = Model::pinned("deepseek-v4-pro-2026-05-01");
/// assert_eq!(pinned.as_str(), "deepseek-v4-pro-2026-05-01");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Model {
    /// `deepseek-v4-flash` — fast and inexpensive; thinking mode on by default.
    V4Flash,
    /// `deepseek-v4-pro` — stronger and pricier; thinking mode on by default.
    V4Pro,
    /// An explicit, caller-supplied model id — typically an exact dated snapshot
    /// such as `"deepseek-v4-pro-2026-05-01"`.
    ///
    /// This is the escape hatch for kernel §9 reproducible attribution: it lets a
    /// concrete snapshot id be sent on the wire and recorded as the author
    /// without the enum having to enumerate every dated release. Construct it via
    /// [`Model::pinned`] (which collapses a known family id back to its named
    /// variant) rather than building the variant directly, so equal ids always
    /// compare equal.
    Pinned(String),
}

impl Model {
    /// The wire identifier sent in the request's `model` field.
    ///
    /// Returns a borrow of `self`: a `'static` string for a named family variant,
    /// or the carried id for a [`Model::Pinned`] snapshot.
    pub fn as_str(&self) -> &str {
        match self {
            Model::V4Flash => "deepseek-v4-flash",
            Model::V4Pro => "deepseek-v4-pro",
            Model::Pinned(id) => id.as_str(),
        }
    }

    /// Builds a [`Model`] from an explicit id, collapsing a known family id back
    /// to its named variant.
    ///
    /// Use this to pin an exact dated snapshot (kernel §9): `"deepseek-v4-pro-2026-05-01"`
    /// becomes a [`Model::Pinned`] carrying that id, while a bare family id like
    /// `"deepseek-v4-pro"` normalizes to the corresponding named variant
    /// ([`Model::V4Pro`]). Normalizing on the way in means two callers that name
    /// the same model — one by family alias, one by the canonical string —
    /// produce equal [`Model`] values, so equality and provenance tags stay
    /// consistent.
    ///
    /// This never fails: any non-empty string is a valid model id as far as this
    /// crate is concerned (the server is the authority on which ids exist).
    ///
    /// # Examples
    ///
    /// ```
    /// use ai_write::req::model::Model;
    ///
    /// // A known family id collapses to its named variant.
    /// assert_eq!(Model::pinned("deepseek-v4-pro"), Model::V4Pro);
    /// // An unknown / dated id is carried verbatim.
    /// let snap = Model::pinned("deepseek-v4-pro-2026-05-01");
    /// assert!(matches!(snap, Model::Pinned(ref id) if id == "deepseek-v4-pro-2026-05-01"));
    /// ```
    pub fn pinned(id: impl Into<String>) -> Self {
        let id = id.into();
        match id.as_str() {
            "deepseek-v4-flash" => Model::V4Flash,
            "deepseek-v4-pro" => Model::V4Pro,
            _ => Model::Pinned(id),
        }
    }
}

impl FromStr for Model {
    type Err = std::convert::Infallible;

    /// Parses any model id into a [`Model`], normalizing known family ids to their
    /// named variants (see [`Model::pinned`]). Parsing is infallible.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Model::pinned(s))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_variants_have_stable_wire_ids() {
        assert_eq!(Model::V4Flash.as_str(), "deepseek-v4-flash");
        assert_eq!(Model::V4Pro.as_str(), "deepseek-v4-pro");
    }

    #[test]
    fn pinned_carries_dated_snapshot_verbatim() {
        let snap = Model::pinned("deepseek-v4-pro-2026-05-01");
        assert_eq!(snap.as_str(), "deepseek-v4-pro-2026-05-01");
        assert_eq!(snap.to_string(), "deepseek-v4-pro-2026-05-01");
        assert!(matches!(snap, Model::Pinned(id) if id == "deepseek-v4-pro-2026-05-01"));
    }

    #[test]
    fn pinned_collapses_known_family_ids_to_named_variants() {
        assert_eq!(Model::pinned("deepseek-v4-flash"), Model::V4Flash);
        assert_eq!(Model::pinned("deepseek-v4-pro"), Model::V4Pro);
        // Equality holds regardless of how the same model was named.
        assert_eq!(
            Model::pinned("deepseek-v4-pro"),
            "deepseek-v4-pro".parse::<Model>().unwrap()
        );
    }

    #[test]
    fn from_str_is_infallible_and_matches_pinned() {
        assert_eq!(
            "deepseek-v4-pro-2026-05-01".parse::<Model>().unwrap(),
            Model::pinned("deepseek-v4-pro-2026-05-01")
        );
        assert_eq!("deepseek-v4-pro".parse::<Model>().unwrap(), Model::V4Pro);
    }

    #[test]
    fn serializes_to_the_wire_id_for_both_kinds() {
        assert_eq!(
            serde_json::to_string(&Model::V4Pro).unwrap(),
            "\"deepseek-v4-pro\""
        );
        assert_eq!(
            serde_json::to_string(&Model::pinned("deepseek-v4-pro-2026-05-01")).unwrap(),
            "\"deepseek-v4-pro-2026-05-01\""
        );
    }
}
