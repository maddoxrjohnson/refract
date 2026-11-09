//! Which coding agent a session or launch belongs to: Claude Code, or Codex CLI as the
//! overflow agent (see `.plans/codex-spec.md`).

use serde::{Deserialize, Serialize};

/// A coding agent. On the wire: `"claude"` or `"codex"`. Fields of this type default to
/// Claude and are left out when Claude, so messages from before Codex read the same.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentKind {
    #[default]
    Claude,
    Codex,
}

impl AgentKind {
    /// The agent's name as the person reads it: `Claude` or `Codex`.
    pub fn name(self) -> &'static str {
        match self {
            AgentKind::Claude => "Claude",
            AgentKind::Codex => "Codex",
        }
    }

    /// The other agent.
    pub fn other(self) -> AgentKind {
        match self {
            AgentKind::Claude => AgentKind::Codex,
            AgentKind::Codex => AgentKind::Claude,
        }
    }

    /// For `skip_serializing_if`.
    pub fn is_claude(&self) -> bool {
        *self == AgentKind::Claude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_go_on_the_wire_in_camel_case() {
        assert_eq!(
            serde_json::to_value(AgentKind::Codex).unwrap(),
            serde_json::json!("codex")
        );
        assert_eq!(
            serde_json::from_value::<AgentKind>(serde_json::json!("claude")).unwrap(),
            AgentKind::Claude
        );
        assert_eq!(AgentKind::default(), AgentKind::Claude);
        assert_eq!(AgentKind::Claude.other(), AgentKind::Codex);
        assert_eq!(AgentKind::Codex.name(), "Codex");
    }
}
