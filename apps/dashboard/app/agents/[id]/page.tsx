"use client";

import { use, useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import {
  api,
  type AgentView,
  type ChatSummary,
  type ConversationTurn,
  type EmbeddingStatus,
  type MeView,
} from "@/lib/api";
import { openChat, type ChatHandle } from "@/lib/websocket";
import { Markdown } from "@/components/Markdown";
import type { ServerMessage } from "@/lib/protocol";
import {
  BookOpen,
  Brain,
  Check,
  Copy,
  Layers,
  MessageSquarePlus,
  MessagesSquare,
  Pencil,
  RotateCcw,
  Search,
  Settings,
  Trash2,
} from "lucide-react";

type ApprovalRequest = Extract<ServerMessage, { type: "approval" }>;

type Bubble = {
  id: number;
  role: "you" | "agent" | "system";
  content: string;
  ts: number;
};

const AGENT_POLL_MS = 5000;
const HISTORY_LIMIT = 100;
const RECONNECT_MS = 1500;

function turnBubbles(turns: ConversationTurn[], nextId: () => number): Bubble[] {
  return turns
    .filter((t) => t.role === "user" || t.role === "assistant")
    .map((t) => ({
      id: nextId(),
      role: t.role === "user" ? "you" : "agent",
      content: t.content,
      ts: new Date(t.created_at).getTime(),
    }));
}

// The id of a new chat: a random UUID, also where `crypto.randomUUID` is
// missing (pages served over plain http to another host).
function newChatId(): string {
  const b = crypto.getRandomValues(new Uint8Array(16));
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

// Allowance for the gap between the gateway's clock and this browser's.
const CLOCK_SKEW_MS = 60_000;

// The stored conversation, fetched from `since` on, replaces what is on
// screen, except notices (they exist only in this tab) and live bubbles
// newer than the fetch that it does not have yet.
function mergeStored(prev: Bubble[], stored: Bubble[], since: number): Bubble[] {
  const recent = stored.filter((s) => s.ts >= since - CLOCK_SKEW_MS);
  const kept = prev.filter(
    (b) =>
      b.role === "system" ||
      (b.ts >= since && !recent.some((s) => s.role === b.role && s.content === b.content)),
  );
  return [...stored, ...kept].sort((a, b) => a.ts - b.ts);
}

export default function ChatPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);

  const [agent, setAgent] = useState<AgentView | null>(null);
  const [me, setMe] = useState<MeView | null>(null);
  const [embedding, setEmbedding] = useState<EmbeddingStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [bubbles, setBubbles] = useState<Bubble[]>([]);
  const [draft, setDraft] = useState("");
  const [wsState, setWsState] = useState<
    "idle" | "connecting" | "open" | "closed" | "error"
  >("idle");
  // True from the moment the user hits send until the next agent
  // message (or error) arrives. Drives the composer's "replying…"
  // state and a small typing-dots ghost bubble so the user sees the
  // model is working — Anthropic calls take 1–3s and dead air feels
  // broken.
  // Replies still due: one per message sent, the running one first and
  // the rest queued behind it in the agent.
  const [waiting, setWaiting] = useState(0);
  const pending = waiting > 0;
  // Tool calls of the running reply that wait for the user.
  const [approvals, setApprovals] = useState<ApprovalRequest[]>([]);
  // The chat on screen; `null` until the active one is known (or when
  // there is none, and the next message starts one). The ref is what the
  // socket handlers read.
  const [chat, setChat] = useState<string | null>(null);
  const chatIdRef = useRef<string | null>(null);
  const [chats, setChats] = useState<ChatSummary[]>([]);
  // Replies still due in chats not on screen, kept for when they are.
  const waitingElsewhere = useRef(new Map<string, number>());
  // Chats deleted here: what they still send is dropped.
  const deletedChats = useRef(new Set<string>());
  // The chat list on narrow screens, where it replaces the chat.
  const [showChats, setShowChats] = useState(false);

  function selectChat(next: string | null) {
    chatIdRef.current = next;
    setChat(next);
  }

  const refreshChats = useCallback(async () => {
    try {
      setChats(await api.chats.list(id));
    } catch (e) {
      console.warn("[chat] chat list load failed", e);
    }
  }, [id]);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const list = await api.chats.list(id);
        if (!cancelled) setChats(list);
      } catch (e) {
        console.warn("[chat] chat list load failed", e);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

  const chatRef = useRef<ChatHandle | null>(null);
  const bubbleIdRef = useRef(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);

  // System-level embedding status (spec §9.4 v0.7). Fetched once
  // on mount — config rarely changes mid-session and stale info
  // here just shows the wrong pill briefly. Failure is silently
  // swallowed (the chat works fine without knowing the embedder).
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const e = await api.embedding();
        if (!cancelled) setEmbedding(e);
      } catch {
        // shrug — non-fatal
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Initial /me — fetched once. The user identity doesn't change for
  // the life of a page, and re-fetching it every poll caused the WS
  // effect's `me` dep to flicker which closed and reopened the
  // connection on every tick. Now: one fetch, kept stable.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const u = await api.me();
        if (!cancelled) setMe(u);
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  // Load the chat's history from agent.db, the active chat's at first.
  // The WS only appends NEW messages — the history seeded here is the
  // single source of truth for "what we said before". Chats are scoped
  // to a stable per-(user, agent) session server-side, so navigating
  // away and back picks up where we left.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const since = Date.now();
        const r = await api.conversation.list(id, HISTORY_LIMIT, chat ?? undefined);
        if (cancelled) return;
        if (!chatIdRef.current && r.conversation) selectChat(r.conversation);
        if (r.turns.length === 0) return;
        const stored = turnBubbles(r.turns, () => bubbleIdRef.current++);
        setBubbles((prev) => mergeStored(prev, stored, since));
      } catch (e) {
        // Not fatal — chat works fine without history (just won't show
        // past turns). Surface the error so we know if it's persistent.
        if (!cancelled)
          console.warn("[chat] history load failed", e);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id, chat]);

  // Poll the agent row for status / connected — used for the stop
  // button and the header pill. Doesn't gate the WS (auto-spawn does
  // that on connect), so a slower cadence is fine.
  useEffect(() => {
    let cancelled = false;
    // Once the agent has loaded, a failed poll (the gateway restarting)
    // keeps the chat on screen; the next poll catches up.
    let loaded = false;
    async function tick() {
      try {
        const a = await api.agents.get(id);
        if (!cancelled) {
          loaded = true;
          setAgent(a);
          setError(null);
        }
      } catch (e) {
        if (!cancelled && !loaded) setError(e instanceof Error ? e.message : String(e));
      }
    }
    tick();
    const t = setInterval(tick, AGENT_POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(t);
    };
  }, [id]);

  // WebSocket lifecycle — open ONCE per (page, token) pair. The
  // gateway lazy-spawns the runtime if needed. After an unintentional
  // close it keeps reconnecting, and each open reloads the stored
  // conversation, which has the replies sent while the chat was away
  // (and the history, if the gateway was down when the page loaded).
  useEffect(() => {
    if (!me) return;
    if (chatRef.current) return;

    let cancelled = false;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    async function catchUp() {
      try {
        const since = Date.now();
        const asked = chatIdRef.current;
        const r = await api.conversation.list(id, HISTORY_LIMIT, asked ?? undefined);
        if (cancelled || chatIdRef.current !== asked) return;
        if (!asked && r.conversation) selectChat(r.conversation);
        const stored = turnBubbles(r.turns, () => bubbleIdRef.current++);
        setBubbles((prev) => mergeStored(prev, stored, since));
      } catch (e) {
        console.warn("[chat] history reload failed", e);
      }
    }

    function connect() {
      if (cancelled) return;
      setWsState("connecting");
      const handle = openChat(id, me!.ws_token, {
        onOpen: () => {
          setWsState("open");
          // The gateway sends what still waits to every new connection.
          setApprovals([]);
          void catchUp();
        },
        onMessage: (msg) => {
          // A reply answers the oldest message waiting; a soft error
          // (agent gone) ends the wait for all of them.
          if (msg.type === "approval") {
            // The reply still runs; it waits for this answer. Requests are
            // sent again to a reconnecting tab, so keep one card per id.
            setApprovals((prev) => (prev.some((a) => a.id === msg.id) ? prev : [...prev, msg]));
            return;
          }
          if (msg.type === "approval_closed") {
            setApprovals((prev) => prev.filter((a) => a.id !== msg.id));
            return;
          }
          if (msg.type === "agent_message" && msg.conversation) {
            if (deletedChats.current.has(msg.conversation)) return;
            void refreshChats();
            const current = chatIdRef.current;
            // A reply in another chat shows there.
            if (current && msg.conversation !== current) {
              const due = waitingElsewhere.current.get(msg.conversation) ?? 0;
              if (!msg.broadcast && due > 0) waitingElsewhere.current.set(msg.conversation, due - 1);
              return;
            }
            if (!current) selectChat(msg.conversation);
          }
          if (msg.type === "agent_message") {
            if (!msg.broadcast) setWaiting((n) => Math.max(0, n - 1));
          } else {
            setWaiting(0);
          }
          if (msg.type === "agent_message") {
            pushBubble("agent", msg.content);
          } else {
            pushBubble("system", `error: ${msg.message}`);
          }
        },
        onError: () => {
          setWaiting(0);
          setWsState("error");
        },
        onClose: () => {
          chatRef.current = null;
          if (cancelled) return;
          setWsState("closed");
          // A reply still due arrives through the reload on reconnect.
          setWaiting(0);
          reconnectTimer = setTimeout(() => {
            if (!cancelled && !chatRef.current) connect();
          }, RECONNECT_MS);
        },
      });
      chatRef.current = handle;
    }
    connect();

    return () => {
      cancelled = true;
      if (reconnectTimer) clearTimeout(reconnectTimer);
      chatRef.current?.close();
      chatRef.current = null;
    };
    // We want this to run ONCE per agent-id/token pair, not on every
    // setAgent re-render. Splitting `me` into `me?.ws_token` keeps
    // the dep stable (a UUID string compares by value).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, me?.ws_token]);

  // Auto-scroll to bottom on new bubbles or when the typing
  // indicator appears.
  useEffect(() => {
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    scrollRef.current?.scrollTo({
      top: scrollRef.current.scrollHeight,
      behavior: reduce ? "auto" : "smooth",
    });
  }, [bubbles.length, pending]);

  function pushBubble(role: Bubble["role"], content: string) {
    setBubbles((prev) => [
      ...prev,
      { id: bubbleIdRef.current++, role, content, ts: Date.now() },
    ]);
  }

  function sendText(text: string) {
    if (!text.trim() || !chatRef.current || wsState !== "open") return;
    pushBubble("you", text);
    chatRef.current.send(text, chatIdRef.current ?? undefined);
    setWaiting((n) => n + 1);
  }

  function stopReply() {
    chatRef.current?.stop(chatIdRef.current ?? undefined);
  }

  // Show chat `next`, or a new one. Approvals stay: they are the agent's,
  // whichever chat asked.
  function openChatById(next: string | null) {
    setShowChats(false);
    if (next !== null && next === chatIdRef.current) return;
    const previous = chatIdRef.current;
    const target = next ?? newChatId();
    selectChat(target);
    setBubbles([]);
    setWaiting((n) => {
      if (previous) waitingElsewhere.current.set(previous, n);
      return waitingElsewhere.current.get(target) ?? 0;
    });
  }

  async function renameChat(c: ChatSummary) {
    const title = prompt("Rename this chat", c.title)?.trim();
    if (!title || title === c.title) return;
    try {
      await api.chats.rename(id, c.id, title);
    } catch (e) {
      pushBubble("system", `error: ${e instanceof Error ? e.message : String(e)}`);
    }
    void refreshChats();
  }

  async function deleteChat(c: ChatSummary) {
    if (!confirm(`Delete the chat "${c.title || "untitled"}" and its messages?`)) return;
    try {
      await api.chats.delete(id, c.id);
    } catch (e) {
      pushBubble("system", `error: ${e instanceof Error ? e.message : String(e)}`);
      return;
    }
    deletedChats.current.add(c.id);
    waitingElsewhere.current.delete(c.id);
    if (c.id === chatIdRef.current) {
      // The next message starts a new chat.
      selectChat(null);
      setBubbles([]);
      setWaiting(0);
    }
    void refreshChats();
  }

  function decide(id: string, approve: boolean, remember: boolean) {
    // The gateway closes the card in every tab once it took the answer.
    if (!chatRef.current || wsState !== "open") return;
    chatRef.current.decide(id, approve, remember);
  }

  function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    sendText(draft);
    setDraft("");
  }

  if (!agent && !error) {
    return (
      <div className="px-4 py-8 sm:px-8 sm:py-12" style={{ color: "var(--color-body)" }}>
        loading…
      </div>
    );
  }
  if (error) {
    return (
      <div className="mx-auto max-w-3xl px-4 py-8 sm:px-8 sm:py-12">
        <div
          className="card px-4 py-3 text-sm"
          style={{ color: "var(--color-ruby)", borderColor: "rgba(234, 34, 97, 0.3)" }}
        >
          {error}
        </div>
        <Link href="/" className="btn-ghost mt-4 inline-block">
          back to agents
        </Link>
      </div>
    );
  }
  if (!agent) return null;

  return (
    <div className="flex h-[calc(100dvh-(--spacing(15)))] md:h-dvh">
      <ChatList
        chats={chats}
        current={chat}
        onOpen={openChatById}
        onRename={renameChat}
        onDelete={deleteChat}
        className={showChats ? "flex w-full" : "hidden"}
      />
      <div className={`${showChats ? "hidden" : "flex"} min-w-0 flex-1 flex-col md:flex`}>
        {/* Header */}
        <div
          className="flex flex-wrap items-center justify-between gap-3 border-b px-4 py-4 sm:px-8"
          style={{ borderColor: "var(--color-border-default)" }}
        >
          <div>
            <h1 className="display-h3">{agent.name}</h1>
            <p
              className="font-mono text-xs"
              style={{ color: "var(--color-body)" }}
            >
              {agent.id}
            </p>
          </div>
          <div className="flex flex-wrap items-center gap-2 sm:gap-3">
            <button
              type="button"
              className="btn-ghost inline-flex items-center gap-1.5 md:hidden"
              onClick={() => setShowChats(true)}
            >
              <MessagesSquare size={16} className="shrink-0" aria-hidden="true" />
              chats
            </button>
            <button
              type="button"
              className="btn-ghost inline-flex items-center gap-1.5"
              onClick={() => openChatById(null)}
              title="Start a new chat; this one stays in the list"
            >
              <MessageSquarePlus size={16} className="shrink-0" aria-hidden="true" />
              new chat
            </button>
            <Link
              href={`/agents/${agent.id}/memory`}
              className="btn-ghost inline-flex items-center gap-1.5"
              title="See what the agent remembers about you"
            >
              <Brain size={16} className="shrink-0" aria-hidden="true" />
              memory
            </Link>
            <Link
              href={`/agents/${agent.id}/skills`}
              className="btn-ghost inline-flex items-center gap-1.5"
              title="Installed skills + curator log"
            >
              <BookOpen size={16} className="shrink-0" aria-hidden="true" />
              skills
            </Link>
            <Link
              href={`/agents/${agent.id}/settings`}
              className="btn-ghost inline-flex items-center gap-1.5"
              title="Rename, change model, edit policy"
            >
              <Settings size={16} className="shrink-0" aria-hidden="true" />
              settings
            </Link>
            <MemoryPill embedding={embedding} />
            <ConnectionPill state={wsState} agent={agent} />
          </div>
        </div>

        {/* Conversation */}
        <div
          ref={scrollRef}
          className="flex-1 overflow-y-auto px-4 py-6 sm:px-8"
          style={{ backgroundColor: "var(--color-surface-muted)" }}
        >
          <div className="mx-auto flex max-w-3xl flex-col gap-3">
            {bubbles.length === 0 && (
              <div
                className="card px-5 py-12 text-center"
                style={{ color: "var(--color-body)" }}
              >
                {wsState === "connecting"
                  ? "starting agent…"
                  : wsState === "error" || wsState === "closed"
                    ? "reconnecting…"
                    : "send a message to start the conversation"}
              </div>
            )}
            {bubbles.map((b, idx) => (
              <Bubble
                key={b.id}
                bubble={b}
                onRetry={
                  b.role === "agent"
                    ? () => {
                        // Find the most recent user bubble before this
                        // agent reply and re-send its content. Both the
                        // original and the retry stay in history — the
                        // agent then has both attempts in context, which
                        // is correct (it can see what didn't work).
                        for (let i = idx - 1; i >= 0; i--) {
                          if (bubbles[i].role === "you") {
                            sendText(bubbles[i].content);
                            return;
                          }
                        }
                      }
                    : undefined
                }
              />
            ))}
            {approvals.map((a) => (
              <div
                key={a.id}
                className="card px-4 py-3 text-sm"
                role="alertdialog"
                aria-label={`approve ${a.tool}`}
              >
                <p className="mb-1">
                  <strong>
                    {a.kind === "network" ? "Allow this network access?" : "Approve this tool call?"}
                  </strong>{" "}
                  <span className="font-mono">{a.tool}</span>
                  {a.conversation && (
                    <span style={{ color: "var(--color-body)" }}>
                      {" "}
                      (in the conversation {a.conversation})
                    </span>
                  )}
                </p>
                <pre
                  className="mb-2 overflow-x-auto whitespace-pre-wrap text-xs font-mono"
                  style={{ color: "var(--color-body)" }}
                >
                  {a.input}
                </pre>
                <div className="flex flex-wrap gap-2">
                  <button
                    type="button"
                    className="btn-primary"
                    disabled={wsState !== "open"}
                    onClick={() => decide(a.id, true, false)}
                  >
                    {a.kind === "network" ? "allow for 10 minutes" : "approve"}
                  </button>
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={wsState !== "open"}
                    onClick={() => decide(a.id, true, true)}
                    title={
                      a.kind === "network"
                        ? "Add these domains to the agent's allowed domains"
                        : "Approve this tool with this exact input for the rest of the conversation, until the agent restarts"
                    }
                  >
                    {a.kind === "network" ? "always allow" : "approve for this conversation"}
                  </button>
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={wsState !== "open"}
                    onClick={() => decide(a.id, false, false)}
                  >
                    deny
                  </button>
                </div>
              </div>
            ))}
            {pending && <TypingBubble />}
          </div>
        </div>

        {/* Composer */}
        <Composer
          wsState={wsState}
          waiting={waiting}
          onStop={stopReply}
          draft={draft}
          setDraft={setDraft}
          onSubmit={onSubmit}
        />
      </div>
    </div>
  );
}

