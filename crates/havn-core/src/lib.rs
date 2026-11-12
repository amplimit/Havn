//! Shared types for havn — IDs, messages, policy, errors.
//!
//! This crate has no I/O. Use it from any other havn crate that needs to
//! describe agents, messages, or policy without pulling in async runtime
//! or networking dependencies.

pub mod error;
pub mod ids;
pub mod message;
pub mod policy;

pub use error::{Error, Result};
pub use ids::{
    AgentId, ChannelBindingId, CredentialId, CronJobId, RoleId, SkillId, TeamId, UserId,
};
pub use message::{InboundMessage, MessageContent, OutboundMessage};
pub use policy::{
    AdminVisibility, BudgetExhaustAction, BudgetPolicy, ChannelAllowance, ContextToolsetEntry,
    ContextToolsets, HookPolicy, MAX_HOOK_PATTERN_LEN, MAX_HOOK_PATTERNS, MAX_HOOK_RULES,
    McpServerConfig, NetworkPolicy, Permissions, Policy, ResourceLimits, ToolHookRule,
    compile_hook_pattern,
};
