"use client";

import { use, useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import {
  api,
  type AgentView,
  type ChatCommand,
  type ChatSearch,
  type ChatSummary,
  type ConversationTurn,
  type EmbeddingStatus,
  type MeView,
  type SentFile,
} from "@/lib/api";
import { openChat, type ChatHandle } from "@/lib/websocket";
import { Markdown } from "@/components/Markdown";
import type { Attachment, MemoryChange, ServerMessage } from "@/lib/protocol";
import {
  BookOpen,
  Brain,
  Check,
  Copy,
  FileText,
  Layers,
  MessageSquarePlus,
  MessagesSquare,
  Paperclip,
  ChevronLeft,
  ChevronRight,
  Pencil,
  RefreshCw,
  RotateCcw,
  Search,
  Settings,
  Trash2,
  X,
} from "lucide-react";

type ApprovalRequest = Extract<ServerMessage, { type: "approval" }>;

type Bubble = {
  id: number;
  role: "you" | "agent" | "system";
  content: string;
  ts: number;
  /** The stored turn it shows, with its versions; absent until stored. */
  turn?: string;
  versions?: string[];
  /** A file the agent sent, to download. */
  file?: { url: string; name: string; caption?: string };
  /** A note of what a reply saved to or forgot from memory, and that
   * reply's stored turn, which it stays under. */
  memory?: MemoryChange[];
  anchor?: string;
};

const AGENT_POLL_MS = 5000;
/** What the gateway takes: files per message, and bytes per file. */
const MAX_ATTACHMENTS = 10;
const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

// A turn's attachment lines, which tell the agent where each file is, as
// the runtime writes them.
const JSON_STRING = String.raw`"(?:[^"\\]|\\.)*"`;
const ATTACHMENT_LINE = new RegExp(
  String.raw`^\[Attached file, user-supplied and untrusted: name (${JSON_STRING}), type (${JSON_STRING}), ([^,\]]+), saved at (${JSON_STRING})(?:; its text \(untrusted\) is at (${JSON_STRING}), read it with file_read)?\]$`,
);

/** A user turn's text, and the files it carried, from its stored content. */
function splitMessage(content: string): { text: string; files: Attachment[] } {
  const files: Attachment[] = [];
  const lines = content.split("\n").filter((line) => {
    const m = ATTACHMENT_LINE.exec(line);
    if (!m) return true;
    try {
      files.push({
        name: JSON.parse(m[1]) as string,
        media_type: JSON.parse(m[2]) as string,
        size: sizeBytes(m[3]),
        path: JSON.parse(m[4]) as string,
        ...(m[5] ? { text_path: JSON.parse(m[5]) as string } : {}),
      });
      return false;
    } catch {
      return true;
    }
  });
  return { text: lines.join("\n").trimEnd(), files };
}

/** A message as its stored turn holds it: the text, then a line per file. */
function joinMessage(text: string, files: Attachment[]): string {
  const line = (a: Attachment) =>
    `[Attached file, user-supplied and untrusted: name ${JSON.stringify(a.name)}, type ${JSON.stringify(a.media_type)}, ${humanSize(a.size)}, saved at ${JSON.stringify(a.path)}` +
    (a.text_path ? `; its text (untrusted) is at ${JSON.stringify(a.text_path)}, read it with file_read]` : "]");
  return [text, ...files.map(line)].filter((l) => l.trim()).join("\n");
}

/** A size as the runtime writes it: 12 B, 3.4 KB, 1.2 MB. */
function humanSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** About the bytes a size [`humanSize`] wrote stands for. */
function sizeBytes(size: string): number {
  const m = /^([\d.]+) (B|KB|MB)$/.exec(size.trim());
  if (!m) return 0;
  const unit = m[2] === "MB" ? 1024 * 1024 : m[2] === "KB" ? 1024 : 1;
  return Math.round(Number(m[1]) * unit);
}
const HISTORY_LIMIT = 100;
// How long after showing a found message new bubbles leave the view where it is.
const JUMP_HOLD_MS = 2000;
const RECONNECT_MS = 1500;

function turnBubbles(turns: ConversationTurn[], nextId: () => number): Bubble[] {
  return turns
    .filter((t) => t.role === "user" || t.role === "assistant")
    .map((t) => ({
      id: nextId(),
      role: t.role === "user" ? "you" : "agent",
      content: t.content,
      ts: new Date(t.created_at).getTime(),
      turn: t.id,
      versions: t.versions,
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
// `prev` with a card for each of `files` it does not show yet.
function withFiles(
  prev: Bubble[],
  files: { url: string; name: string; caption?: string; ts: number }[],
  nextId: () => number,
): Bubble[] {
  // By the file's id, the last part of its link: the page may name the
  // agent by its name, the gateway by its id.
  const fileId = (url: string) => url.slice(url.lastIndexOf("/") + 1);
  const fresh = files.filter((f) => !prev.some((b) => b.file && fileId(b.file.url) === fileId(f.url)));
  if (fresh.length === 0) return prev;
  const cards: Bubble[] = fresh.map((f) => ({
    id: nextId(),
    role: "system",
    content: "",
    ts: f.ts,
    file: { url: f.url, name: f.name, caption: f.caption },
  }));
  return [...prev, ...cards].sort((a, b) => a.ts - b.ts);
}

function sentFiles(
  agentId: string,
  sent: SentFile[],
): { url: string; name: string; caption?: string; ts: number }[] {
  return sent.map((f) => ({
    url: `/agents/${agentId}/outbox/${f.id}`,
    name: f.name,
    caption: f.caption,
    ts: new Date(f.created_at).getTime(),
  }));
}

function mergeStored(prev: Bubble[], fetched: Bubble[], since: number): Bubble[] {
  // A turn already on screen keeps its bubble (and an editor open on it).
  const shown = new Map(prev.flatMap((b) => (b.turn ? [[b.turn, b.id] as const] : [])));
  const stored = fetched.map((s) => (s.turn && shown.has(s.turn) ? { ...s, id: shown.get(s.turn)! } : s));
  const recent = stored.filter((s) => s.ts >= since - CLOCK_SKEW_MS);
  const kept = prev
    .filter(
      (b) =>
        b.role === "system" ||
        (b.ts >= since && !recent.some((s) => s.role === b.role && s.content === b.content)),
    )
    .map((b) => underAnchor(b, stored));
  return [...stored, ...kept].sort((a, b) => a.ts - b.ts);
}

// A memory note goes right under the reply it came with, whatever the
// browser's clock says.
function underAnchor(b: Bubble, stored: Bubble[]): Bubble {
  const reply = b.anchor ? stored.find((s) => s.turn === b.anchor) : undefined;
  return reply ? { ...b, ts: reply.ts + 0.5 } : b;
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
  // The tool call the reply being written in this chat is making.
  const [step, setStep] = useState<Step | null>(null);
  // The text of the reply being written, by model call, as it arrives.
  const [live, setLive] = useState<{ call: string; text: string }[]>([]);
  // Calls whose text is no longer shown: their reply came or was stopped;
  // and those shown now. Kept apart from `live` so a call ends at once.
  const doneCalls = useRef(new Set<string>());
  const liveCalls = useRef(new Set<string>());
  const pending = waiting > 0;
  // Tool calls of the running reply that wait for the user.
  const [approvals, setApprovals] = useState<ApprovalRequest[]>([]);
  // The chat on screen; `null` until the active one is known (or when
  // there is none, and the next message starts one). The ref is what the
  // socket handlers read.
  const [chat, setChat] = useState<string | null>(null);
  const chatIdRef = useRef<string | null>(null);
  // `/undo`s sent whose answer has not come: the chat reloads with it.
  const undoing = useRef(0);
  // A found message to show once its chat is on screen (`branched` once
  // its chat was moved to the message's branch), and the one shown; when
  // a jump scrolled, so the next updates do not scroll away from it.
  const jumpTo = useRef<{ chat: string; turn: string; branched: boolean } | null>(null);
  const [found, setFound] = useState<string | null>(null);
  const jumpedAt = useRef(0);
  const [chats, setChats] = useState<ChatSummary[]>([]);
  // For the socket's handler, which outlives a render.
  const chatsRef = useRef<ChatSummary[]>([]);
  useEffect(() => {
    chatsRef.current = chats;
  }, [chats]);
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
        // A reply still running when the page was left shows again; the
        // agent's count is the count.
        if (r.waiting !== undefined) setWaiting(r.waiting);
        if (r.turns.length === 0) return;
        const stored = turnBubbles(r.turns, () => bubbleIdRef.current++);
        setBubbles((prev) => mergeStored(prev, stored, since));
        // The files sent in it, which its stored turns do not hold.
        const shown = r.conversation ?? chat ?? undefined;
        if (shown) {
          api.outbox
            .list(id, shown)
            .then((sent) => {
              if (cancelled) return;
              setBubbles((prev) => withFiles(prev, sentFiles(id, sent), () => bubbleIdRef.current++));
            })
            .catch(() => {});
        }
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

    // `reconnected`: the socket is new, and with it what this tab knew of
    // the replies running; the agent says how many still are.
    async function catchUp(reconnected = false) {
      try {
        const since = Date.now();
        const asked = chatIdRef.current;
        const r = await api.conversation.list(id, HISTORY_LIMIT, asked ?? undefined);
        if (cancelled || chatIdRef.current !== asked) return;
        if (!asked && r.conversation) selectChat(r.conversation);
        if (reconnected && r.waiting !== undefined) setWaiting(r.waiting);
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
          void catchUp(true);
        },
        onMessage: (msg) => {
          // A reply answers the oldest message waiting; a soft error
          // (agent gone) ends the wait for all of them.
          if (msg.type === "approval") {
            // The card says what waits; the step before it is not running.
            if (!msg.conversation || msg.conversation === chatIdRef.current) setStep(null);
            // The reply still runs; it waits for this answer. Requests are
            // sent again to a reconnecting tab, so keep one card per id.
            setApprovals((prev) => (prev.some((a) => a.id === msg.id) ? prev : [...prev, msg]));
            return;
          }
          if (msg.type === "approval_closed") {
            setApprovals((prev) => prev.filter((a) => a.id !== msg.id));
            return;
          }
          if (msg.type === "step" || msg.type === "reply_text") {
            const current = chatIdRef.current;
            const of = msg.conversation;
            if (of && deletedChats.current.has(of)) return;
            if (of && current && of !== current) return;
            // A new chat not saved yet shows only a chat that is new too.
            if (of && !current && chatsRef.current.some((c) => c.id === of)) return;
            if (msg.type === "step") {
              setStep({ tool: msg.tool, detail: msg.detail });
              return;
            }
            if (doneCalls.current.has(msg.call)) return;
            liveCalls.current.add(msg.call);
            setStep(null);
            setLive((prev) => {
              const i = prev.findIndex((p) => p.call === msg.call);
              if (msg.at === 0) {
                if (i < 0) return [...prev, { call: msg.call, text: msg.text }];
                const next = [...prev];
                next[i] = { call: msg.call, text: msg.text };
                return next;
              }
              // A piece that does not follow what is shown waits for the
              // next whole text.
              if (i < 0 || prev[i].text.length !== msg.at) return prev;
              const next = [...prev];
              next[i] = { call: msg.call, text: prev[i].text + msg.text };
              return next;
            });
            return;
          }
          if (msg.type === "file") {
            const current = chatIdRef.current;
            if (msg.conversation && current && msg.conversation !== current) return;
            addFile(msg.url, msg.name, msg.caption, Date.now());
            return;
          }
          if (msg.type === "chat_title") {
            void refreshChats();
            return;
          }
          if (msg.type === "notice") {
            const current = chatIdRef.current;
            if (msg.conversation && current && msg.conversation !== current) return;
            pushBubble("system", msg.content);
            // What `/undo` took back leaves the screen; other answers leave
            // the chat as it is, messages still waiting included.
            if (undoing.current > 0) {
              undoing.current -= 1;
              void catchUp();
            }
            return;
          }
          if (msg.type === "memory") {
            // Under the reply it came with, in the chat on screen only.
            const current = chatIdRef.current;
            if (msg.conversation && current && msg.conversation !== current) return;
            setBubbles((prev) => [
              ...prev,
              {
                id: bubbleIdRef.current++,
                role: "system",
                content: "",
                ts: Date.now(),
                memory: msg.changes,
                anchor: msg.turn,
              },
            ]);
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
            // A reply to this tab's message names the chat it started; a
            // task's result does not choose the chat on screen.
            if (!current && !msg.broadcast) selectChat(msg.conversation);
          }
          if (msg.type === "agent_message" && msg.conversation && !msg.broadcast) {
            // The stored turns, with their ids, for editing and versions.
            void catchUp();
          }
          if (msg.type === "agent_message") {
            if (!msg.broadcast) {
              setWaiting((n) => Math.max(0, n - 1));
              setStep(null);
              endLive();
            }
          } else if (msg.type === "error") {
            setWaiting(0);
            setStep(null);
            endLive();
          }
          if (msg.type === "agent_message") {
            pushBubble("agent", msg.content);
          } else if (msg.type === "error") {
            pushBubble("system", `error: ${msg.message}`);
          }
        },
        onError: () => {
          setWaiting(0);
          setStep(null);
          clearLive();
          setWsState("error");
        },
        onClose: () => {
          chatRef.current = null;
          if (cancelled) return;
          setWsState("closed");
          // A reply still due arrives through the reload on reconnect, and
          // the agent says again what is still running.
          setWaiting(0);
          waitingElsewhere.current.clear();
          setStep(null);
          clearLive();
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
  // indicator appears; not while a found message is to be shown, nor right
  // after it was (the chat's file cards arrive just after its messages).
  useEffect(() => {
    if (jumpTo.current || Date.now() - jumpedAt.current < JUMP_HOLD_MS) return;
    const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    scrollRef.current?.scrollTo({
      top: scrollRef.current.scrollHeight,
      behavior: reduce ? "auto" : "smooth",
    });
  }, [bubbles.length, pending]);

  // A found message, once its chat's messages are on screen: scrolled to
  // and marked; on another branch, the chat moves to it first; older than
  // what is loaded, the user is told.
  useEffect(() => {
    const jump = jumpTo.current;
    if (!jump || chatIdRef.current !== jump.chat || bubbles.length === 0) return;
    const el = scrollRef.current?.querySelector(`[data-turn="${CSS.escape(jump.turn)}"]`);
    if (el) {
      jumpTo.current = null;
      jumpedAt.current = Date.now();
      const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      el.scrollIntoView({ block: "center", behavior: reduce ? "auto" : "smooth" });
      setFound(jump.turn);
      setTimeout(() => setFound((f) => (f === jump.turn ? null : f)), 4000);
      return;
    }
    if (jump.branched) {
      jumpTo.current = null;
      pushBubble("system", "That message is further back than this chat shows.");
      return;
    }
    if (waiting > 0) {
      jumpTo.current = null;
      pushBubble("system", "That message is on another version of this chat; wait for the reply, then open it again.");
      return;
    }
    jump.branched = true;
    void showVersion(jump.turn);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bubbles]);

  // As the reply being written grows, follow it, unless the user scrolled
  // up to read.
  const atBottom = useRef(true);
  const liveLength = live.reduce((n, p) => n + p.text.length, 0);
  useEffect(() => {
    const el = scrollRef.current;
    if (el && liveLength > 0 && atBottom.current) el.scrollTo({ top: el.scrollHeight });
  }, [liveLength]);

  // A file card, once per file.
  function addFile(url: string, name: string, caption: string | undefined, ts: number) {
    setBubbles((prev) => withFiles(prev, [{ url, name, caption, ts }], () => bubbleIdRef.current++));
  }

  // Hide the text being written; a call still running shows it again
  // with its next whole text.
  function clearLive() {
    liveCalls.current.clear();
    setLive([]);
  }

  // Stop showing the text being written: the reply replaces it.
  function endLive() {
    for (const call of liveCalls.current) doneCalls.current.add(call);
    liveCalls.current.clear();
    // The oldest are forgotten; their text stopped coming long ago.
    for (const call of doneCalls.current) {
      if (doneCalls.current.size <= 500) break;
      doneCalls.current.delete(call);
    }
    setLive([]);
  }

  function pushBubble(role: Bubble["role"], content: string) {
    setBubbles((prev) => [
      ...prev,
      { id: bubbleIdRef.current++, role, content, ts: Date.now() },
    ]);
  }

  function sendText(text: string, files: Attachment[] = []) {
    if ((!text.trim() && files.length === 0) || !chatRef.current || wsState !== "open") return;
    // A command (alone, without files) is not a message: the page does
    // what it has a button for, and the gateway answers the rest.
    const command = files.length === 0 ? commandIn(text) : null;
    if (command !== null) {
      // A command given what it does not take is the gateway's to answer.
      const known = command.bare
        ? commands.find((c) => c.name === command.name || c.aliases.includes(command.name))
        : undefined;
      if (known?.name === "new") {
        openChatById(null);
        return;
      }
      if (known?.name === "stop") {
        stopReply();
        return;
      }
      if (known?.name === "retry") {
        const last = [...bubbles].map((b, i) => [b, i] as const).reverse().find(([b]) => b.role === "agent" && b.turn);
        if (last) regenerate(last[1], last[0].turn!);
        else pushBubble("system", "There is no reply here to answer again.");
        return;
      }
      pushBubble("system", text.trim());
      if (command.bare && command.name === "undo") undoing.current += 1;
      chatRef.current.send(text.trim(), chatIdRef.current ?? undefined);
      return;
    }
    // In the shape of the stored turn, which shows the same.
    pushBubble("you", joinMessage(text, files));
    chatRef.current.send(text, chatIdRef.current ?? undefined, undefined, files);
    // Text left from a stopped reply goes before a new one starts.
    if (waiting === 0) clearLive();
    setWaiting((n) => n + 1);
  }

  // The chat commands, for the composer's suggestions.
  const [commands, setCommands] = useState<ChatCommand[]>([]);
  useEffect(() => {
    api
      .commands()
      .then(setCommands)
      .catch((e) => console.warn("[chat] commands unavailable", e));
  }, []);

  // Files waiting to go with the next message, uploaded already.
  const [attached, setAttached] = useState<Attachment[]>([]);
  const [uploading, setUploading] = useState(0);
  // Upload `picked` to the agent's inbox, as many as `room` leaves, each
  // as it goes: `done` gets each one uploaded.
  async function uploadFiles(picked: File[], room: number, done: (a: Attachment) => void) {
    if (picked.length > room) {
      pushBubble("system", `error: a message carries up to ${MAX_ATTACHMENTS} files`);
    }
    const batch = picked.slice(0, Math.max(0, room)).filter((file) => {
      if (file.size <= MAX_UPLOAD_BYTES) return true;
      pushBubble("system", `error: ${file.name} is over ${MAX_UPLOAD_BYTES / (1024 * 1024)} MB`);
      return false;
    });
    for (const file of batch) {
      try {
        done(await api.files.upload(id, file));
      } catch (e) {
        pushBubble("system", `error: could not attach ${file.name}: ${e instanceof Error ? e.message : String(e)}`);
      }
    }
  }

  async function attachFiles(files: File[]) {
    if (files.length === 0) return;
    // The whole pick counts at once, so a second pick cannot pass the limit.
    const room = MAX_ATTACHMENTS - attached.length - uploading;
    const counted = Math.min(files.length, Math.max(0, room));
    setUploading((n) => n + counted);
    try {
      await uploadFiles(files, room, (a) => setAttached((prev) => [...prev, a]));
    } finally {
      setUploading((n) => n - counted);
    }
  }

  function stopReply() {
    chatRef.current?.stop(chatIdRef.current ?? undefined);
  }

  // A new version of the user turn at `idx`: what came after it leaves
  // the screen, and stays stored on its own branch.
  function editMessage(idx: number, turn: string, text: string, files: Attachment[]) {
    if ((!text.trim() && files.length === 0) || !chatRef.current || wsState !== "open") return;
    // Memory notes stay: what the replies changed is still in memory.
    setBubbles((prev) => [...prev.slice(0, idx), ...prev.slice(idx).filter((b) => b.memory)]);
    pushBubble("you", joinMessage(text, files));
    chatRef.current.send(text, chatIdRef.current ?? undefined, turn, files);
    // Text left from a stopped reply goes before a new one starts.
    if (waiting === 0) clearLive();
    setWaiting((n) => n + 1);
  }

  // Another reply in place of the one at `idx`.
  function regenerate(idx: number, turn: string) {
    if (!chatRef.current || wsState !== "open") return;
    // Memory notes stay: what the replies changed is still in memory.
    setBubbles((prev) => [...prev.slice(0, idx), ...prev.slice(idx).filter((b) => b.memory)]);
    chatRef.current.regenerate(chatIdRef.current ?? undefined, turn);
    // Text left from a stopped reply goes before a new one starts.
    if (waiting === 0) clearLive();
    setWaiting((n) => n + 1);
  }

  // Show the branch of version `turn`.
  async function showVersion(turn: string) {
    const current = chatIdRef.current;
    if (!current) return;
    try {
      await api.chats.branch(id, current, turn);
      const r = await api.conversation.list(id, HISTORY_LIMIT, current);
      if (chatIdRef.current !== current) return;
      const stored = turnBubbles(r.turns, () => bubbleIdRef.current++);
      // Notes of replies on other branches stay with them.
      setBubbles((prev) =>
        [
          ...stored,
          ...prev
            .filter((b) => b.role === "system" && (!b.anchor || stored.some((s) => s.turn === b.anchor)))
            .map((b) => underAnchor(b, stored)),
        ].sort((a, b) => a.ts - b.ts),
      );
    } catch (e) {
      pushBubble("system", `error: ${e instanceof Error ? e.message : String(e)}`);
    }
  }

  // Show chat `target`, and in it the found message `turn` (see the
  // effect above); a found title only opens its chat.
  function openFound(target: string, turn: string | null) {
    jumpTo.current = turn ? { chat: target, turn, branched: false } : null;
    if (target === chatIdRef.current) {
      // Already on screen: the effect runs on what is shown.
      setBubbles((prev) => [...prev]);
    } else {
      openChatById(target);
    }
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
    setStep(null);
    clearLive();
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
      setStep(null);
      endLive();
    }
    void refreshChats();
  }

  function decide(id: string, approve: boolean, remember: boolean) {
    // The gateway closes the card in every tab once it took the answer.
    if (!chatRef.current || wsState !== "open") return;
    chatRef.current.decide(id, approve, remember);
  }

  function onSubmit(e?: React.FormEvent) {
    e?.preventDefault();
    if (uploading > 0) return;
    sendText(draft, attached);
    setDraft("");
    setAttached([]);
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
        onFound={(chat, turn) => {
          setShowChats(false);
          void openFound(chat, turn);
        }}
        agentId={id}
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

        {agent.failure && wsState !== "open" && (
          <div
            role="alert"
            className="border-b px-4 py-3 text-sm sm:px-8"
            style={{
              borderColor: "rgba(234, 34, 97, 0.3)",
              backgroundColor: "rgba(234, 34, 97, 0.06)",
              color: "var(--color-ruby)",
            }}
          >
            <p className="mb-1 font-medium">The agent exited before it was ready. Its runtime said:</p>
            <pre className="overflow-x-auto whitespace-pre-wrap font-mono text-xs">{agent.failure}</pre>
          </div>
        )}

        {/* Conversation */}
        <div
          ref={scrollRef}
          onScroll={(e) => {
            const el = e.currentTarget;
            atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 80;
          }}
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
            {bubbles.map((b, idx) =>
              b.file ? (
                <FileCard key={b.id} url={b.file.url} name={b.file.name} caption={b.file.caption} />
              ) : b.memory ? (
                <MemoryNote key={b.id} agentId={id} changes={b.memory} />
              ) : (
              <Bubble
                key={b.id}
                agentId={id}
                bubble={b}
                found={b.turn !== undefined && b.turn === found}
                busy={pending || wsState !== "open"}
                onUpload={uploadFiles}
                onEdit={
                  b.role === "you" && b.turn
                    ? (text, files) => editMessage(idx, b.turn!, text, files)
                    : undefined
                }
                onRegenerate={
                  b.role === "agent" && b.turn ? () => regenerate(idx, b.turn!) : undefined
                }
                onVersion={(turn) => void showVersion(turn)}
                onRetry={
                  b.role === "agent" && !b.turn
                    ? () => {
                        // Find the most recent user bubble before this
                        // agent reply and re-send its content. Both the
                        // original and the retry stay in history — the
                        // agent then has both attempts in context, which
                        // is correct (it can see what didn't work).
                        for (let i = idx - 1; i >= 0; i--) {
                          if (bubbles[i].role === "you") {
                            const again = splitMessage(bubbles[i].content);
                            sendText(again.text, again.files);
                            return;
                          }
                        }
                      }
                    : undefined
                }
              />
              ),
            )}
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
            {pending && live.some((p) => p.text.trim() !== "") && (
              <LiveBubble text={live.map((p) => p.text.trim()).filter(Boolean).join("\n\n")} />
            )}
            {pending && <TypingBubble step={step} />}
          </div>
        </div>

        {/* Composer */}
        <Composer
          agentId={id}
          wsState={wsState}
          waiting={waiting}
          onStop={stopReply}
          draft={draft}
          setDraft={setDraft}
          onSubmit={onSubmit}
          attached={attached}
          uploading={uploading}
          onAttach={(files) => void attachFiles(files)}
          onDetach={(path) => setAttached((prev) => prev.filter((a) => a.path !== path))}
          commands={commands}
        />
      </div>
    </div>
  );
}

function ChatList({
  agentId,
  chats,
  current,
  onOpen,
  onRename,
  onDelete,
  onFound,
  className,
}: {
  agentId: string;
  chats: ChatSummary[];
  current: string | null;
  onOpen: (chat: string | null) => void;
  onRename: (chat: ChatSummary) => void;
  onDelete: (chat: ChatSummary) => void;
  /** A search result: its chat, and the message to show. */
  onFound: (chat: string, turn: string | null) => void;
  className: string;
}) {
  const fresh = current !== null && !chats.some((c) => c.id === current);
  const [query, setQuery] = useState("");
  const [results, setResults] = useState<ChatSearch | null>(null);
  const [searchError, setSearchError] = useState<string | null>(null);
  // A search once typing pauses; a newer one replaces it.
  useEffect(() => {
    const q = query.trim();
    if (!q) return;
    let cancelled = false;
    const timer = setTimeout(() => {
      api.chats.search(agentId, q).then(
        (r) => {
          if (!cancelled) {
            setResults(r);
            setSearchError(null);
          }
        },
        (e: unknown) => {
          if (!cancelled) setSearchError(e instanceof Error ? e.message : String(e));
        },
      );
    }, 300);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  }, [agentId, query]);
  const searching = query.trim() !== "";
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
        <input
          type="search"
          className="input mt-2 w-full text-sm"
          placeholder="search chats"
          aria-label="search chats"
          value={query}
          maxLength={200}
          onChange={(e) => {
            setQuery(e.target.value);
            // A new search shows nothing found by the last one.
            setResults(null);
            setSearchError(null);
          }}
          onKeyDown={(e) => {
            if (e.key === "Escape") setQuery("");
          }}
        />
      </div>
      {searching ? (
        <SearchResults
          query={query.trim()}
          results={results}
          error={searchError}
          onFound={onFound}
        />
      ) : (
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
      )}
    </aside>
  );
}

/** `text` with each occurrence of a word of `query` marked. */
function Marked({ text, query }: { text: string; query: string }) {
  const words = query
    .split(/\s+/)
    .filter((w) => w.length > 0)
    .map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  if (words.length === 0) return <>{text}</>;
  const parts = text.split(new RegExp(`(${words.join("|")})`, "gi"));
  return (
    <>
      {parts.map((part, i) =>
        i % 2 === 1 ? (
          <mark key={i} style={{ backgroundColor: "var(--color-highlight, #fde68a)", color: "inherit" }}>
            {part}
          </mark>
        ) : (
          part
        ),
      )}
    </>
  );
}

function SearchResults({
  query,
  results,
  error,
  onFound,
}: {
  query: string;
  results: ChatSearch | null;
  error: string | null;
  onFound: (chat: string, turn: string | null) => void;
}) {
  const muted = { color: "var(--color-body)" };
  if (error) {
    return (
      <p className="px-4 text-xs" role="alert" style={{ color: "var(--color-ruby)" }}>
        {error}
      </p>
    );
  }
  if (!results) {
    return (
      <p className="px-4 text-xs" style={muted}>
        searching…
      </p>
    );
  }
  if (results.chats.length === 0 && results.turns.length === 0) {
    return (
      <p className="px-4 text-xs" style={muted}>
        nothing found
      </p>
    );
  }
  return (
    <nav aria-label="search results" className="flex-1 overflow-y-auto px-2 pb-3">
      {results.chats.length > 0 && (
        <>
          <h3 className="px-2 pt-1 text-xs uppercase" style={muted}>
            titles
          </h3>
          <ul className="flex flex-col gap-0.5">
            {results.chats.map((c) => (
              <li key={c.id}>
                <button
                  type="button"
                  className="w-full truncate rounded px-2 py-1.5 text-left text-sm"
                  onClick={() => onFound(c.id, null)}
                  title={c.title}
                >
                  <Marked text={c.title || "untitled"} query={query} />
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
      {results.turns.length > 0 && (
        <>
          <h3 className="px-2 pt-2 text-xs uppercase" style={muted}>
            messages
          </h3>
          <ul className="flex flex-col gap-1">
            {results.turns.map((t) => (
              <li key={t.id}>
                <button
                  type="button"
                  className="w-full rounded px-2 py-1.5 text-left"
                  onClick={() => onFound(t.chat, t.id)}
                >
                  <span className="block truncate text-xs" style={muted}>
                    {t.title || "untitled"} · {t.role === "user" ? "you" : "agent"} ·{" "}
                    {new Date(t.created_at).toLocaleDateString()}
                  </span>
                  <span className="line-clamp-3 block text-sm">
                    <Marked text={t.excerpt} query={query} />
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </>
      )}
    </nav>
  );
}

function Bubble({
  bubble,
  busy,
  found,
  agentId,
  onRetry,
  onEdit,
  onUpload,
  onRegenerate,
  onVersion,
}: {
  bubble: Bubble;
  agentId: string;
  /** A reply is due: branching waits for it. */
  busy: boolean;
  /** The message a search led to: marked for a moment. */
  found?: boolean;
  onRetry?: () => void;
  onEdit?: (text: string, files: Attachment[]) => void;
  onUpload: (picked: File[], room: number, done: (a: Attachment) => void) => Promise<void>;
  onRegenerate?: () => void;
  onVersion: (turn: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const align =
    bubble.role === "you" ? "items-end" : "items-start";
  // A message of the user's: its text, its files shown above it.
  const message = bubble.role === "you" ? splitMessage(bubble.content) : null;
  if (editing && message) {
    return (
      <div className="flex flex-col items-end">
        <MessageEditor
          agentId={agentId}
          text={message.text}
          files={message.files}
          busy={busy}
          onUpload={onUpload}
          onCancel={() => setEditing(false)}
          onSend={(text, files) => {
            setEditing(false);
            onEdit?.(text, files);
          }}
        />
      </div>
    );
  }
  // System bubbles stay plain — markdown renderer is overkill for
  // "agent reconnecting…" hints. User + agent get full markdown +
  // a below-bubble action row (copy / retry).
  const isSystem = bubble.role === "system";
  return (
    <div
      className={`flex flex-col gap-1.5 ${align}`}
      data-turn={bubble.turn}
      style={
        found
          ? { outline: "2px solid var(--color-stripe-purple)", outlineOffset: 4, borderRadius: 8 }
          : undefined
      }
    >
      {message && message.files.length > 0 && (
        <AttachmentCards agentId={agentId} files={message.files} align="end" />
      )}
      {(!message || message.text) && (
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
          <Markdown>{message ? message.text : bubble.content}</Markdown>
        )}
      </div>
      )}
      {!isSystem && (
        <ActionRow
          text={message ? message.text : bubble.content}
          onRetry={onRetry}
          align={bubble.role === "you" ? "end" : "start"}
        >
          {onEdit && (
            <RowButton
              onClick={() => setEditing(true)}
              disabled={busy}
              title="Edit and send again; the messages after it stay on their own branch. What tools already did (files written, messages sent) is not undone."
              label="edit"
            >
              <Pencil size={12} className="shrink-0" aria-hidden="true" />
              edit
            </RowButton>
          )}
          {onRegenerate && (
            <RowButton
              onClick={onRegenerate}
              disabled={busy}
              title="Answer your last message before this reply again; this reply stays as another version. What tools already did is not undone."
              label="regenerate"
            >
              <RefreshCw size={12} className="shrink-0" aria-hidden="true" />
              regenerate
            </RowButton>
          )}
          {bubble.turn && bubble.versions && bubble.versions.length > 1 && (
            <Versions turn={bubble.turn} versions={bubble.versions} busy={busy} onVersion={onVersion} />
          )}
        </ActionRow>
      )}
    </div>
  );
}

/** Files a message carries, as cards: images as thumbnails. */
function AttachmentCards({
  agentId,
  files,
  onRemove,
  align = "start",
}: {
  agentId: string;
  files: Attachment[];
  /** Remove the file at this index. */
  onRemove?: (index: number) => void;
  align?: "start" | "end";
}) {
  return (
    <div className={`flex flex-wrap gap-2 ${align === "end" ? "justify-end" : ""}`} aria-label="attached files">
      {files.map((f, i) => (
        <AttachmentCard
          key={`${i}:${f.path}`}
          agentId={agentId}
          file={f}
          onRemove={onRemove ? () => onRemove(i) : undefined}
        />
      ))}
    </div>
  );
}

function AttachmentCard({
  agentId,
  file,
  onRemove,
}: {
  agentId: string;
  file: Attachment;
  onRemove?: () => void;
}) {
  // The gateway shows an inbox image in place and gives anything else as a
  // download.
  const url = `/api/agents/${agentId}/files?path=${encodeURIComponent(file.path)}`;
  const image = /^image\/(png|jpeg|gif|webp)$/.test(file.media_type);
  const dot = file.name.lastIndexOf(".");
  const kind = (dot > 0 ? file.name.slice(dot + 1) : file.media_type.split("/").pop() || "file")
    .toUpperCase()
    .slice(0, 6);
  return (
    <div className="relative">
      {image ? (
        <a
          href={url}
          target="_blank"
          rel="noreferrer"
          title={file.name}
          className="block overflow-hidden"
          style={{
            width: 96,
            height: 96,
            borderRadius: "var(--radius-large)",
            border: "1px solid var(--color-border-default)",
          }}
        >
          {/* eslint-disable-next-line @next/next/no-img-element -- an inbox file, served by the gateway */}
          <img src={url} alt={file.name} loading="lazy" className="h-full w-full object-cover" />
        </a>
      ) : (
        <a
          href={url}
          download={file.name}
          title={`${file.name}${file.size ? ` (${humanSize(file.size)})` : ""}`}
          className="card flex items-center gap-2 px-3 py-2"
          style={{ width: 208, backgroundColor: "var(--color-surface)" }}
        >
          <FileText size={20} className="shrink-0" aria-hidden="true" style={{ color: "var(--color-body)" }} />
          <span className="min-w-0">
            <span className="block truncate text-sm" style={{ color: "var(--color-deep-navy)" }}>
              {file.name}
            </span>
            <span className="block text-xs" style={{ color: "var(--color-body)" }}>
              {kind}
              {file.size ? ` · ${humanSize(file.size)}` : ""}
            </span>
          </span>
        </a>
      )}
      {onRemove && (
        <button
          type="button"
          onClick={onRemove}
          aria-label={`remove ${file.name}`}
          title="Remove it from the message (it stays in the agent's inbox/)"
          className="absolute -right-1.5 -top-1.5 rounded-full p-0.5"
          style={{ backgroundColor: "var(--color-deep-navy)", color: "white" }}
        >
          <X size={12} aria-hidden="true" />
        </button>
      )}
    </div>
  );
}

/**
 * A text box that grows with its text. Enter calls `onEnter` and
 * Shift+Enter starts a new line, except while an input method composes
 * (Chinese, Japanese, Korean), whose Enter confirms what it composes.
 * Files pasted go to `onFiles`.
 */
/** The command `text` is (lowercase, without the slash) and whether
 * nothing follows it, or `null` for an ordinary message. */
function commandIn(text: string): { name: string; bare: boolean } | null {
  const m = /^\/([A-Za-z0-9_]{1,32})(?:@[A-Za-z0-9_]+)?(?:\s+(.*))?$/.exec(text.trim());
  // `/start` (Telegram's first message) goes to the agent, as the gateway
  // sends it.
  if (!m || text.trim().includes("\n") || m[1].toLowerCase() === "start") return null;
  return { name: m[1].toLowerCase(), bare: !m[2] };
}

function MessageInput({
  value,
  onChange,
  onEnter,
  onEscape,
  onFiles,
  disabled,
  placeholder,
  autoFocus,
  label,
}: {
  value: string;
  onChange: (value: string) => void;
  onEnter: () => void;
  onEscape?: () => void;
  onFiles?: (files: File[]) => void;
  disabled?: boolean;
  placeholder?: string;
  autoFocus?: boolean;
  label: string;
}) {
  const ref = useRef<HTMLTextAreaElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight + 2, 320)}px`;
  }, [value]);
  return (
    <textarea
      ref={ref}
      value={value}
      onChange={(e) => onChange(e.target.value)}
      onKeyDown={(e) => {
        // What an input method composes takes its keys first.
        if (e.nativeEvent.isComposing || e.keyCode === 229) return;
        if (e.key === "Escape" && onEscape) {
          e.preventDefault();
          onEscape();
          return;
        }
        if (e.key !== "Enter" || e.shiftKey) return;
        // A touch keyboard has no Shift: its Enter starts a line, and the
        // send button sends.
        if (!(e.metaKey || e.ctrlKey) && window.matchMedia("(pointer: coarse)").matches) return;
        e.preventDefault();
        onEnter();
      }}
      onPaste={(e) => {
        const files = Array.from(e.clipboardData.files);
        // Text copied with a picture (from a document) stays text.
        if (files.length > 0 && onFiles && !e.clipboardData.getData("text/plain")) {
          e.preventDefault();
          onFiles(files);
        }
      }}
      disabled={disabled}
      placeholder={placeholder}
      autoFocus={autoFocus}
      aria-label={label}
      rows={2}
      className="input w-full"
      style={{
        resize: "none",
        overflowY: "auto",
        minHeight: 56,
        maxHeight: 320,
        fontFamily: "inherit",
        padding: "10px 12px",
        lineHeight: 1.45,
      }}
    />
  );
}

/**
 * Editing a message: its text and its files, which can be removed and
 * added to. Send waits for a change.
 */
function MessageEditor({
  agentId,
  text,
  files,
  busy,
  onUpload,
  onCancel,
  onSend,
}: {
  agentId: string;
  text: string;
  files: Attachment[];
  busy: boolean;
  onUpload: (picked: File[], room: number, done: (a: Attachment) => void) => Promise<void>;
  onCancel: () => void;
  onSend: (text: string, files: Attachment[]) => void;
}) {
  const [draft, setDraft] = useState(text);
  const [kept, setKept] = useState(files);
  const [uploading, setUploading] = useState(0);
  const fileInput = useRef<HTMLInputElement>(null);
  const unchanged =
    draft.trim() === text.trim() &&
    kept.length === files.length &&
    kept.every((f, i) => f.path === files[i].path);
  const empty = !draft.trim() && kept.length === 0;
  const blocked = busy || uploading > 0 || unchanged || empty;
  async function add(picked: File[]) {
    if (picked.length === 0) return;
    const room = MAX_ATTACHMENTS - kept.length - uploading;
    const counted = Math.min(picked.length, Math.max(0, room));
    setUploading((n) => n + counted);
    try {
      await onUpload(picked, room, (a) => setKept((prev) => [...prev, a]));
    } finally {
      setUploading((n) => n - counted);
    }
  }
  function send() {
    if (!blocked) onSend(draft, kept);
  }
  return (
    <form
      className="card flex w-full flex-col gap-2 p-3"
      style={{ maxWidth: "min(85%, 720px)", minWidth: "min(100%, 360px)", backgroundColor: "var(--color-surface)" }}
      onSubmit={(e) => {
        e.preventDefault();
        send();
      }}
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes("Files")) e.preventDefault();
      }}
      onDrop={(e) => {
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        void add(Array.from(e.dataTransfer.files));
      }}
    >
      {kept.length > 0 && (
        <AttachmentCards
          agentId={agentId}
          files={kept}
          onRemove={(index) => setKept((prev) => prev.filter((_, i) => i !== index))}
        />
      )}
      <MessageInput
        value={draft}
        onChange={setDraft}
        onEnter={send}
        onEscape={onCancel}
        onFiles={(picked) => void add(picked)}
        autoFocus
        label="edit message"
      />
      <div className="flex items-center gap-2">
        <input
          ref={fileInput}
          type="file"
          multiple
          hidden
          onChange={(e) => {
            void add(Array.from(e.target.files ?? []));
            e.target.value = "";
          }}
        />
        <button
          type="button"
          className="btn-ghost mr-auto inline-flex items-center gap-1.5"
          onClick={() => fileInput.current?.click()}
          disabled={kept.length + uploading >= MAX_ATTACHMENTS}
          title="Attach files to the message"
        >
          <Paperclip size={16} className="shrink-0" aria-hidden="true" />
          attach
        </button>
        {uploading > 0 && (
          <span className="inline-flex items-center gap-1 text-xs" role="status" style={{ color: "var(--color-body)" }}>
            <Spinner /> uploading
          </span>
        )}
        <button type="button" className="btn-ghost" onClick={onCancel}>
          cancel
        </button>
        <button
          type="submit"
          className="btn-primary"
          disabled={blocked}
          title={unchanged ? "Change the text or the files to send it again" : undefined}
        >
          send
        </button>
      </div>
    </form>
  );
}

/** A file the agent sent, as a download. */
function FileCard({ url, name, caption }: { url: string; name: string; caption?: string }) {
  return (
    <div className="flex flex-col items-start gap-1 self-start">
      {caption && (
        <p className="text-sm" style={{ whiteSpace: "pre-wrap" }}>
          {caption}
        </p>
      )}
      <a
        href={`/api${url}`}
        download={name}
        className="card inline-flex items-center gap-2 px-3 py-2 text-sm"
        title="Download the file the agent sent"
      >
        <Paperclip size={14} className="shrink-0" aria-hidden="true" />
        <span className="wrap-anywhere">{name}</span>
      </a>
    </div>
  );
}

/** What a reply saved to or forgot from memory, each with an undo. */
function MemoryNote({ agentId, changes }: { agentId: string; changes: MemoryChange[] }) {
  return (
    <div
      className="self-start rounded px-3 py-2 text-xs"
      role="status"
      style={{ background: "rgba(83, 58, 253, 0.06)", color: "var(--color-body)" }}
    >
      {changes.map((c, i) => (
        <MemoryChangeLine key={`${c.key}-${i}`} agentId={agentId} change={c} />
      ))}
    </div>
  );
}

function MemoryChangeLine({ agentId, change }: { agentId: string; change: MemoryChange }) {
  const [state, setState] = useState<"shown" | "undoing" | "undone">("shown");
  const [error, setError] = useState<string | null>(null);
  const saved = change.action === "saved";

  async function undo() {
    setState("undoing");
    setError(null);
    try {
      // By row: the entry as it was comes back, and only if nothing
      // changed the key since.
      await api.memory.undo(agentId, change.key, change.before, change.after);
      setState("undone");
    } catch (e) {
      setState("shown");
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <div className="flex flex-wrap items-baseline gap-x-2">
      <Brain size={12} className="shrink-0 self-center" aria-hidden="true" />
      <span>
        {saved ? "Saved to memory" : "Forgot"} <code className="font-mono">{change.key}</code>
        {": "}
        <span style={{ textDecoration: state === "undone" ? "line-through" : undefined }}>
          {change.value}
        </span>
      </span>
      {state === "undone" ? (
        <span>(undone)</span>
      ) : (
        <button
          type="button"
          className="underline disabled:opacity-40"
          onClick={() => void undo()}
          disabled={state === "undoing"}
          title={
            saved
              ? change.before
                ? "Put back what it replaced"
                : "Forget this memory"
              : "Bring it back as it was"
          }
        >
          {state === "undoing" ? "undoing…" : "undo"}
        </button>
      )}
      {error && (
        <span role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </span>
      )}
    </div>
  );
}

function Versions({
  turn,
  versions,
  busy,
  onVersion,
}: {
  turn: string;
  versions: string[];
  busy: boolean;
  onVersion: (turn: string) => void;
}) {
  const at = versions.indexOf(turn);
  return (
    <span className="inline-flex items-center gap-0.5" aria-label="versions">
      <RowButton
        onClick={() => onVersion(versions[at - 1])}
        disabled={busy || at <= 0}
        title="previous version"
        label="previous version"
      >
        <ChevronLeft size={12} className="shrink-0" aria-hidden="true" />
      </RowButton>
      <span>
        {at + 1}/{versions.length}
      </span>
      <RowButton
        onClick={() => onVersion(versions[at + 1])}
        disabled={busy || at >= versions.length - 1}
        title="next version"
        label="next version"
      >
        <ChevronRight size={12} className="shrink-0" aria-hidden="true" />
      </RowButton>
    </span>
  );
}

function RowButton({
  onClick,
  disabled,
  title,
  label,
  children,
}: {
  onClick: () => void;
  disabled?: boolean;
  title: string;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={label}
      className="inline-flex items-center gap-1 rounded px-1.5 py-0.5 transition-colors disabled:opacity-40"
      style={{ background: "transparent" }}
    >
      {children}
    </button>
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
  children,
}: {
  text: string;
  onRetry?: () => void;
  align: "start" | "end";
  children?: React.ReactNode;
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
      {children}
    </div>
  );
}

/**
 * Inline "agent is replying" placeholder. Three pulsing dots in an
 * agent-shaped bubble — same styling as a real agent message so the
 * eye doesn't have to retrain. Replaced by the actual reply when
 * `agent_message` arrives (parent flips `pending` to false).
 */
/** The reply as the model writes it, before the reply itself comes. */
function LiveBubble({ text }: { text: string }) {
  return (
    <div className="flex flex-col items-start">
      <div
        className="px-4 py-3"
        style={{
          maxWidth: "min(85%, 720px)",
          borderRadius: "var(--radius-large)",
          backgroundColor: "var(--color-surface)",
          color: "var(--color-deep-navy)",
          border: "1px solid var(--color-border-default)",
          boxShadow: "var(--shadow-ambient)",
        }}
        aria-live="polite"
      >
        <Markdown>{text}</Markdown>
      </div>
    </div>
  );
}

/** A tool call the agent makes while it replies. */
type Step = { tool: string; detail: string };

/** What a step does, as the chat shows it. */
function stepText({ tool, detail }: Step): string {
  const verbs: Record<string, string> = {
    web_search: "Searching the web",
    web_fetch: "Reading a page",
    shell: "Running",
    file_read: "Reading",
    file_write: "Writing",
    file_edit: "Editing",
    memory_search: "Searching memory",
    memory_remember: "Remembering",
    memory_forget: "Forgetting",
    memory_store: "Remembering",
    skill_manage: "Updating a skill",
    conversation_search: "Searching past chats",
    send_file: "Sending",
    schedule: "Scheduling",
    set_timezone: "Setting the time zone",
    subagent_spawn: "Handing off a task",
    agent_query: "Asking another agent",
  };
  const verb = verbs[tool] ?? `Using ${tool}`;
  return detail ? `${verb}: ${detail}` : verb;
}

function TypingBubble({ step }: { step: Step | null }) {
  return (
    <div className="flex flex-col items-start gap-1">
      {step && (
        <p className="wrap-anywhere px-1 text-xs" style={{ color: "var(--color-body)" }} role="status">
          {stepText(step)}
        </p>
      )}
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
  agentId,
  wsState,
  waiting,
  onStop,
  draft,
  setDraft,
  onSubmit,
  attached,
  uploading,
  onAttach,
  onDetach,
  commands,
}: {
  agentId: string;
  wsState: "idle" | "connecting" | "open" | "closed" | "error";
  waiting: number;
  onStop: () => void;
  draft: string;
  setDraft: (s: string) => void;
  onSubmit: (e?: React.FormEvent) => void;
  /** Files uploaded for the next message, and uploads still running. */
  attached: Attachment[];
  uploading: number;
  onAttach: (files: File[]) => void;
  onDetach: (path: string) => void;
  commands: ChatCommand[];
}) {
  const fileInput = useRef<HTMLInputElement>(null);
  // Commands that start with what is typed after a slash, before any
  // space: picking one puts it in the box.
  const typed = /^\/([A-Za-z0-9_]*)$/.exec(draft.trim())?.[1]?.toLowerCase();
  const suggested =
    typed === undefined || attached.length > 0
      ? []
      : commands.filter((c) => c.name.startsWith(typed) || c.aliases.some((a) => a.startsWith(typed)));
  const canSend = (draft.trim() !== "" || attached.length > 0) && uploading === 0;
  const [tab, setTab] = useState<"write" | "preview">("write");
  // The composer is ready whenever the WS is up. A message sent while a
  // reply runs waits in the agent and is answered after it, in order.
  const ready = wsState === "open";
  const pending = waiting > 0;
  return (
    <form
      onSubmit={onSubmit}
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes("Files")) e.preventDefault();
      }}
      onDrop={(e) => {
        if (!e.dataTransfer.types.includes("Files")) return;
        e.preventDefault();
        if (ready) onAttach(Array.from(e.dataTransfer.files));
      }}
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
            <kbd className="font-mono" style={{ fontSize: 11 }}>
              Enter
            </kbd>{" "}
            to send ·{" "}
            <kbd className="font-mono" style={{ fontSize: 11 }}>
              Shift+Enter
            </kbd>{" "}
            new line · markdown
          </span>
        </div>

        {(attached.length > 0 || uploading > 0) && (
          <div className="mb-2 flex flex-wrap items-center gap-2">
            <AttachmentCards agentId={agentId} files={attached} onRemove={(i) => onDetach(attached[i].path)} />
            {uploading > 0 && (
              <span className="inline-flex items-center gap-1 text-xs" role="status" style={{ color: "var(--color-body)" }}>
                <Spinner /> uploading
              </span>
            )}
          </div>
        )}

        {suggested.length > 0 && tab === "write" && (
          <ul
            className="card mb-2 overflow-y-auto py-1 text-sm"
            role="listbox"
            aria-label="Commands"
            style={{ maxHeight: 240, backgroundColor: "var(--color-surface)" }}
          >
            {suggested.map((c) => (
              <li key={c.name}>
                <button
                  type="button"
                  role="option"
                  aria-selected={false}
                  className="flex w-full items-baseline gap-2 px-3 py-1.5 text-left"
                  // The text box keeps the focus: Enter then sends.
                  onMouseDown={(e) => e.preventDefault()}
                  onClick={() => setDraft(`/${c.name}${c.args ? " " : ""}`)}
                >
                  <span className="font-mono" style={{ color: "var(--color-deep-navy)" }}>
                    /{c.name}
                    {c.args && <span style={{ color: "var(--color-body)" }}> {c.args}</span>}
                  </span>
                  <span style={{ color: "var(--color-body)" }}>
                    {c.about}
                    {c.admin && " (owner only)"}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}

        {/* Body */}
        {tab === "write" ? (
          <MessageInput
            placeholder={
              ready
                ? "Message the agent: Enter sends, Shift+Enter starts a new line, files can be pasted or dropped"
                : "Waiting for agent connection…"
            }
            value={draft}
            onChange={setDraft}
            onEnter={() => {
              if (ready && canSend) onSubmit();
            }}
            onFiles={(files) => {
              if (ready) onAttach(files);
            }}
            disabled={!ready}
            label="message"
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
          <input
            ref={fileInput}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              onAttach(Array.from(e.target.files ?? []));
              e.target.value = "";
            }}
          />
          <button
            type="button"
            className="btn-ghost mr-auto inline-flex items-center gap-1.5"
            onClick={() => fileInput.current?.click()}
            disabled={!ready}
            title="Attach files: they are saved in the agent's inbox/ and sent with your message (up to 25 MB each)"
          >
            <Paperclip size={16} className="shrink-0" aria-hidden="true" />
            attach
          </button>
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
            disabled={!ready || !canSend}
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