function ChatList({
  chats,
  current,
  onOpen,
  onRename,
  onDelete,
  className,
}: {
  chats: ChatSummary[];
  current: string | null;
  onOpen: (chat: string | null) => void;
  onRename: (chat: ChatSummary) => void;
  onDelete: (chat: ChatSummary) => void;
  className: string;
}) {
  const fresh = current !== null && !chats.some((c) => c.id === current);
  const itemStyle = (on: boolean) =>
    on ? { backgroundColor: "var(--color-surface-muted)" } : undefined;
  return (
    <aside
      className={`${className} shrink-0 flex-col border-r md:flex md:w-64`}
      style={{ borderColor: "var(--color-border-default)" }}
    >
      <div className="p-3">
        <button
          type="button"
          className="btn-primary inline-flex w-full items-center justify-center gap-1.5"
          onClick={() => onOpen(null)}
        >
          <MessageSquarePlus size={16} className="shrink-0" aria-hidden="true" />
          new chat
        </button>
      </div>
      <nav aria-label="chats" className="flex-1 overflow-y-auto px-2 pb-3">
        <ul className="flex flex-col gap-0.5">
          {fresh && (
            <li className="rounded px-2 py-1.5 text-sm" style={itemStyle(true)} aria-current="true">
              new chat
            </li>
          )}
          {chats.map((c) => {
            const on = c.id === current;
            const title = c.title || "untitled";
            return (
              <li key={c.id} className="flex items-center gap-0.5 rounded" style={itemStyle(on)}>
                <button
                  type="button"
                  className="min-w-0 flex-1 truncate px-2 py-1.5 text-left text-sm"
                  onClick={() => onOpen(c.id)}
                  aria-current={on ? "true" : undefined}
                  title={title}
                >
                  {title}
                </button>
                <button
                  type="button"
                  className="shrink-0 p-1.5"
                  style={{ color: "var(--color-body)" }}
                  onClick={() => onRename(c)}
                  aria-label={`rename ${title}`}
                  title="rename"
                >
                  <Pencil size={14} aria-hidden="true" />
                </button>
                <button
                  type="button"
                  className="shrink-0 p-1.5"
                  style={{ color: "var(--color-body)" }}
                  onClick={() => onDelete(c)}
                  aria-label={`delete ${title}`}
                  title="delete"
                >
                  <Trash2 size={14} aria-hidden="true" />
                </button>
              </li>
            );
          })}
          {chats.length === 0 && !fresh && (
            <li className="px-2 py-1.5 text-xs" style={{ color: "var(--color-body)" }}>
              no chats yet
            </li>
          )}
        </ul>
      </nav>
    </aside>
  );
}

