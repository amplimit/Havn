/**
 * WebSocket client for the WebChat endpoint (spec §3.2 + §11).
 *
 * The dashboard's `server.mjs` custom Next server proxies WebSocket
 * upgrades for `/ws/*` to the loopback-only gateway, so the browser
 * always opens WS to the dashboard's own origin. This keeps the
 * gateway out of the public network surface and means the same code
 * works whether the user accesses the dashboard via SSH tunnel,
 * direct public IP, or behind a real reverse proxy.
 *
 * `NEXT_PUBLIC_HAVN_GATEWAY_WS` overrides for the rare case where you
 * want the browser to bypass the dashboard and hit the gateway
 * directly (e.g. a separate reverse proxy already exposes the
 * gateway under its own DNS name).
 */

import type { ServerMessage, ClientMessage, Attachment } from "./protocol";

export type ChatHandlers = {
  onOpen?: () => void;
  onMessage: (msg: ServerMessage) => void;
  onError?: (err: Event) => void;
  onClose?: (ev: CloseEvent) => void;
};

export type ChatHandle = {
  /** Send `text` in chat `conversation`, or the active chat; with `edit`,
   * as a new version of that user turn. */
  send: (text: string, conversation?: string, edit?: string, attachments?: Attachment[]) => void;
  /** Ask for another reply in place of reply `turn`. */
  regenerate: (conversation: string | undefined, turn: string) => void;
  /** Stop the reply the agent is working on in a chat; queued messages still run. */
  stop: (conversation?: string) => void;
  /** Answer an approval request. */
  decide: (id: string, approve: boolean, remember: boolean) => void;
  close: () => void;
};

function resolveGatewayUrl(): string {
  const override = process.env.NEXT_PUBLIC_HAVN_GATEWAY_WS;
  if (override) return override;
  if (typeof window !== "undefined" && window.location.host) {
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    return `${proto}//${window.location.host}`;
  }
  // SSR fallback — never actually reached because `openChat` is always
  // called from a `"use client"` page, but TS needs the branch.
  return "ws://127.0.0.1:3000";
}

export function openChat(
  agentId: string,
  token: string,
  handlers: ChatHandlers,
): ChatHandle {
  const base = resolveGatewayUrl();
  const url = `${base}/ws/chat/${encodeURIComponent(agentId)}?token=${encodeURIComponent(token)}`;
  const ws = new WebSocket(url);

  ws.addEventListener("open", () => handlers.onOpen?.());
  ws.addEventListener("message", (ev) => {
    try {
      const parsed = JSON.parse(ev.data) as ServerMessage;
      handlers.onMessage(parsed);
    } catch {
      // Malformed frame — surface as an error message to the UI.
      handlers.onMessage({
        type: "error",
        message: `malformed server frame: ${String(ev.data).slice(0, 80)}`,
      });
    }
  });
  ws.addEventListener("error", (ev) => handlers.onError?.(ev));
  ws.addEventListener("close", (ev) => handlers.onClose?.(ev));

  return {
    send: (text: string, conversation?: string, edit?: string, attachments?: Attachment[]) => {
      const frame: ClientMessage = {
        type: "send",
        content: text,
        conversation,
        edit,
        attachments: attachments?.length ? attachments : undefined,
      };
      ws.send(JSON.stringify(frame));
    },
    regenerate: (conversation, turn) => {
      const frame: ClientMessage = { type: "regenerate", conversation, turn };
      ws.send(JSON.stringify(frame));
    },
    stop: (conversation?: string) => {
      const frame: ClientMessage = { type: "stop", conversation };
      ws.send(JSON.stringify(frame));
    },
    decide: (id, approve, remember) => {
      const frame: ClientMessage = { type: "decide", id, approve, remember };
      ws.send(JSON.stringify(frame));
    },
    close: () => ws.close(),
  };
}
