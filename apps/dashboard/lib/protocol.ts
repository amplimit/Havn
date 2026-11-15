/**
 * WebChat wire protocol — mirrors `havn-gateway::webchat::{ClientMessage,ServerMessage}`.
 * Keep aligned by hand: this file is the only client-side authority.
 */

export type ClientMessage =
  /** `conversation`: the chat it continues, a new one when the id is new. */
  | { type: "send"; content: string; conversation?: string }
  | { type: "stop"; conversation?: string }
  | { type: "decide"; id: string; approve: boolean; remember: boolean };

export type ServerMessage =
  /** `broadcast`: sent on its own (heartbeat, cron), not a reply; `conversation`: the chat it answers. */
  | { type: "agent_message"; content: string; broadcast?: boolean; conversation?: string }
  | { type: "error"; message: string }
  /** A tool call waits for the owner's approval; `conversation` when it came from a channel. */
  | {
      type: "approval";
      id: string;
      tool: string;
      input: string;
      reason: string;
      conversation?: string;
      /** `network`: a question of the egress proxy about a domain. */
      kind?: string;
    }
  /** The approval request was decided elsewhere or ended. */
  | { type: "approval_closed"; id: string };