function Bubble({
  bubble,
  onRetry,
}: {
  bubble: Bubble;
  onRetry?: () => void;
}) {
  const align =
    bubble.role === "you" ? "items-end" : "items-start";
  // System bubbles stay plain — markdown renderer is overkill for
  // "agent reconnecting…" hints. User + agent get full markdown +
  // a below-bubble action row (copy / retry).
  const isSystem = bubble.role === "system";
  return (
    <div className={`flex flex-col ${align}`}>
      <div
        className="px-4 py-2.5"
        style={{
          maxWidth: "min(85%, 720px)",
          borderRadius: "var(--radius-large)",
          ...(bubble.role === "you"
            ? {
                backgroundColor: "var(--color-stripe-purple)",
                color: "white",
              }
            : isSystem
              ? {
                  backgroundColor: "var(--color-surface)",
                  color: "var(--color-body)",
                  border: "1px dashed var(--color-border-soft-purple)",
                  fontStyle: "italic",
                  fontSize: 12,
                }
              : {
                  backgroundColor: "var(--color-surface)",
                  color: "var(--color-deep-navy)",
                  border: "1px solid var(--color-border-default)",
                  boxShadow: "var(--shadow-ambient)",
                }),
        }}
      >
        {isSystem ? (
          <span style={{ whiteSpace: "pre-wrap" }}>{bubble.content}</span>
        ) : (
          <Markdown>{bubble.content}</Markdown>
        )}
      </div>
      {!isSystem && (
        <ActionRow
          text={bubble.content}
          onRetry={onRetry}
          align={bubble.role === "you" ? "end" : "start"}
        />
      )}
    </div>
  );
}

