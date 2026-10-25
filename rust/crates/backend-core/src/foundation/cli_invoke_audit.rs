//! Typed facts for the backend-side `cli.invoke` audit stream.
//!
//! The HTTP adapter decides when one invocation starts and ends. This module owns only
//! the closed, privacy-safe fact it may hand to a sink. In particular, validator reports,
//! request bodies, raw paths, query values, and arbitrary field names have no type here.

use crate::{
    foundation::cli_preflight_audit::NonNegativeFiniteF64,
    foundation::identity::{AgentId, ConversationId},
};

/// The only input coordinate the coarse validation taxonomy currently publishes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliInvokeIssuePath {
    Body,
}

impl CliInvokeIssuePath {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Body => "body",
        }
    }
}

/// Stable input classifications. These are deliberately not Serde/Garde error tags.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliInvokeIssueCode {
    UnsupportedMediaType,
    MalformedJson,
    InvalidStructure,
    InvalidValue,
}

impl CliInvokeIssueCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::MalformedJson => "malformed_json",
            Self::InvalidStructure => "invalid_structure",
            Self::InvalidValue => "invalid_value",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliInvokeIssue {
    pub path: CliInvokeIssuePath,
    pub code: CliInvokeIssueCode,
}

/// A bounded issue collection. The private representation makes the 16-entry ceiling
/// impossible to bypass: producers can only enter through [`Self::new`], which applies
/// the one deterministic first-16 policy before a [`CliInvoke`] can exist.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CliInvokeIssues(Vec<CliInvokeIssue>);

impl CliInvokeIssues {
    pub const MAX_LEN: usize = 16;

    pub fn new(mut issues: Vec<CliInvokeIssue>) -> Self {
        issues.truncate(Self::MAX_LEN);
        Self(issues)
    }

    pub fn as_slice(&self) -> &[CliInvokeIssue] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl CliInvokeIssue {
    pub const fn body(code: CliInvokeIssueCode) -> Self {
        Self {
            path: CliInvokeIssuePath::Body,
            code,
        }
    }
}

/// Stable failure buckets. A more specific typed response marker wins over the HTTP
/// status fallback, but neither arm can carry a human error sentence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliInvokeErrorCode {
    InvalidInput,
    Unauthorized,
    Forbidden,
    NotFound,
    Conflict,
    PayloadTooLarge,
    InternalError,
}

impl CliInvokeErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::Conflict => "conflict",
            Self::PayloadTooLarge => "payload_too_large",
            Self::InternalError => "internal_error",
        }
    }
}

/// A response body's machine `code` (e.g. `target_unresolved`). Only a short
/// snake_case token is accepted, so free text can never pass as a code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliRefusalCode(String);

impl CliRefusalCode {
    pub const MAX_LEN: usize = 64;

    pub fn parse(raw: &str) -> Option<Self> {
        let mut chars = raw.chars();
        let valid = raw.len() <= Self::MAX_LEN
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        valid.then(|| Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliInvokeErrorKind {
    Panic,
}

impl CliInvokeErrorKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Panic => "panic",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliInvokeMethod {
    Get,
    Post,
    Patch,
    Delete,
    Put,
    Head,
    Options,
    Other,
}

impl CliInvokeMethod {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Put => "PUT",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Other => "OTHER",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CliInvokeRoute {
    pub noun: &'static str,
    pub verb: &'static str,
    pub route_suffix: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CliStaleResolutionReason {
    Unaffected,
    Revised,
    Obsolete,
    ForceSend,
    Unattributed,
}

impl CliStaleResolutionReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unaffected => "unaffected",
            Self::Revised => "revised",
            Self::Obsolete => "obsolete",
            Self::ForceSend => "force_send",
            Self::Unattributed => "unattributed",
        }
    }
}

/// One completed authenticated internal-agent request.
#[derive(Clone, Debug, PartialEq)]
pub struct CliInvoke {
    pub request_id: String,
    pub agent_id: AgentId,
    pub route: Option<CliInvokeRoute>,
    pub method: CliInvokeMethod,
    pub query_keys: Vec<&'static str>,
    pub http_status: u16,
    pub duration_ms: NonNegativeFiniteF64,
    pub error_code: Option<CliInvokeErrorCode>,
    pub refusal_code: Option<CliRefusalCode>,
    pub error_issues: CliInvokeIssues,
    /// The conversation the request resolved to act on, when the route reports one.
    pub conversation_id: Option<ConversationId>,
    pub stale_resolution_reason: Option<CliStaleResolutionReason>,
    pub error_kind: Option<CliInvokeErrorKind>,
}

pub trait CliInvokeAuditSink: Clone + Send + Sync + 'static {
    /// Best-effort, non-blocking recording; sink health never becomes an HTTP result.
    fn record(&self, invoke: CliInvoke);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusal_code_accepts_only_snake_case_tokens() {
        assert!(CliRefusalCode::parse("target_unresolved").is_some());
        assert!(CliRefusalCode::parse("e2").is_some());
        for bad in ["", "Target", "_x", "has space", "a-b", &"a".repeat(65)] {
            assert!(CliRefusalCode::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn issue_value_object_deterministically_keeps_only_the_first_sixteen() {
        let input: Vec<_> = (0..17)
            .map(|index| {
                CliInvokeIssue::body(if index == 16 {
                    CliInvokeIssueCode::InvalidValue
                } else {
                    CliInvokeIssueCode::InvalidStructure
                })
            })
            .collect();
        let issues = CliInvokeIssues::new(input);

        assert_eq!(issues.as_slice().len(), 16);
        assert!(
            issues
                .as_slice()
                .iter()
                .all(|issue| issue.code == CliInvokeIssueCode::InvalidStructure),
            "the seventeenth entry cannot enter the bounded fact"
        );
    }
}
