//! The one SQL shape for an agent's workspace capability: the workspace owner holds
//! every capability; anyone else needs an explicit `workspace_agent_capabilities`
//! grant of that token. A missing workspace grants nothing.

use sqlx::{Executor, Sqlite};

/// `(w.owner_agent_id = ? OR EXISTS (grant of $token))` for a workspace aliased `w`.
/// Binds: agent, agent.
macro_rules! capability_predicate {
    ($token:literal) => {
        concat!(
            "(w.owner_agent_id = ? OR EXISTS (SELECT 1 FROM workspace_agent_capabilities c \
             WHERE c.workspace_id = w.id AND c.agent_id = ? AND c.capability = '",
            $token,
            "'))"
        )
    };
}
pub(crate) use capability_predicate;

const HAS_CAPABILITY: &str = "SELECT EXISTS (SELECT 1 FROM workspaces w WHERE w.id = ? AND \
     (w.owner_agent_id = ? OR EXISTS (SELECT 1 FROM workspace_agent_capabilities c \
     WHERE c.workspace_id = w.id AND c.agent_id = ? AND c.capability = ?)))";

pub(crate) async fn agent_has_workspace_capability<'e, X>(
    executor: X,
    workspace: &str,
    agent: &str,
    capability: &str,
) -> Result<bool, sqlx::Error>
where
    X: Executor<'e, Database = Sqlite>,
{
    let granted: i64 = sqlx::query_scalar(HAS_CAPABILITY)
        .bind(workspace)
        .bind(agent)
        .bind(agent)
        .bind(capability)
        .fetch_one(executor)
        .await?;
    Ok(granted != 0)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Production reads of the grant table outside this module must be listed here
    /// with the reason they are not the owner-or-grant capability check.
    const ALLOWED: &[&str] = &[
        "foundation/capability.rs",
        // Grant administration: list/replace a member's grants.
        "identity_access/access/membership.rs",
        // Witness read that returns the grant row itself.
        "conversation_messaging/messaging/collaboration/attachment/agent.rs",
        // Conversation-level rule: direct-pair endpoint or explicit grant, no owner arm.
        "conversation_messaging/messaging/collaboration/membership.rs",
    ];

    fn production(source: &str) -> &str {
        source
            .find("#[cfg(test)]\nmod ")
            .map_or(source, |end| &source[..end])
    }

    fn visit(root: &Path, dir: &Path, offenders: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, offenders);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if path.extension().is_none_or(|ext| ext != "rs")
                || relative.ends_with("tests.rs")
                || ALLOWED.contains(&relative.as_str())
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            if production(&source).contains("FROM workspace_agent_capabilities") {
                offenders.push(relative);
            }
        }
    }

    #[test]
    fn capability_reads_go_through_the_shared_predicate() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        visit(&root, &root, &mut offenders);
        assert!(
            offenders.is_empty(),
            "use foundation::capability instead of reading workspace_agent_capabilities: {offenders:?}"
        );
    }
}