/**
 * Below-bubble action row: always-visible Copy + (for agent bubbles)
 * Retry. Aligns with whichever side the bubble sits on.
 */
function ActionRow({
  text,
  onRetry,
  align,
}: {
  text: string;
  onRetry?: () => void;
  align: "start" | "end";
}) {
  const [copied, setCopied] = useState(false);

  async function onCopy() {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Fallback for older browsers / no permission. Select +
      // execCommand is deprecated but works as a last resort.
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
      } catch {
        // give up silently
      }
      document.body.removeChild(ta);
    }
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  }

  return (
    <div
      className={`mt-1 flex gap-2 px-1 ${align === "end" ? "justify-end" : "justify-start"}`}
      style={{ fontSize: 11, color: "var(--color-body)" }}
    >
      <button
        type="button"
        onClick={onCopy}
        title="copy markdown source"
        aria-label={copied ? "copied" : "copy message"}
        className="rounded px-1.5 py-0.5 transition-colors"
        style={{ background: "transparent" }}
        onMouseEnter={(e) =>
          (e.currentTarget.style.background = "rgba(100,116,141,0.08)")
        }
        onMouseLeave={(e) =>
          (e.currentTarget.style.background = "transparent")
        }
      >
        <span className="inline-flex items-center gap-1">
          {copied ? <Check size={12} className="shrink-0" aria-hidden="true" /> : <Copy size={12} className="shrink-0" aria-hidden="true" />}
          {copied ? "copied" : "copy"}
        </span>
      </button>
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          title="re-send the previous user message"
          aria-label="retry"
          className="rounded px-1.5 py-0.5 transition-colors"
          style={{ background: "transparent" }}
          onMouseEnter={(e) =>
            (e.currentTarget.style.background = "rgba(100,116,141,0.08)")
          }
          onMouseLeave={(e) =>
            (e.currentTarget.style.background = "transparent")
          }
        >
          <span className="inline-flex items-center gap-1">
            <RotateCcw size={12} className="shrink-0" aria-hidden="true" />
            retry
          </span>
        </button>
      )}
    </div>
  );
}

