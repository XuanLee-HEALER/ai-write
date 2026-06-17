//! Shared response types for the utility endpoints (`/models`, `/user/balance`).

use serde::Deserialize;

/// One model entry returned by `GET /models`.
///
/// The `id` is kept as a raw string rather than [`Model`](crate::req::model::Model)
/// so that a newly-released model the crate does not yet know about still
/// deserializes cleanly.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    /// The model identifier, e.g. `"deepseek-v4-flash"`.
    pub id: String,
    /// Object type, always `"model"`.
    #[serde(default)]
    pub object: String,
    /// Owning organization, e.g. `"deepseek"`.
    #[serde(default)]
    pub owned_by: String,
}

/// Account balance returned by `GET /user/balance`.
///
/// This reflects *remaining funds* and lags real usage; do not gate requests on
/// it (rely on HTTP 402 instead). Per-request cost should be computed from the
/// `usage` token counts, not from this value.
#[derive(Debug, Clone, Deserialize)]
pub struct Balance {
    /// Whether the account currently has balance available for API calls.
    pub is_available: bool,
    /// Per-currency balance details.
    #[serde(default)]
    pub balance_infos: Vec<BalanceInfo>,
}

/// A per-currency balance breakdown within [`Balance`].
///
/// Amounts are kept as the raw strings returned by the API; convert to a decimal
/// type upstream if arithmetic is needed.
#[derive(Debug, Clone, Deserialize)]
pub struct BalanceInfo {
    /// Currency code, `"CNY"` or `"USD"`.
    pub currency: String,
    /// Total available balance (granted plus topped-up).
    pub total_balance: String,
    /// Unexpired granted balance.
    pub granted_balance: String,
    /// Topped-up (paid) balance.
    pub topped_up_balance: String,
}
