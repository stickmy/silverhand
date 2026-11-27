//! Per-agent environment variables injected into the runtime child process.
//!
//! Values may carry credentials and are strictly owner-private: only the agent's
//! human creator may read or write them via the management API, and the backend
//! delivers them exclusively to the owning host daemon on `runtime_start`.

use std::{collections::HashSet, future::Future};

use crate::foundation::{identity::AgentId, persistence::PersistenceFailure};

pub const MAX_AGENT_ENV_VARS: usize = 64;
pub const MAX_AGENT_ENV_VAR_NAME_UTF16: usize = 128;
pub const MAX_AGENT_ENV_VAR_VALUE_UTF16: usize = 8192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentEnvVar {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentEnvValidationError {
    TooManyEntries,
    InvalidName,
    DuplicateName,
    InvalidValue,
}

pub fn is_valid_agent_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == b'_') {
        return false;
    }
    if name.encode_utf16().count() > MAX_AGENT_ENV_VAR_NAME_UTF16 {
        return false;
    }
    bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

pub fn validate_agent_env_vars(vars: &[AgentEnvVar]) -> Result<(), AgentEnvValidationError> {
    if vars.len() > MAX_AGENT_ENV_VARS {
        return Err(AgentEnvValidationError::TooManyEntries);
    }
    let mut seen = HashSet::with_capacity(vars.len());
    for var in vars {
        if !is_valid_agent_env_name(&var.name) {
            return Err(AgentEnvValidationError::InvalidName);
        }
        if !seen.insert(var.name.as_str()) {
            return Err(AgentEnvValidationError::DuplicateName);
        }
        if var.value.contains('\0')
            || var.value.encode_utf16().count() > MAX_AGENT_ENV_VAR_VALUE_UTF16
        {
            return Err(AgentEnvValidationError::InvalidValue);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentEnvWriteOutcome {
    NoSuchAgent,
    Replaced(Vec<AgentEnvVar>),
}

pub trait AgentEnvStore: Clone + Send + Sync + 'static {
    fn read_agent_env(
        &self,
        agent: &AgentId,
    ) -> impl Future<Output = Result<Vec<AgentEnvVar>, PersistenceFailure>> + Send;

    fn replace_agent_env(
        &self,
        agent: &AgentId,
        vars: &[AgentEnvVar],
    ) -> impl Future<Output = Result<AgentEnvWriteOutcome, PersistenceFailure>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, value: &str) -> AgentEnvVar {
        AgentEnvVar {
            name: name.to_owned(),
            value: value.to_owned(),
        }
    }

    #[test]
    fn accepts_valid_entries_including_acp_auto_compact() {
        let vars = vec![
            entry("SILVERHAND_ACP_AUTO_COMPACT", "200000"),
            entry("_FOO_1", ""),
            entry("a", "hello=world"),
        ];
        assert_eq!(validate_agent_env_vars(&vars), Ok(()));
    }

    #[test]
    fn rejects_invalid_names_duplicates_nul_and_limits() {
        assert_eq!(
            validate_agent_env_vars(&[entry("", "1")]),
            Err(AgentEnvValidationError::InvalidName)
        );
        assert_eq!(
            validate_agent_env_vars(&[entry("1ABC", "1")]),
            Err(AgentEnvValidationError::InvalidName)
        );
        assert_eq!(
            validate_agent_env_vars(&[entry("FOO-BAR", "1")]),
            Err(AgentEnvValidationError::InvalidName)
        );
        assert_eq!(
            validate_agent_env_vars(&[entry("FOO", "1"), entry("FOO", "2")]),
            Err(AgentEnvValidationError::DuplicateName)
        );
        assert_eq!(
            validate_agent_env_vars(&[entry("FOO", "a\0b")]),
            Err(AgentEnvValidationError::InvalidValue)
        );
        let too_many: Vec<_> = (0..=MAX_AGENT_ENV_VARS)
            .map(|i| entry(&format!("VAR_{i}"), "x"))
            .collect();
        assert_eq!(
            validate_agent_env_vars(&too_many),
            Err(AgentEnvValidationError::TooManyEntries)
        );
    }
}