/**
 * Inline "agent is replying" placeholder. Three pulsing dots in an
 * agent-shaped bubble — same styling as a real agent message so the
 * eye doesn't have to retrain. Replaced by the actual reply when
 * `agent_message` arrives (parent flips `pending` to false).
 */
function TypingBubble() {
  return (
    <div className="flex flex-col items-start">
      <div
        className="flex items-center gap-1 px-4 py-3"
        style={{
          maxWidth: "min(85%, 720px)",
          borderRadius: "var(--radius-large)",
          backgroundColor: "var(--color-surface)",
          color: "var(--color-body)",
          border: "1px solid var(--color-border-default)",
          boxShadow: "var(--shadow-ambient)",
        }}
        aria-label="agent is replying"
      >
        <Dot delayMs={0} />
        <Dot delayMs={180} />
        <Dot delayMs={360} />
      </div>
    </div>
  );
}

function Spinner() {
  return (
    <span
      aria-hidden="true"
      style={{
        width: 12,
        height: 12,
        border: "1.5px solid rgba(255,255,255,0.4)",
        borderTopColor: "white",
        borderRadius: "50%",
        display: "inline-block",
        animation: "havn-spin 0.7s linear infinite",
      }}
    />
  );
}

function Dot({ delayMs }: { delayMs: number }) {
  return (
    <span
      style={{
        display: "inline-block",
        width: 6,
        height: 6,
        borderRadius: "50%",
        background: "var(--color-body)",
        opacity: 0.5,
        animation: "havn-typing 1.2s ease-in-out infinite",
        animationDelay: `${delayMs}ms`,
      }}
    />
  );
}

