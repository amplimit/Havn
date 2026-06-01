/**
 * WebChat wire protocol — mirrors `havn-gateway::webchat::{ClientMessage,ServerMessage}`.
 * Keep aligned by hand: this file is the only client-side authority.
 */

export type ClientMessage = { type: "send"; content: string };

export type ServerMessage =
  | { type: "agent_message"; content: string }
  | { type: "error"; message: string };
