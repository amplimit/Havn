/**
 * WebChat wire protocol — mirrors `havn-gateway::webchat::{ClientMessage,ServerMessage}`.
 * Keep aligned by hand: this file is the only client-side authority.
 */

export type ClientMessage = { type: "send"; content: string } | { type: "stop" };

export type ServerMessage =
  /** `broadcast`: sent on its own (heartbeat, cron), not a reply. */
  | { type: "agent_message"; content: string; broadcast?: boolean }
  | { type: "error"; message: string };
