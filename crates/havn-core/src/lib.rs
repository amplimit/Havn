//! Shared types for havn — IDs, messages, policy, errors.
//!
//! This crate has no I/O. Use it from any other havn crate that needs to
//! describe agents, messages, or policy without pulling in async runtime
//! or networking dependencies.

pub mod calendar;
pub mod error;
pub mod ids;
pub mod message;
pub mod policy;

pub use error::{Error, Result};
pub use ids::{
    AgentId, ChannelBindingId, CredentialId, CronJobId, RoleId, SkillId, TeamId, UserId,
};
pub use message::{
    Attachment, Branch, Button, INBOX_DIR, InboundMessage, MAX_BUTTON_DATA, MessageContent,
    OutboundMessage, Poll,
};
pub use policy::{
    AGENT_CGROUP_PARENT, AGENT_CGROUP_PREFIX, AdminVisibility, BudgetExhaustAction, BudgetPolicy,
    ChannelAllowance, ChatTitles, ContextToolsetEntry, ContextToolsets, HOOK_REGEX_SIZE_LIMIT,
    HookPolicy, MAX_HOOK_PATTERN_LEN, MAX_HOOK_PATTERNS, MAX_HOOK_RULES, McpServerConfig,
    NetworkPolicy, Permissions, Policy, ResourceLimits, ToolHookRule, agent_cgroup,
    compile_hook_pattern,
};

/// The language of what Havn writes for an agent (issue #250): the parts of
/// the prompt it adds, and the texts it sends people without the model.
/// Tools stay English. Set by the agent's owner as `config.language`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Language {
    #[default]
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh")]
    Chinese,
}

impl Language {
    /// `en` or `zh`, whichever is this language's.
    pub fn pick<T>(self, en: T, zh: T) -> T {
        match self {
            Self::English => en,
            Self::Chinese => zh,
        }
    }

    /// The text of a `[English, Chinese]` pair in this language.
    #[must_use]
    pub fn of(self, texts: [&'static str; 2]) -> &'static str {
        self.pick(texts[0], texts[1])
    }

    /// The language an agent's `config` sets; English when it sets none
    /// or one Havn does not have.
    #[must_use]
    pub fn of_config(config: &serde_json::Value) -> Self {
        config
            .get("language")
            .cloned()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod language_tests {
    #![allow(clippy::expect_used)]

    use super::Language;

    #[test]
    fn an_agent_s_language_comes_from_its_config_and_defaults_to_english() {
        let zh = serde_json::json!({"language": "zh"});
        assert_eq!(Language::of_config(&zh), Language::Chinese);
        assert_eq!(
            Language::of_config(&serde_json::json!({})),
            Language::English
        );
        assert_eq!(
            Language::of_config(&serde_json::json!({"language": "fr"})),
            Language::English
        );
        assert_eq!(Language::Chinese.pick("a", "b"), "b");
        assert_eq!(serde_json::to_value(Language::English).expect("json"), "en");
    }
}
