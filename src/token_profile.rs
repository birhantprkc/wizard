//! Token profile: how much of the harness goes into every request.
//!
//! Selected by `WIZARD_TOKEN_PROFILE`. `stock` (the default) is the harness
//! as it has always been. Each later profile includes everything the one
//! before it does:
//!
//! - `safe` fixes skill frontmatter parsing, sends the subagent roster as
//!   names only, sends a continuous run's mission once per compaction, stubs
//!   an identical re-read of an unchanged file, and stops repeating a
//!   subagent result the completion note already delivered.
//! - `lean` defers every tool outside the everyday set behind `tool_search`,
//!   digests old tool-call arguments, and caps `execute` output at 12 KB.
//! - `min` cuts the system prompt and core tool schemas to the bone.

use std::sync::OnceLock;

/// Environment variable that picks the profile.
pub const ENV: &str = "WIZARD_TOKEN_PROFILE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum TokenProfile {
    #[default]
    Stock,
    Safe,
    Lean,
    Min,
}

impl TokenProfile {
    /// Parse a profile name. Unknown names are `None`.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "" | "stock" => Some(Self::Stock),
            "safe" => Some(Self::Safe),
            "lean" => Some(Self::Lean),
            "min" => Some(Self::Min),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Safe => "safe",
            Self::Lean => "lean",
            Self::Min => "min",
        }
    }

    /// `safe` and everything above it.
    pub fn safe(self) -> bool {
        self >= Self::Safe
    }

    /// `lean` and everything above it.
    pub fn lean(self) -> bool {
        self >= Self::Lean
    }

    /// `min` only.
    pub fn min(self) -> bool {
        self >= Self::Min
    }
}

static CURRENT: OnceLock<TokenProfile> = OnceLock::new();

/// The process's profile, read from the environment once. An unknown name
/// falls back to `stock` with a warning, so a typo never changes behavior.
pub fn current() -> TokenProfile {
    *CURRENT.get_or_init(|| {
        let raw = std::env::var(ENV).unwrap_or_default();
        TokenProfile::parse(&raw).unwrap_or_else(|| {
            tracing::warn!("{ENV}={raw:?} is not stock, safe, lean or min; using stock");
            TokenProfile::Stock
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_orders_profiles() {
        assert_eq!(TokenProfile::parse(""), Some(TokenProfile::Stock));
        assert_eq!(TokenProfile::parse("Lean"), Some(TokenProfile::Lean));
        assert_eq!(TokenProfile::parse("nope"), None);
        assert!(TokenProfile::Min.lean() && TokenProfile::Min.safe());
        assert!(TokenProfile::Lean.safe() && !TokenProfile::Lean.min());
        assert!(!TokenProfile::Stock.safe());
    }
}