// Composer with GitHub-style Write / Preview tabs. Submit on Cmd/Ctrl+Enter
// or via the explicit button. Plain textarea (multi-line, monospace
// optional) keeps the input ergonomic for code blocks and longer prompts.
function Composer({
  wsState,
  waiting,
  onStop,
  draft,
  setDraft,
  onSubmit,
}: {
  wsState: "idle" | "connecting" | "open" | "closed" | "error";
  waiting: number;
  onStop: () => void;
  draft: string;
  setDraft: (s: string) => void;
  onSubmit: (e: React.FormEvent) => void;
}) {
  const [tab, setTab] = useState<"write" | "preview">("write");
  // The composer is ready whenever the WS is up. A message sent while a
  // reply runs waits in the agent and is answered after it, in order.
  const ready = wsState === "open";
  const pending = waiting > 0;
  return (
    <form
      onSubmit={onSubmit}
      className="border-t px-4 py-3 sm:px-8"
      style={{ borderColor: "var(--color-border-default)" }}
    >
      <div className="mx-auto max-w-3xl">
        {/* Tab strip */}
        <div className="mb-2 flex items-center justify-between">
          <div
            className="flex gap-1 text-sm"
            role="tablist"
            aria-label="Composer mode"
          >
            <ComposerTab
              active={tab === "write"}
              onClick={() => setTab("write")}
            >
              Write
            </ComposerTab>
            <ComposerTab
              active={tab === "preview"}
              onClick={() => setTab("preview")}
              disabled={!draft.trim()}
            >
              Preview
            </ComposerTab>
          </div>
          <span className="text-xs" style={{ color: "var(--color-body)" }}>
            <kbd
              className="font-mono"
              style={{ fontSize: 11 }}
            >
              Cmd/Ctrl+Enter
            </kbd>{" "}
            to send · markdown supported
          </span>
        </div>

        {/* Body */}
        {tab === "write" ? (
          <textarea
            placeholder={
              ready
                ? "Type a message — markdown welcome (lists, code blocks, links, tables)"
                : "Waiting for agent connection…"
            }
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (
                (e.metaKey || e.ctrlKey) &&
                e.key === "Enter" &&
                ready &&
                draft.trim()
              ) {
                e.preventDefault();
                onSubmit(e as unknown as React.FormEvent);
              }
            }}
            disabled={!ready}
            rows={4}
            className="input w-full"
            style={{
              resize: "vertical",
              minHeight: 84,
              fontFamily: "inherit",
              padding: "10px 12px",
              lineHeight: 1.45,
            }}
          />
        ) : (
          <div
            className="card w-full overflow-y-auto px-4 py-3"
            style={{
              minHeight: 84,
              maxHeight: 360,
              backgroundColor: "var(--color-surface)",
            }}
          >
            {draft.trim() ? (
              <Markdown>{draft}</Markdown>
            ) : (
              <span style={{ color: "var(--color-body)" }}>
                Nothing to preview yet.
              </span>
            )}
          </div>
        )}

        {/* Action row */}
        <div className="mt-2 flex items-center justify-end gap-3">
          {pending && (
            <span
              className="flex items-center gap-2 text-sm"
              style={{ color: "var(--color-body)" }}
              role="status"
            >
              <Spinner /> replying
              {waiting > 1 && `, ${waiting - 1} queued`}
            </span>
          )}
          {pending && (
            <button
              type="button"
              className="btn-ghost"
              onClick={onStop}
              disabled={wsState !== "open"}
              title="Stop the reply in progress; queued messages still get answered"
            >
              stop
            </button>
          )}
          <button
            type="submit"
            className="btn-primary"
            disabled={!ready || !draft.trim()}
            style={{ minWidth: 110 }}
          >
            {pending ? "queue" : "send"}
          </button>
        </div>
      </div>
    </form>
  );
}

