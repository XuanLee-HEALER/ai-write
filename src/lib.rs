//! ai-write —— DeepSeek V4 辅助写作工具。
//!
//! 当前仅含最底层 `req` module:无状态 DeepSeek API wrapper。
//! 详见 `docs/req-module-design.md`。

pub mod req;

// The v0 collaborative-writing layers are synchronous (sync + `std::thread`),
// built on the blocking `req` client, so they are gated on the `blocking`
// feature.
#[cfg(feature = "blocking")]
pub mod engine;
#[cfg(feature = "blocking")]
pub mod session;
#[cfg(feature = "blocking")]
pub mod tool;
