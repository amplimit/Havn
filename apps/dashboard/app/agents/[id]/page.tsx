"use client";

import { use, useEffect, useRef, useState } from "react";
import Link from "next/link";
import {
  api,
  type AgentView,
  type EmbeddingStatus,
  type MeView,
} from "@/lib/api";
import { openChat, type ChatHandle } from "@/lib/websocket";
import { Markdown } from "@/components/Markdown";

type Bubble = {
  id: number;
  role: "you" | "agent" | "system";
  content: string;
  ts: number;
};

const AGENT_POLL_MS = 5000;

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
  const [pending, setPending] = useState(false);

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

  // Load past conversation from agent.db on mount. The WS opens
  // afterwards and only appends NEW messages — the history seeded
  // here is the single source of truth for "what we said before".
  // Conversations are scoped to a stable per-(user, agent) channel
  // server-side, so navigating away and back picks up where we left.
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const r = await api.conversation.list(id, 100);
        if (cancelled || r.turns.length === 0) return;
        const seeded: Bubble[] = r.turns
          .filter((t) => t.role === "user" || t.role === "assistant")
          .map((t) => ({
            id: bubbleIdRef.current++,
            role: t.role === "user" ? "you" : "agent",
            content: t.content,
            ts: new Date(t.created_at).getTime(),
          }));
        setBubbles(seeded);
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
  }, [id]);

  // Poll the agent row for status / connected — used for the stop
  // button and the header pill. Doesn't gate the WS (auto-spawn does
  // that on connect), so a slower cadence is fine.
  useEffect(() => {
    let cancelled = false;
    async function tick() {
      try {
        const a = await api.agents.get(id);
        if (!cancelled) {
          setAgent(a);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
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
  // gateway lazy-spawns the runtime if needed; auto-reconnect on
  // unintentional close is handled inside this effect (re-run a
  // single time after a backoff).
  useEffect(() => {
    if (!me) return;
    if (chatRef.current) return;

    let cancelled = false;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;

    function connect() {
      if (cancelled) return;
      setWsState("connecting");
      const handle = openChat(id, me!.ws_token, {
        onOpen: () => setWsState("open"),
        onMessage: (msg) => {
          // Either a real reply or a soft error closes the "replying…"
          // state — the user gets feedback either way.
          setPending(false);
          if (msg.type === "agent_message") {
            pushBubble("agent", msg.content);
          } else {
            pushBubble("system", `error: ${msg.message}`);
          }
        },
        onError: () => {
          setPending(false);
          setWsState("error");
        },
        onClose: () => {
          chatRef.current = null;
          if (cancelled) return;
          setWsState("closed");
          // Auto-reconnect once after a small backoff. If the agent
          // was explicitly stopped (status flips away from running),
          // the next message will trigger lazy-spawn anyway, so a
          // quiet retry is the right default.
          reconnectTimer = setTimeout(() => {
            if (!cancelled && !chatRef.current) connect();
          }, 1500);
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
    scrollRef.current?.scrollTo({
      top: scrollRef.current.scrollHeight,
      behavior: "smooth",
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
    chatRef.current.send(text);
    setPending(true);
  }

  function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    sendText(draft);
    setDraft("");
  }

  if (!agent && !error) {
    return (
      <div className="px-8 py-12" style={{ color: "var(--color-body)" }}>
        loading…
      </div>
    );
  }
  if (error) {
    return (
      <div className="mx-auto max-w-3xl px-8 py-12">
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
    <div className="flex h-screen flex-col">
      {/* Header */}
      <div
        className="flex items-center justify-between border-b px-8 py-4"
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
        <div className="flex items-center gap-3">
          <Link
            href={`/agents/${agent.id}/memory`}
            className="btn-ghost"
            title="See what the agent remembers about you"
          >
            🧠 memory
          </Link>
          <Link
            href={`/agents/${agent.id}/skills`}
            className="btn-ghost"
            title="Installed skills + curator log"
          >
            📚 skills
          </Link>
          <Link
            href={`/agents/${agent.id}/settings`}
            className="btn-ghost"
            title="Rename, change model, edit policy"
          >
            ⚙ settings
          </Link>
          <MemoryPill embedding={embedding} />
          <ConnectionPill state={wsState} agent={agent} />
        </div>
      </div>

      {/* Conversation */}
      <div
        ref={scrollRef}
        className="flex-1 overflow-y-auto px-8 py-6"
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
                  ? "trouble connecting — try sending a message anyway"
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
          {pending && <TypingBubble />}
        </div>
      </div>

      {/* Composer */}
      <Composer
        wsState={wsState}
        pending={pending}
        draft={draft}
        setDraft={setDraft}
        onSubmit={onSubmit}
      />
    </div>
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
        aria-label="copy message"
        className="rounded px-1.5 py-0.5 transition-colors"
        style={{ background: "transparent" }}
        onMouseEnter={(e) =>
          (e.currentTarget.style.background = "rgba(100,116,141,0.08)")
        }
        onMouseLeave={(e) =>
          (e.currentTarget.style.background = "transparent")
        }
      >
        {copied ? "✓ copied" : "⧉ copy"}
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
          ↻ retry
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
  pending,
  draft,
  setDraft,
  onSubmit,
}: {
  wsState: "idle" | "connecting" | "open" | "closed" | "error";
  pending: boolean;
  draft: string;
  setDraft: (s: string) => void;
  onSubmit: (e: React.FormEvent) => void;
}) {
  const [tab, setTab] = useState<"write" | "preview">("write");
  // The composer is "ready" when the WS is up AND we're not still
  // waiting on the agent's previous reply. Disabling input while
  // pending prevents the user from queueing a second prompt that the
  // agent would interleave with the first response — confusing.
  const ready = wsState === "open" && !pending;
  return (
    <form
      onSubmit={onSubmit}
      className="border-t px-8 py-3"
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
        <div className="mt-2 flex justify-end">
          <button
            type="submit"
            className="btn-primary"
            disabled={!ready || !draft.trim()}
            style={{ minWidth: 110 }}
          >
            {pending ? (
              <span className="flex items-center justify-center gap-2">
                <Spinner /> replying…
              </span>
            ) : (
              "send"
            )}
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
  const label = hybrid ? `🧬 ${provider}` : "🔍 keyword-only";
  const tip = hybrid
    ? `Hybrid retrieval (memory + skills): vector (${provider}) + BM25 keyword. Semantic-near queries work even with no shared words.`
    : "FTS5 keyword-only (memory + skills). Set [embedding] in config.toml to enable hybrid retrieval (spec §9.4 / §13).";
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
    return <span className="pill pill-stopped">disconnected</span>;
  return <span className="pill pill-stopped">{agent.status}</span>;
}