function ComposerTab({
  active,
  disabled,
  onClick,
  children,
}: {
  active: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      role="tab"
      aria-selected={active}
      disabled={disabled}
      onClick={onClick}
      className="px-3 py-1.5 text-sm transition-colors"
      style={{
        borderBottom: active
          ? "2px solid var(--color-stripe-purple)"
          : "2px solid transparent",
        color: active
          ? "var(--color-stripe-purple)"
          : "var(--color-body)",
        background: "transparent",
        opacity: disabled ? 0.4 : 1,
        cursor: disabled ? "not-allowed" : "pointer",
      }}
    >
      {children}
    </button>
  );
}

/**
 * Tiny diagnostic pill in the chat header showing whether retrieval
 * (memory **and** skills — they share the same embedding provider per
 * spec §13 Phase 3) is hybrid (vector + keyword) or keyword-only.
 * Helps users know when their semantic-near queries should work vs
 * when they need to use the actual words from the saved fact / skill
 * description.
 */
function MemoryPill({ embedding }: { embedding: EmbeddingStatus | null }) {
  if (!embedding) return null;
  const hybrid = embedding.hybrid_enabled;
  const provider = embedding.provider;
  const label = hybrid ? provider : "keyword-only";
  const LabelIcon = hybrid ? Layers : Search;
  const tip = hybrid
    ? `Hybrid retrieval (memory + skills): vector (${provider}) + BM25 keyword. Semantic-near queries work even with no shared words.`
    : "FTS5 keyword-only (memory + skills). Set [embedding] in config.toml to enable hybrid retrieval.";
  return (
    <span
      className="pill pill-stopped"
      title={tip}
      style={{
        background: hybrid
          ? "rgba(83, 58, 253, 0.08)"
          : "var(--color-surface-muted)",
        color: hybrid
          ? "var(--color-stripe-purple)"
          : "var(--color-body)",
      }}
    >
      <LabelIcon size={12} className="shrink-0" aria-hidden="true" />
      {label}
    </span>
  );
}

function ConnectionPill({
  state,
  agent,
}: {
  state: "idle" | "connecting" | "open" | "closed" | "error";
  agent: AgentView;
}) {
  // The chat WS lifecycle is the source of truth — agent.status from
  // the polling fetch is only used as a fallback hint when WS hasn't
  // started yet (idle).
  if (state === "open") return <span className="pill pill-running">live</span>;
  if (state === "connecting")
    return <span className="pill pill-stopped">starting…</span>;
  if (state === "error" || state === "closed")
    return <span className="pill pill-stopped">reconnecting…</span>;
  return <span className="pill pill-stopped">{agent.status}</span>;
}
