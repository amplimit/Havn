/**
 * WebChat wire protocol — mirrors `havn-gateway::webchat::{ClientMessage,ServerMessage}`.
 * Keep aligned by hand: this file is the only client-side authority.
 */

export type ClientMessage =
  /** `conversation`: the chat it continues, a new one when the id is new;
   * `edit`: a user turn this is a new version of. */
  | { type: "send"; content: string; conversation?: string; edit?: string; attachments?: Attachment[] }
  /** Another reply in place of reply `turn`, or of the last one. */
  | { type: "regenerate"; conversation?: string; turn?: string }
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
  | { type: "approval_closed"; id: string }
  /** What the reply just sent saved to or forgot from memory. */
  | { type: "memory"; conversation?: string; turn?: string; changes: MemoryChange[] };

/** A file uploaded to the agent's inbox for a message. */
export type Attachment = {
  path: string;
  name: string;
  media_type: string;
  size: number;
  /** Where its text is, when it was extracted (a PDF). */
  text_path?: string;
};

export type MemoryChange = {
  action: "saved" | "forgot";
  key: string;
  /** What was saved, or what the forgotten entry said. */
  value: string;
  kind: string;
  /** The entry the change replaced or forgot, and the one it left
   * active: what undoing it puts back and takes out. */
  before?: string;
  after?: string;
};
