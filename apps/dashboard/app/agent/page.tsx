"use client";

import { useCallback, useEffect, useId, useRef, useState } from "react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import {
  api,
  type AgentView,
  type AgentModels,
  type ChatCommand,
  type ChatSearch,
  thinkingChoices,
  modelName,
  type ChannelChat,
  type ChatSummary,
  type ConversationTurn,
  type MeView,
  type SentFile,
  type TraceItem,
  type TraceSummary,
  type TurnTrace,
} from "@/lib/api";
import { openChat, type ChatHandle } from "@/lib/websocket";
import { Markdown } from "@/components/Markdown";
import type {
  Attachment,
  ChoiceButton,
  ChoicePoll,
  MemoryChange,
  ServerMessage,
} from "@/lib/protocol";
import {
  ArrowDown,
  ArrowUp,
  BookOpen,
  Bot,
  Brain,
  CalendarClock,
  Check,
  Copy,
  Database,
  Eye,
  FileText,
  FolderOpen,
  Globe,
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
  Square,
  Terminal,
  Download,
  Trash2,
  Wrench,
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
  /** For a reply: what its turn did on the way, in short. */
  trace?: TraceSummary;
  /** A file the agent sent, to download. */
  file?: { url: string; name: string; caption?: string };
  /** A note of what a reply saved to or forgot from memory, and that
   * reply's stored turn, which it stays under. */
  memory?: MemoryChange[];
  anchor?: string;
  /** A question the agent asked (issue #204): buttons, or a poll of
   * several answers, and whether it was answered here. */
  buttons?: ChoiceButton[][];
  poll?: ChoicePoll;
  answered?: boolean;
  /** New while the page is open, not loaded from history: it fades in. */
  fresh?: boolean;
};

const AGENT_POLL_MS = 5000;
/** What the gateway takes: files per message, and bytes per file. */
const MAX_ATTACHMENTS = 10;
const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

// A turn's attachment lines, which tell the agent where each file is, as
// the runtime writes them.
const JSON_STRING = String.raw`"(?:[^"\\]|\\.)*"`;
const SCANNED = "; little or no text could be read from it, so it is likely a scan: ";
const ATTACHMENT_LINE = new RegExp(
  String.raw`^\[Attached file, user-supplied and untrusted: name (${JSON_STRING}), type (${JSON_STRING}), ([^,\]]+), saved at (${JSON_STRING})(?:${SCANNED}((?:[^"\]]|${JSON_STRING})*?))?(?:; its text \(untrusted\) is at (${JSON_STRING}), read it with file_read)?\]$`,
);

/** A scanned PDF's pages, as its line says them. */
function scanOf(told: string): { pages: number; shown: number } {
  const many = /^pages 1 to (\d+) of (\d+) /.exec(told);
  if (many) return { shown: Number(many[1]), pages: Number(many[2]) };
  const one = /^page 1 of (\d+) /.exec(told);
  if (one) return { shown: 1, pages: Number(one[1]) };
  return { shown: 0, pages: 0 };
}

/** What a scanned PDF's line says of its pages, as the runtime writes it. */
function scanText(a: Attachment): string {
  if (!a.scan) return "";
  const { pages, shown } = a.scan;
  const slash = a.path.lastIndexOf("/");
  const at = (n: number) =>
    JSON.stringify(`${a.path.slice(0, slash + 1)}.${a.path.slice(slash + 1)}.page-${n}.jpg`);
  const told =
    shown === 0
      ? "its pages could not be made images"
      : shown === 1
        ? `page 1 of ${pages} is an image at ${at(1)}, to look at with view_image`
        : `pages 1 to ${shown} of ${pages} are images at ${at(1)} to ${at(shown)}, to look at with view_image`;
  return `${SCANNED}${told}`;
}

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
        ...(m[5] !== undefined ? { scan: scanOf(m[5]) } : {}),
        ...(m[6] ? { text_path: JSON.parse(m[6]) as string } : {}),
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
    scanText(a) +
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
      trace: t.trace,
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
  // A question the agent asked is not a stored turn: it stays (issue #204).
  const kept = prev
    .filter(
      (b) =>
        b.role === "system" ||
        Boolean(b.buttons?.length || b.poll) ||
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

export default function ChatPage() {
  const id = useSearchParams().get("id") ?? "";
  return <Chat key={id} id={id} />;
}

function Chat({ id }: { id: string }) {

  const [agent, setAgent] = useState<AgentView | null>(null);
  const [me, setMe] = useState<MeView | null>(null);
  // Whether the agent's workspace can be browsed (issue #177).
  const [workspaceOpen, setWorkspaceOpen] = useState(false);
  useEffect(() => {
    api.workspace
      .access(id)
      .then((a) => setWorkspaceOpen(a.enabled))
      .catch(() => setWorkspaceOpen(false));
  }, [id]);
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
  const [live, setLive] = useState<{ call: string; text: string; thinking: string }[]>([]);
  // Calls whose text is no longer shown: their reply came or was stopped;
  // and those shown now. Kept apart from `live` so a call ends at once.
  const doneCalls = useRef(new Set<string>());
  const liveCalls = useRef(new Set<string>());
  // The frame of the easing that follows a reply being written, while it runs.
  const following = useRef(0);
  // The reader is at the bottom of the chat (within 80 pixels): it follows
  // what comes; scrolled up, it stays, with a button back to the latest.
  const atBottom = useRef(true);
  const lastTop = useRef(0);
  const touchY = useRef<number | null>(null);
  const [away, setAway] = useState(false);
  // The reader moved up: the chat stops following, and offers the way back.
  // Not when the chat cannot move up, nor when a box inside it that still
  // can (the reasoning, a step, a code block) takes the move.
  function leaveBottom(from: EventTarget | null) {
    const el = scrollRef.current;
    if (!el || el.scrollTop <= 0 || el.scrollHeight <= el.clientHeight) return;
    for (let node = from instanceof Element ? from : null; node && node !== el; node = node.parentElement) {
      if (node.scrollTop > 0 && node.scrollHeight > node.clientHeight) return;
    }
    atBottom.current = false;
    cancelAnimationFrame(following.current);
    following.current = 0;
    setAway(true);
  }
  // The reply's own text was on screen as it was written: the reply that
  // replaces it does not fade in again.
  const liveText = useRef(false);
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
  const jumpTo = useRef<{
    chat: string;
    turn: string;
    /** It is on the chat's branch, so the chat stays on it. */
    onBranch: boolean;
    /** The chat is being, or was, moved to its branch. */
    branching: boolean;
    branched: boolean;
  } | null>(null);
  const [found, setFound] = useState<string | null>(null);
  const jumpedAt = useRef(0);
  const [chats, setChats] = useState<ChatSummary[]>([]);
  // The caller's chats in the chat apps they linked (issue #184), and the
  // scope of the one on screen when it is one: read-only here.
  const [channelChats, setChannelChats] = useState<ChannelChat[]>([]);
  const [scope, setScope] = useState<string | null>(null);
  const scopeRef = useRef<string | null>(null);
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

  function selectChat(next: string | null, nextScope: string | null = null) {
    chatIdRef.current = next;
    scopeRef.current = nextScope;
    setChat(next);
    setScope(nextScope);
  }

  const refreshChats = useCallback(async () => {
    try {
      setChats(await api.chats.list(id));
    } catch (e) {
      console.warn("[chat] chat list load failed", e);
    }
    api.channels
      .chats(id)
      .then(setChannelChats)
      .catch(() => setChannelChats([]));
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
    api.channels
      .chats(id)
      .then((list) => {
        if (!cancelled) setChannelChats(list);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [id]);

  const chatRef = useRef<ChatHandle | null>(null);
  const bubbleIdRef = useRef(0);
  const scrollRef = useRef<HTMLDivElement | null>(null);


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
        const r = await api.conversation.list(id, HISTORY_LIMIT, chat ?? undefined, scope ?? undefined);
        if (cancelled) return;
        if (!chatIdRef.current && r.conversation) selectChat(r.conversation);
        // A reply still running when the page was left shows again; the
        // agent's count is the count. A chat-app chat's replies are the
        // app's to show.
        if (r.waiting !== undefined) setWaiting(scope ? 0 : r.waiting);
        if (r.turns.length === 0) return;
        const stored = turnBubbles(r.turns, () => bubbleIdRef.current++);
        setBubbles((prev) => mergeStored(prev, stored, since));
        // The files sent in it, which its stored turns do not hold.
        const shown = r.conversation ?? chat ?? undefined;
        if (shown && !scope) {
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
  }, [id, chat, scope]);

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
        // A channel chat on screen is read from the chat app's side.
        if (scopeRef.current) return;
        const changes = settingsChanged.current;
        const r = await api.conversation.list(id, HISTORY_LIMIT, asked ?? undefined);
        if (cancelled || chatIdRef.current !== asked) return;
        if (!asked && r.conversation) selectChat(r.conversation);
        if (reconnected && r.waiting !== undefined) setWaiting(r.waiting);
        if (settingsChanged.current === changes) {
          setChatModel(r.model ?? null);
          setChatThinking(r.thinking ?? null);
        }
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
          // The socket carries the web chats; a chat-app chat on screen
          // shows none of it, and the web chats' list keeps up.
          if (scopeRef.current) {
            if (msg.type === "agent_message" && msg.conversation) {
              const due = waitingElsewhere.current.get(msg.conversation) ?? 0;
              if (!msg.broadcast && due > 0) waitingElsewhere.current.set(msg.conversation, due - 1);
              void refreshChats();
            }
            if (msg.type !== "approval_closed") return;
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
            // The call's reasoning or its reply, each pieced together alike.
            const key = msg.thinking ? "thinking" : "text";
            if (!msg.thinking) {
              setStep(null);
              liveText.current = true;
            }
            setLive((prev) => {
              const i = prev.findIndex((p) => p.call === msg.call);
              const entry = i < 0 ? { call: msg.call, text: "", thinking: "" } : prev[i];
              let value: string;
              if (msg.at === 0) {
                value = msg.text;
              } else if (i >= 0 && entry[key].length === msg.at) {
                value = entry[key] + msg.text;
              } else {
                // A piece that does not follow what is shown waits for the
                // next whole text.
                return prev;
              }
              const next = i < 0 ? [...prev, entry] : [...prev];
              next[i < 0 ? next.length - 1 : i] = { ...entry, [key]: value };
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
            // A choice the gateway answered (a question answered already,
            // or expired) gets no reply.
            if (choicesWaiting.current > 0) {
              choicesWaiting.current -= 1;
              setWaiting((n) => Math.max(0, n - 1));
            }
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
            // A reply in another chat shows there, as does a message nobody
            // asked for while no chat is open.
            if (msg.conversation !== current && (current || msg.broadcast)) {
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
          // A question the agent asked comes in the middle of its turn,
          // which goes on.
          const asking = msg.type === "agent_message" && Boolean(msg.buttons?.length || msg.poll);
          const written = liveText.current;
          if (msg.type === "agent_message" && !asking) {
            if (!msg.broadcast) {
              choicesWaiting.current = Math.max(0, choicesWaiting.current - 1);
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
            pushBubble("agent", msg.content, { buttons: msg.buttons, poll: msg.poll, fresh: !written });
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
    const jump = jumpTo.current;
    if ((jump && jump.chat === chatIdRef.current) || Date.now() - jumpedAt.current < JUMP_HOLD_MS) {
      return;
    }
    if (!atBottom.current) return;
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
    if (!jump || jump.branching || chatIdRef.current !== jump.chat || bubbles.length === 0) return;
    const el = scrollRef.current?.querySelector(`[data-turn="${CSS.escape(jump.turn)}"]`);
    if (el) {
      jumpTo.current = null;
      jumpedAt.current = Date.now();
      const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
      el.scrollIntoView({ block: "center", behavior: reduce ? "auto" : "smooth" });
      atBottom.current = false;
      setAway(true);
      setFound(jump.turn);
      setTimeout(() => setFound((f) => (f === jump.turn ? null : f)), 4000);
      return;
    }
    if (jump.branched || jump.onBranch) {
      jumpTo.current = null;
      pushBubble("system", "That message is further back than this chat shows.");
      return;
    }
    if (waiting > 0) {
      jumpTo.current = null;
      pushBubble("system", "That message is on another version of this chat; wait for the reply, then open it again.");
      return;
    }
    // Decided once the chat is on the message's branch, whatever arrives
    // meanwhile.
    jump.branching = true;
    void showVersion(jump.turn).then((moved) => {
      if (jumpTo.current !== jump) return;
      if (!moved) {
        jumpTo.current = null;
        return;
      }
      jump.branching = false;
      jump.branched = true;
      setBubbles((prev) => [...prev]);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bubbles]);

  // As the reply being written grows, follow it, unless the user scrolled
  // up to read.
  const liveLength = live.reduce((n, p) => n + p.text.length + p.thinking.length, 0);
  useEffect(() => {
    const el = scrollRef.current;
    if (!el || liveLength === 0 || !atBottom.current || following.current) return;
    if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
      el.scrollTop = el.scrollHeight;
      return;
    }
    // It keeps easing while the reply's height still moves, and stops once
    // the bottom has held for a quarter of a second.
    let held = 0;
    const ease = () => {
      const gap = el.scrollHeight - el.clientHeight - el.scrollTop;
      if (!atBottom.current) {
        following.current = 0;
        return;
      }
      if (gap <= 1) {
        held += 1;
        if (held > 15) {
          following.current = 0;
          return;
        }
      } else {
        held = 0;
        el.scrollTop += Math.max(1, gap * 0.2);
      }
      following.current = requestAnimationFrame(ease);
    };
    following.current = requestAnimationFrame(ease);
  }, [liveLength]);
  useEffect(() => () => cancelAnimationFrame(following.current), []);

  // A file card, once per file.
  function addFile(url: string, name: string, caption: string | undefined, ts: number) {
    setBubbles((prev) => withFiles(prev, [{ url, name, caption, ts }], () => bubbleIdRef.current++));
  }

  // Hide the text being written; a call still running shows it again
  // with its next whole text.
  function clearLive() {
    liveCalls.current.clear();
    liveText.current = false;
    setLive([]);
  }

  // Stop showing the text being written: the reply replaces it.
  function endLive() {
    for (const call of liveCalls.current) doneCalls.current.add(call);
    liveCalls.current.clear();
    liveText.current = false;
    // The oldest are forgotten; their text stopped coming long ago.
    for (const call of doneCalls.current) {
      if (doneCalls.current.size <= 500) break;
      doneCalls.current.delete(call);
    }
    setLive([]);
  }

  function pushBubble(role: Bubble["role"], content: string, extra: Partial<Bubble> = {}) {
    setBubbles((prev) => [
      ...prev,
      { fresh: true, ...extra, id: bubbleIdRef.current++, role, content, ts: Date.now() },
    ]);
  }

  // Answer a question the agent asked: what the button or the poll sends,
  // shown as what was chosen.
  function choose(bubbleId: number, data: string, shown: string) {
    if (!chatRef.current || wsState !== "open") return;
    setBubbles((prev) => prev.map((b) => (b.id === bubbleId ? { ...b, answered: true } : b)));
    pushBubble("you", shown);
    chatRef.current.send(data, chatIdRef.current ?? undefined);
    if (waiting === 0) clearLive();
    choicesWaiting.current += 1;
    setWaiting((n) => n + 1);
  }
  // Choices sent and not answered yet: the gateway's notice (a question
  // answered already, or expired) answers one in place of the agent.
  const choicesWaiting = useRef(0);

  function sendText(text: string, files: Attachment[] = []) {
    if ((!text.trim() && files.length === 0) || !chatRef.current || wsState !== "open") return;
    atBottom.current = true;
    setAway(false);
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

  // The models the agent may use, and the chat's own model and thinking
  // choice (null: the agent's model, the model's default).
  const [models, setModels] = useState<AgentModels | null>(null);
  const [chatModel, setChatModel] = useState<string | null>(null);
  const [chatThinking, setChatThinking] = useState<string | null>(null);
  // Counts the chat settings changed here, so a history load that started
  // before one does not put back what the chat had.
  const settingsChanged = useRef(0);
  useEffect(() => {
    api.models
      .forAgent(id)
      .then(setModels)
      .catch((e) => console.warn("[chat] models unavailable", e));
  }, [id]);

  // Set the chat's model or thinking choice; a chat with no message yet
  // gets its id here.
  async function setChatSettings(settings: { model?: string | null; thinking?: string | null }) {
    settingsChanged.current += 1;
    let chatId = chatIdRef.current;
    if (!chatId) {
      chatId = newChatId();
      selectChat(chatId);
    }
    try {
      await api.chats.settings(id, chatId, settings);
      settingsChanged.current += 1;
      if (settings.model !== undefined) setChatModel(settings.model);
      if (settings.thinking !== undefined) setChatThinking(settings.thinking);
    } catch (e) {
      pushBubble("system", `error: ${e instanceof Error ? e.message : String(e)}`);
    }
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
  // Whether the chat moved to the branch of `turn`.
  async function showVersion(turn: string): Promise<boolean> {
    const current = chatIdRef.current;
    if (!current) return false;
    try {
      const onScope = scopeRef.current ?? undefined;
      await api.chats.branch(id, current, turn, onScope);
      const r = await api.conversation.list(id, HISTORY_LIMIT, current, onScope);
      if (chatIdRef.current !== current) return false;
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
      return true;
    } catch (e) {
      pushBubble("system", `error: ${e instanceof Error ? e.message : String(e)}`);
      return false;
    }
  }

  // Show chat `target`, and in it the found message `turn` (see the
  // effect above); a found title only opens its chat.
  function openFound(target: string, turn: string | null, onBranch = true) {
    jumpTo.current = turn
      ? { chat: target, turn, onBranch, branching: false, branched: false }
      : null;
    if (target === chatIdRef.current) {
      // Already on screen: the effect runs on what is shown.
      setBubbles((prev) => [...prev]);
    } else {
      openChatById(target);
    }
  }

  // Show chat `next`, or a new one. Approvals stay: they are the agent's,
  // whichever chat asked.
  function openChatById(next: string | null, nextScope: string | null = null) {
    setShowChats(false);
    // A found message of another chat is no longer to be shown.
    if (jumpTo.current && jumpTo.current.chat !== next) jumpTo.current = null;
    if (next !== null && next === chatIdRef.current && nextScope === scopeRef.current) return;
    // Another chat opens at its end.
    atBottom.current = true;
    setAway(false);
    const previous = chatIdRef.current;
    const target = next ?? newChatId();
    selectChat(target, nextScope);
    setBubbles([]);
    // Until its history says otherwise, a chat has the defaults.
    setChatModel(null);
    setChatThinking(null);
    setStep(null);
    clearLive();
    setWaiting((n) => {
      if (previous) waitingElsewhere.current.set(previous, n);
      return waitingElsewhere.current.get(target) ?? 0;
    });
  }

  // Continue the channel chat on screen on the web (issue #184): it
  // becomes one of the caller's web chats; there is no way back.
  async function continueOnWeb() {
    const current = chatIdRef.current;
    const from = scopeRef.current;
    if (!current || !from) return;
    try {
      await api.channels.move(id, current, from);
      selectChat(current, null);
      await refreshChats();
    } catch (e) {
      pushBubble("system", `error: ${e instanceof Error ? e.message : String(e)}`);
    }
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

  function decide(id: string, approve: boolean, remember: boolean, note?: string) {
    // The gateway closes the card in every tab once it took the answer.
    if (!chatRef.current || wsState !== "open") return;
    chatRef.current.decide(id, approve, remember, note);
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
        current={scope ? null : chat}
        channelChats={channelChats}
        currentChannel={scope ? { chat, scope } : null}
        onOpenChannel={(c) => openChatById(c.id, c.scope)}
        onOpen={(c) => openChatById(c)}
        onRename={renameChat}
        onDelete={deleteChat}
        onFound={(chat, turn, onBranch) => {
          setShowChats(false);
          openFound(chat, turn, onBranch);
        }}
        agentId={id}
        className={showChats ? "flex w-full" : "hidden"}
      />
      <div className={`${showChats ? "hidden" : "flex"} min-w-0 flex-1 flex-col md:flex`}>
        {/* Header */}
        <div
          className="flex flex-wrap items-center justify-between gap-x-3 gap-y-1 border-b px-4 py-2.5 sm:px-8"
          style={{ borderColor: "var(--color-border-default)" }}
        >
          <div className="min-w-0">
            <p className="flex items-center gap-2 text-xs" style={{ color: "var(--color-body)" }}>
              <span className="truncate font-medium" title={`agent ${agent.id}`}>
                {agent.name}
              </span>
              <ConnectionPill state={wsState} agent={agent} />
            </p>
            <h1 className="truncate text-base font-semibold" style={{ color: "var(--color-heading)" }}>
              {(() => {
                const shown = scope
                  ? channelChats.find((c) => c.id === chat && c.scope === scope)
                  : chats.find((c) => c.id === chat);
                return shown ? shown.title || "untitled" : "new chat";
              })()}
            </h1>
          </div>
          <div className="flex flex-wrap items-center gap-0.5">
            <button
              type="button"
              className="tool-button text-sm md:hidden"
              onClick={() => setShowChats(true)}
            >
              <MessagesSquare size={16} className="shrink-0" aria-hidden="true" />
              chats
            </button>
            <button
              type="button"
              className="tool-button text-sm md:hidden"
              onClick={() => openChatById(null)}
              title="Start a new chat; this one stays in the list"
            >
              <MessageSquarePlus size={16} className="shrink-0" aria-hidden="true" />
              new chat
            </button>
            <Link
              href={`/agent/memory?id=${agent.id}`}
              className="tool-button text-sm"
              title="See what the agent remembers about you"
            >
              <Brain size={16} className="shrink-0" aria-hidden="true" />
              memory
            </Link>
            <Link
              href={`/agent/skills?id=${agent.id}`}
              className="tool-button text-sm"
              title="Installed skills + curator log"
            >
              <BookOpen size={16} className="shrink-0" aria-hidden="true" />
              skills
            </Link>
            {workspaceOpen && (
              <Link
                href={`/agent/workspace?id=${agent.id}`}
                className="tool-button text-sm"
                title="Browse the files in the agent's workspace"
              >
                <FolderOpen size={16} className="shrink-0" aria-hidden="true" />
                workspace
              </Link>
            )}
            <Link
              href={`/agent/settings?id=${agent.id}`}
              className="tool-button text-sm"
              title="Rename, change model, edit policy"
            >
              <Settings size={16} className="shrink-0" aria-hidden="true" />
              settings
            </Link>
            <ExportMenu
              agentId={agent.id}
              agentName={agent.name}
              chat={chat}
              scope={scope}
              title={
                (scope
                  ? channelChats.find((c) => c.id === chat && c.scope === scope)?.title
                  : chats.find((c) => c.id === chat)?.title) || "chat"
              }
            />
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
            const up = el.scrollTop < lastTop.current - 2;
            lastTop.current = el.scrollTop;
            if (el.scrollHeight - el.scrollTop - el.clientHeight < 8) {
              if (!up) atBottom.current = true;
            } else if (up) {
              atBottom.current = false;
            }
            if (away === atBottom.current) setAway(!atBottom.current);
          }}
          onWheel={(e) => {
            if (e.deltaY < 0) leaveBottom(e.target);
          }}
          onTouchStart={(e) => {
            touchY.current = e.touches[0]?.clientY ?? null;
          }}
          onTouchMove={(e) => {
            const y = e.touches[0]?.clientY;
            // A finger moving down scrolls the chat up.
            if (touchY.current !== null && y !== undefined && y > touchY.current + 4) leaveBottom(e.target);
          }}
          onKeyDown={(e) => {
            const typing = (e.target as HTMLElement).closest("input, textarea, select, [contenteditable]");
            if (!typing && ["PageUp", "ArrowUp", "Home"].includes(e.key)) leaveBottom(e.target);
          }}
          className="relative flex-1 overflow-y-auto px-4 py-6 sm:px-8"
          style={{ backgroundColor: "var(--color-surface)" }}
        >
          <div className="mx-auto flex max-w-3xl flex-col gap-4">
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
                chatId={chat}
                scope={scope}
                busy={pending || wsState !== "open"}
                onUpload={uploadFiles}
                onEdit={
                  // A chat-app chat is read-only here.
                  b.role === "you" && b.turn && !scope
                    ? (text, files) => editMessage(idx, b.turn!, text, files)
                    : undefined
                }
                onRegenerate={
                  b.role === "agent" && b.turn && !scope ? () => regenerate(idx, b.turn!) : undefined
                }
                onVersion={(turn) => void showVersion(turn)}
                onChoose={
                  // A chat-app chat is answered in its app.
                  !scope && (b.buttons?.length || b.poll)
                    ? (data, shown) => choose(b.id, data, shown)
                    : undefined
                }
                onRetry={
                  b.role === "agent" && !b.turn && !scope
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
            {approvals.map((a) =>
              a.kind === "call" || a.kind === "result" ? (
                <OwnerApproval
                  key={a.id}
                  request={a}
                  disabled={wsState !== "open"}
                  onDecide={(approve, remember, note) => decide(a.id, approve, remember, note)}
                />
              ) : (
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
              ),
            )}
            {pending && live.some((p) => p.thinking.trim() !== "") && (
              <LiveThinking
                text={live.map((p) => p.thinking.trim()).filter(Boolean).join("\n\n")}
                active={!live.some((p) => p.text.trim() !== "")}
              />
            )}
            {pending && live.some((p) => p.text.trim() !== "") && (
              <LiveBubble text={live.map((p) => p.text.trim()).filter(Boolean).join("\n\n")} />
            )}
            {pending && <TypingBubble step={step} />}
          </div>
          {away && (
            <div className="pointer-events-none sticky bottom-0 flex justify-center pt-2">
              <button
                type="button"
                className="tool-button enter pointer-events-auto text-sm"
                style={{
                  backgroundColor: "var(--color-surface)",
                  border: "1px solid var(--color-border-default)",
                  boxShadow: "var(--shadow-ambient)",
                  borderRadius: 999,
                  padding: "6px 14px",
                }}
                onClick={() => {
                  const el = scrollRef.current;
                  if (!el) return;
                  const reduce = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
                  atBottom.current = true;
                  setAway(false);
                  el.scrollTo({ top: el.scrollHeight, behavior: reduce ? "auto" : "smooth" });
                }}
              >
                <ArrowDown size={14} className="shrink-0" aria-hidden="true" />
                latest
              </button>
            </div>
          )}
        </div>

        {/* Composer; a channel chat continues in its app, or moves here */}
        {scope ? (
          <ChannelChatBar
            channel={channelChats.find((c) => c.id === chat && c.scope === scope)?.channel ?? "the chat app"}
            onMove={continueOnWeb}
          />
        ) : (
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
          models={models}
          chatModel={chatModel}
          chatThinking={chatThinking}
          onSettings={(s) => void setChatSettings(s)}
        />
        )}
      </div>
    </div>
  );
}

/** The name a chat app goes by. */
function appName(channel: string): string {
  return channel.charAt(0).toUpperCase() + channel.slice(1);
}

/**
 * In place of the composer for a channel chat: it lives in its app and
 * continues there, or moves to the web for good.
 */
function ChannelChatBar({ channel, onMove }: { channel: string; onMove: () => Promise<void> }) {
  const [moving, setMoving] = useState(false);
  return (
    <div
      className="flex flex-wrap items-center justify-between gap-3 border-t px-4 py-4 text-sm sm:px-8"
      style={{ borderColor: "var(--color-border-default)", color: "var(--color-body)" }}
      role="region"
      aria-label="channel chat"
    >
      <p>
        This chat lives in {appName(channel)} and continues there. Continue it here instead: it
        becomes a web chat, and your next message in {appName(channel)} starts a new chat. There
        is no way back.
      </p>
      <button
        type="button"
        className="btn-ghost"
        disabled={moving}
        onClick={() => {
          setMoving(true);
          void onMove().finally(() => setMoving(false));
        }}
      >
        continue on the web
      </button>
    </div>
  );
}

function ChatList({
  agentId,
  chats,
  current,
  channelChats,
  currentChannel,
  onOpenChannel,
  onOpen,
  onRename,
  onDelete,
  onFound,
  className,
}: {
  agentId: string;
  chats: ChatSummary[];
  current: string | null;
  /** The caller's chats in the chat apps they linked, read-only here. */
  channelChats: ChannelChat[];
  currentChannel: { chat: string | null; scope: string } | null;
  onOpenChannel: (chat: ChannelChat) => void;
  onOpen: (chat: string | null) => void;
  onRename: (chat: ChatSummary) => void;
  onDelete: (chat: ChatSummary) => void;
  /** A search result: its chat, and the message to show. */
  onFound: (chat: string, turn: string | null, onBranch?: boolean) => void;
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
          className="btn-ghost inline-flex w-full items-center justify-center gap-1.5"
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
        {channelChats.length > 0 && (
          <>
            <p className="mt-4 px-2 pb-1 text-xs" style={{ color: "var(--color-body)" }}>
              in chat apps
            </p>
            <ul className="flex flex-col gap-0.5" aria-label="chats in chat apps">
              {channelChats.map((c) => {
                const on = currentChannel?.chat === c.id && currentChannel.scope === c.scope;
                const title = c.title || "untitled";
                return (
                  <li key={`${c.scope}/${c.id}`} className="flex items-center gap-1.5 rounded" style={itemStyle(on)}>
                    <button
                      type="button"
                      className="min-w-0 flex-1 truncate px-2 py-1.5 text-left text-sm"
                      onClick={() => onOpenChannel(c)}
                      aria-current={on ? "true" : undefined}
                      title={`${title} (${appName(c.channel)}, read-only here)`}
                    >
                      {title}
                    </button>
                    <span className="pill pill-stopped mr-1.5 shrink-0 text-xs">{appName(c.channel)}</span>
                  </li>
                );
              })}
            </ul>
          </>
        )}
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
  onFound: (chat: string, turn: string | null, onBranch?: boolean) => void;
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
                  onClick={() => onFound(t.chat, t.id, t.on_branch)}
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

/** The buttons of a question the agent asked, or the options of its
 * poll to tick; answered once (issue #204). */
function Choices({
  bubble,
  onChoose,
}: {
  bubble: Bubble;
  onChoose: (data: string, shown: string) => void;
}) {
  const [ticked, setTicked] = useState<number[]>([]);
  const done = bubble.answered === true;
  if (bubble.poll) {
    const poll = bubble.poll;
    return (
      <div className="flex flex-col gap-1.5" style={{ maxWidth: "min(85%, 720px)" }}>
        {poll.options.map((option, n) => (
          <label key={n} className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              disabled={done}
              checked={ticked.includes(n + 1)}
              onChange={(e) =>
                setTicked((prev) =>
                  e.target.checked ? [...prev, n + 1] : prev.filter((x) => x !== n + 1),
                )
              }
            />
            {option}
          </label>
        ))}
        <div>
          <button
            type="button"
            className="btn-secondary"
            disabled={done}
            onClick={() => {
              const numbers = [...ticked].sort((a, b) => a - b);
              const shown = numbers.length
                ? numbers.map((n) => poll.options[n - 1]).join(", ")
                : "none of these";
              onChoose(`${poll.answer} ${numbers.join(",")}`.trim(), `Chose: ${shown}`);
            }}
          >
            send
          </button>
        </div>
      </div>
    );
  }
  return (
    <div className="flex flex-wrap gap-1.5" style={{ maxWidth: "min(85%, 720px)" }}>
      {(bubble.buttons ?? []).flat().map((button) => (
        <button
          key={button.data}
          type="button"
          className="btn-secondary"
          disabled={done}
          onClick={() => onChoose(button.data, `Chose: ${button.label}`)}
        >
          {button.label}
        </button>
      ))}
    </div>
  );
}

function Bubble({
  bubble,
  busy,
  found,
  agentId,
  chatId,
  scope,
  onRetry,
  onEdit,
  onUpload,
  onRegenerate,
  onVersion,
  onChoose,
}: {
  bubble: Bubble;
  agentId: string;
  /** The chat it is in, once it has one. */
  chatId?: string | null;
  /** A chat-app chat's scope, when the bubble is in one. */
  scope?: string | null;
  /** A reply is due: branching waits for it. */
  busy: boolean;
  /** The message a search led to: marked for a moment. */
  found?: boolean;
  onRetry?: () => void;
  onEdit?: (text: string, files: Attachment[]) => void;
  onUpload: (picked: File[], room: number, done: (a: Attachment) => void) => Promise<void>;
  onRegenerate?: () => void;
  onVersion: (turn: string) => void;
  /** Answer the question it asks: what to send, and what to show sent. */
  onChoose?: (data: string, shown: string) => void;
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
      className={`flex flex-col gap-1.5 ${align}${bubble.fresh ? " enter" : ""}`}
      data-turn={bubble.turn}
      style={
        found
          ? { outline: "2px solid var(--color-stripe-purple)", outlineOffset: 4, borderRadius: 8 }
          : undefined
      }
    >
      {bubble.role === "agent" && bubble.trace && bubble.turn && chatId && (bubble.trace.thought || bubble.trace.steps > 0) && (
        <TraceFold agentId={agentId} chat={chatId} turn={bubble.turn} summary={bubble.trace} scope={scope} />
      )}
      {message && message.files.length > 0 && (
        <AttachmentCards agentId={agentId} files={message.files} align="end" />
      )}
      {(!message || message.text) && (
      <div
        className={bubble.role === "agent" ? "w-full" : "px-4 py-2.5"}
        style={{
          maxWidth: bubble.role === "agent" ? "100%" : "min(85%, 720px)",
          borderRadius: "var(--radius-large)",
          ...(bubble.role === "you"
            ? {
                backgroundColor: "rgba(83, 58, 253, 0.08)",
                color: "var(--color-deep-navy)",
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
                  color: "var(--color-deep-navy)",
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
      {onChoose && <Choices bubble={bubble} onChoose={onChoose} />}
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
              <Pencil size={14} className="shrink-0" aria-hidden="true" />
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
              <RefreshCw size={14} className="shrink-0" aria-hidden="true" />
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
  // download; the thumbnail is the copy it made of a HEIC or large photo.
  const url = `/api/agents/${agentId}/files?path=${encodeURIComponent(file.path)}`;
  const thumbnail = `${url}&shown=true`;
  const [unshown, setUnshown] = useState(false);
  const image = !unshown && /^image\/(png|jpeg|gif|webp|heic|heif)$/.test(file.media_type);
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
          <img
            src={thumbnail}
            alt={file.name}
            loading="lazy"
            className="h-full w-full object-cover"
            onError={() => setUnshown(true)}
          />
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
  bare,
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
  /** Inside a panel that draws the frame: no border of its own. */
  bare?: boolean;
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
      className={bare ? "block w-full bg-transparent outline-none placeholder:text-[var(--color-body)]" : "input w-full"}
      style={{
        resize: "none",
        overflowY: "auto",
        minHeight: 56,
        maxHeight: 320,
        fontFamily: "inherit",
        padding: bare ? "12px 12px 4px" : "10px 12px",
        lineHeight: 1.45,
        ...(bare ? { fontSize: 15, color: "var(--color-deep-navy)" } : {}),
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
  const [visibility, setVisibility] = useState(change.visibility);
  const [switching, setSwitching] = useState(false);
  const saved = change.action === "saved";

  // Only the entry this reply saved: nothing changes once the key holds
  // another (issue #309).
  async function switchVisibility() {
    const next = visibility === "public" ? "private" : "public";
    const ask =
      next === "public"
        ? `Make "${change.key}" public? It is then in the agent's prompt in every chat, other people's and groups' too, as it says:\n\n${change.value}`
        : `Make "${change.key}" private? From now on it is in your own private chats only. What others already read cannot be taken back.`;
    if (!confirm(ask)) return;
    setSwitching(true);
    setError(null);
    try {
      const entry = await api.memory.setVisibility(agentId, change.key, next, change.after);
      setVisibility(entry.visibility);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSwitching(false);
    }
  }

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
          {change.value_cut && "…"}
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
      {saved && change.visibility && change.after && state !== "undone" && (
        <>
          {/* What it was when saved; the memory page shows what it is now.
              The agent only suggests public: the owner makes it so. */}
          <span>
            {change.visibility === "private" && change.suggested_public
              ? "(saved as private, suggested public)"
              : `(saved as ${change.visibility})`}
          </span>
          {visibility !== "public" && change.value_cut ? (
            // Only what the owner read in full is made public: the memory
            // page shows all of it.
            <Link
              href={`/agent/memory?id=${agentId}`}
              className="underline"
              title="It says more than is shown here: read it all on the memory page and make it public there."
            >
              make public on the memory page
            </Link>
          ) : (
            <button
              type="button"
              className="underline disabled:opacity-40"
              onClick={() => void switchVisibility()}
              disabled={switching}
              title={
                visibility === "public"
                  ? "Make it private: in your own private chats only."
                  : "Make it public: in the prompt of every chat, other people's and groups' too."
              }
            >
              {switching ? "switching…" : visibility === "public" ? "make private" : "make public"}
            </button>
          )}
          {visibility !== change.visibility && <span>(now {visibility})</span>}
        </>
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
      className="inline-flex items-center gap-1 rounded px-2 py-1 transition-colors hover:bg-[rgba(100,116,141,0.08)] disabled:opacity-40"
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
      className={`mt-0.5 flex gap-1 ${align === "end" ? "justify-end" : "justify-start"}`}
      style={{ fontSize: 13, color: "var(--color-body)" }}
    >
      <button
        type="button"
        onClick={onCopy}
        title="copy markdown source"
        aria-label={copied ? "copied" : "copy message"}
        className="rounded px-2 py-1 transition-colors hover:bg-[rgba(100,116,141,0.08)]"
        style={{ background: "transparent", color: copied ? "var(--color-success-text)" : undefined }}
      >
        <span className="inline-flex items-center gap-1">
          {copied ? (
            <Check size={14} className="enter shrink-0" aria-hidden="true" />
          ) : (
            <Copy size={14} className="shrink-0" aria-hidden="true" />
          )}
          {copied ? "copied" : "copy"}
        </span>
      </button>
      {onRetry && (
        <button
          type="button"
          onClick={onRetry}
          title="re-send the previous user message"
          aria-label="retry"
          className="rounded px-2 py-1 transition-colors hover:bg-[rgba(100,116,141,0.08)]"
          style={{ background: "transparent" }}
        >
          <span className="inline-flex items-center gap-1">
            <RotateCcw size={14} className="shrink-0" aria-hidden="true" />
            retry
          </span>
        </button>
      )}
      {children}
    </div>
  );
}

/**
 * The chat's model and how much it thinks, for this chat only: the model
 * menu as on the agent's settings page; the thinking menu with off first,
 * then what the model takes, greyed out for a model that cannot think.
 */
function ChatModelMenus({
  models,
  chatModel,
  chatThinking,
  disabled,
  onSettings,
}: {
  models: AgentModels;
  chatModel: string | null;
  chatThinking: string | null;
  disabled: boolean;
  onSettings: (settings: { model?: string | null; thinking?: string | null }) => void;
}) {
  const agentModel = models.agent_model ?? models.default_model;
  const model = chatModel ?? agentModel;
  const spec = models.models.find((m) => m.id === model)?.thinking;
  const choices = spec ? thinkingChoices(spec) : [];
  const fallback = spec?.default ?? choices[0] ?? "off";
  const thinking = chatThinking && choices.includes(chatThinking) ? chatThinking : fallback;
  const short = (m: { id: string; label?: string }) => {
    const name = modelName(m.id, m.label);
    return models.models.filter((x) => modelName(x.id, x.label) === name).length > 1 ? m.id : name;
  };
  const name = (id: string | null) => {
    const m = models.models.find((x) => x.id === id);
    return id ? short(m ?? { id }) : "default";
  };
  const thinkingName = (c: string) =>
    c === "off" ? "thinking off" : c === "on" ? "thinking on" : spec?.style === "budget" ? `think ${Number(c).toLocaleString()} tokens` : `think ${c}`;
  return (
    <>
      <select
        className="composer-select min-w-0"
        style={{ maxWidth: 280 }}
        aria-label="model of this chat"
        title={`The model of this chat; others keep theirs. Now: ${model ?? "the default"}`}
        value={chatModel ?? ""}
        disabled={disabled}
        onChange={(e) => onSettings({ model: e.target.value || null })}
      >
        <option value="">{name(agentModel)} (agent&apos;s)</option>
        {models.models.map((m) => (
          <option key={m.id} value={m.id} title={m.id}>
            {short(m)}
            {m.id === agentModel ? " (kept for this chat)" : ""}
          </option>
        ))}
      </select>
      <select
        className="composer-select min-w-0"
        style={{ maxWidth: 180 }}
        aria-label="thinking of this chat"
        title={spec ? "How much this chat's model thinks" : "This model is not set up to think"}
        value={spec ? thinking : "off"}
        disabled={disabled || !spec}
        onChange={(e) => onSettings({ thinking: e.target.value })}
      >
        {spec ? (
          choices.map((c) => (
            <option key={c} value={c}>
              {thinkingName(c)}
              {c === fallback && !chatThinking ? " (default)" : ""}
            </option>
          ))
        ) : (
          <option value="off">no thinking</option>
        )}
      </select>
    </>
  );
}

/** The reasoning of the reply being written, as it arrives. */
// Its head says what it is on now, its latest line, and while the model
// still thinks, a highlight runs over it; a click folds the rest away.
function LiveThinking({ text, active }: { text: string; active: boolean }) {
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = box.current;
    if (el) el.scrollTop = el.scrollHeight;
  }, [text]);
  const lines = text.split("\n").filter((line) => line.trim() !== "");
  const now = oneLine(lines[lines.length - 1] ?? "");
  return (
    <div className="enter w-full text-[13px]" style={{ color: "var(--color-body)" }} aria-live="off">
      <Fold
        opened
        head={
          <>
            <Brain size={14} className="shrink-0" aria-hidden="true" style={{ color: "var(--color-stripe-purple-mid)" }} />
            <span className={`min-w-0 truncate ${active ? "shimmer" : ""}`}>
              <span className="font-medium">Thinking</span>
              {now && <> · {now}</>}
            </span>
          </>
        }
      >
        <div
          ref={box}
          className="mb-1 ml-5 mt-1 overflow-y-auto border-l-2 pl-3"
          style={{ maxHeight: 160, whiteSpace: "pre-wrap", borderColor: "var(--color-border-default)" }}
        >
          {text}
        </div>
      </Fold>
    </div>
  );
}

/**
 * A part that opens on a click: its head always shown, with an arrow that
 * turns, and what it holds, which opens and closes with a short motion.
 * What it holds is made the first time it opens, after `onOpen`.
 */
function Fold({
  head,
  onOpen,
  opened = false,
  className,
  style,
  children,
}: {
  head: React.ReactNode;
  onOpen?: () => void;
  /** Open when it is first shown. */
  opened?: boolean;
  className?: string;
  style?: React.CSSProperties;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(opened);
  const [made, setMade] = useState(opened);
  const id = useId();
  return (
    <div className={className} style={style}>
      <button
        type="button"
        aria-expanded={open}
        aria-controls={id}
        onClick={() => {
          if (!open) {
            onOpen?.();
            setMade(true);
          }
          setOpen(!open);
        }}
        className="flex w-full min-w-0 items-center gap-1.5 rounded py-0.5 text-left transition-colors hover:text-[var(--color-deep-navy)]"
      >
        <ChevronRight size={14} className="fold-arrow shrink-0" data-open={open} aria-hidden="true" />
        {head}
      </button>
      <div id={id} className="fold" data-open={open} inert={!open}>
        <div>{made && children}</div>
      </div>
    </div>
  );
}

/** Text on one line, its runs of white space as one space. */
function oneLine(text: string): string {
  return text.slice(0, 400).replace(/\s+/g, " ").trim();
}

/** A tool call's input in a few words: the query, address, command or path it was given. */
function briefInput(input: string): string {
  let value: unknown;
  try {
    value = JSON.parse(input);
  } catch {
    return oneLine(input);
  }
  if (typeof value === "string") return oneLine(value);
  if (!value || typeof value !== "object" || Array.isArray(value)) return "";
  const fields = value as Record<string, unknown>;
  for (const key of ["query", "url", "command", "path", "pattern", "question", "task", "text", "name"]) {
    const field = fields[key];
    if (typeof field === "string" && field.trim()) return oneLine(field);
  }
  const first = Object.values(fields).find((v) => typeof v === "string" && v.trim());
  return typeof first === "string" ? oneLine(first) : "";
}

/** How long, in words a person reads: "12 s", "3 min". */
function took(ms: number): string {
  const s = Math.max(1, Math.round(ms / 1000));
  return s < 60 ? `${s} s` : `${Math.round(s / 60)} min`;
}

/**
 * What a reply's turn did on the way, folded into one line: opened, the
 * model's reasoning and each tool call, one line each, each opened in turn
 * for the whole of it.
 */
function TraceFold({
  agentId,
  chat,
  turn,
  summary,
  scope,
}: {
  agentId: string;
  chat: string;
  turn: string;
  summary: TraceSummary;
  /** A chat-app chat's scope. */
  scope?: string | null;
}) {
  const [trace, setTrace] = useState<TurnTrace | null>(null);
  const [error, setError] = useState<string | null>(null);
  const parts = [
    summary.thought ? "Thought" : null,
    summary.steps > 0 ? `${summary.steps} tool call${summary.steps === 1 ? "" : "s"}` : null,
  ].filter(Boolean);
  return (
    <Fold
      className="w-full text-[13px]"
      style={{ color: "var(--color-body)" }}
      head={
        <span>
          {parts.join(" · ")} · {took(summary.ms)}
        </span>
      }
      onOpen={() => {
        if (trace) return;
        setError(null);
        api.chats.trace(agentId, chat, turn, scope ?? undefined).then(setTrace, (err: unknown) =>
          setError(err instanceof Error ? err.message : String(err)),
        );
      }}
    >
      {!trace ? (
        <p className="mt-1 pl-5" role={error ? "alert" : undefined}>
          {error ?? "loading…"}
        </p>
      ) : (
        <TraceList items={trace.items} leftOut={trace.left_out} />
      )}
    </Fold>
  );
}

/** Steps shown at once; a longer turn shows the rest when asked. */
const STEPS_SHOWN = 8;

/** What a turn did, in order, a line each; a subagent's own steps under its call. */
function TraceList({ items, leftOut }: { items: TraceItem[]; leftOut: number }) {
  const [all, setAll] = useState(items.length <= STEPS_SHOWN + 2);
  const shown = all ? items : items.slice(0, STEPS_SHOWN);
  return (
    <ol
      className="ml-[7px] mt-0.5 flex flex-col gap-0.5 border-l pl-3"
      style={{ borderColor: "var(--color-border-default)" }}
    >
      {shown.map((item, i) => (
        <li key={i}>
          {item.kind === "thinking" ? <ThinkingStep text={item.text} /> : <ToolStep step={item} />}
        </li>
      ))}
      {!all && (
        <li>
          <button
            type="button"
            className="py-0.5 underline decoration-dotted underline-offset-2 hover:text-[var(--color-deep-navy)]"
            onClick={() => setAll(true)}
          >
            show {items.length - STEPS_SHOWN} more
          </button>
        </li>
      )}
      {all && leftOut > 0 && <li className="py-0.5">…and {leftOut} more, not kept.</li>}
    </ol>
  );
}

/** The model's reasoning: its first words, and opened, all of it. */
function ThinkingStep({ text }: { text: string }) {
  return (
    <Fold
      head={
        <>
          <Brain size={14} className="shrink-0" aria-hidden="true" style={{ color: "var(--color-stripe-purple-mid)" }} />
          <span className="shrink-0 font-medium" style={{ color: "var(--color-deep-navy)" }}>
            Thinking
          </span>
          <span className="min-w-0 truncate">{oneLine(text)}</span>
        </>
      }
    >
      <div className="mb-2 ml-5 mt-1 overflow-y-auto" style={{ maxHeight: 320, whiteSpace: "pre-wrap" }}>
        {text}
      </div>
    </Fold>
  );
}

/** A tool call: its name and what it was given, and opened, its input and result. */
function ToolStep({ step }: { step: Extract<TraceItem, { kind: "step" }> }) {
  const { did, what } = stepWords(step.tool, step.input);
  const inner = step.steps?.length ?? 0;
  return (
    <Fold
      head={
        <>
          <ToolIcon tool={step.tool} color={step.error ? "var(--color-warning-text)" : "var(--color-label)"} />
          <span
            className="shrink-0 font-medium"
            style={{ color: step.error ? "var(--color-warning-text)" : "var(--color-deep-navy)" }}
          >
            {did}
          </span>
          {what && <span className="min-w-0 truncate">{what}</span>}
          {inner > 0 && <span className="shrink-0">· {inner} step{inner === 1 ? "" : "s"}</span>}
          {step.error && (
            <span className="shrink-0" style={{ color: "var(--color-warning-text)" }}>
              · failed
            </span>
          )}
        </>
      }
    >
      <div className="mb-2 ml-5 mt-1 flex flex-col gap-2">
        <TraceBlock label={`input to ${step.tool}`} text={readable(step.input)} />
        {inner > 0 && <TraceList items={step.steps ?? []} leftOut={step.left_out ?? 0} />}
        <TraceBlock label={step.error ? "error" : "result"} text={step.result} />
      </div>
    </Fold>
  );
}

/** The icon of a tool, by what it does. */
function ToolIcon({ tool, color }: { tool: string; color: string }) {
  const look = { size: 14, className: "shrink-0", "aria-hidden": true, style: { color } } as const;
  if (tool === "web_search") return <Search {...look} />;
  if (tool === "web_fetch") return <Globe {...look} />;
  if (tool === "shell" || tool === "process") return <Terminal {...look} />;
  if (tool.startsWith("file_") || tool === "send_file" || tool === "view_image" || tool === "transcribe")
    return <FileText {...look} />;
  if (tool.startsWith("memory_")) return <Database {...look} />;
  if (tool === "conversation_search") return <MessagesSquare {...look} />;
  if (tool === "subagent_spawn" || tool === "agent_query") return <Bot {...look} />;
  if (tool === "schedule") return <CalendarClock {...look} />;
  return <Wrench {...look} />;
}

/** A tool call as a person reads it: what was done, and to what. */
function stepWords(tool: string, input: string): { did: string; what: string } {
  const brief = briefInput(input);
  const words: Record<string, string> = {
    web_search: "Searched the web",
    web_fetch: "Read",
    memory_search: "Searched memory",
    memory_remember: "Remembered",
    memory_store: "Remembered",
    memory_forget: "Forgot",
    conversation_search: "Searched past chats",
    file_read: "Read file",
    file_write: "Wrote file",
    file_edit: "Edited file",
    file_find: "Found files",
    file_search: "Searched files",
    shell: "Ran",
    process: "Checked a command",
    send_file: "Sent",
    view_image: "Looked at",
    transcribe: "Transcribed",
    schedule: "Scheduled",
    set_timezone: "Set the time zone",
    skill_manage: "Updated a skill",
    subagent_spawn: "Handed off",
    agent_query: "Asked another agent",
    ask_choice: "Asked",
  };
  if (tool === "web_fetch") {
    try {
      const url = new URL(brief);
      const path = url.pathname === "/" ? "" : url.pathname;
      return { did: "Read", what: `${url.hostname.replace(/^www\./, "")}${path}` };
    } catch {
      return { did: "Read", what: brief };
    }
  }
  if (tool.startsWith("mcp__")) {
    const [, server, name] = tool.split("__");
    return { did: `Used ${name ?? tool}`, what: server ? `(${server}) ${brief}` : brief };
  }
  return { did: words[tool] ?? tool, what: brief };
}

/** A call's input as JSON laid out to read, or as it was when it is not JSON. */
function readable(input: string): string {
  try {
    return JSON.stringify(JSON.parse(input), null, 2);
  } catch {
    return input;
  }
}

/** A call's input or result, as it was, scrolling within its own box. */
function TraceBlock({ label, text }: { label: string; text: string }) {
  return (
    <div>
      <div className="mb-0.5 font-medium" style={{ color: "var(--color-label)" }}>
        {label}
      </div>
      <pre
        className="overflow-y-auto rounded px-2 py-1.5 font-mono text-xs"
        style={{
          maxHeight: 240,
          whiteSpace: "pre-wrap",
          wordBreak: "break-word",
          background: "var(--color-surface)",
          border: "1px solid var(--color-border-default)",
        }}
      >
        {text}
      </pre>
    </div>
  );
}

/**
 * A call in a conversation that is not the owner's (issue #255): who asked
 * and what, the call, and, asked after it ran, all the agent gets if the
 * owner lets it; with a note for what not to say.
 */
function OwnerApproval({
  request,
  disabled,
  onDecide,
}: {
  request: ApprovalRequest;
  disabled: boolean;
  onDecide: (approve: boolean, remember: boolean, note?: string) => void;
}) {
  const [note, setNote] = useState("");
  let shown: { from?: string; said?: string; input?: unknown; found?: string; owner?: boolean } = {};
  try {
    const parsed: unknown = JSON.parse(request.input);
    shown = parsed && typeof parsed === "object" ? (parsed as typeof shown) : { found: request.input };
  } catch {
    shown = { found: request.input };
  }
  const before = request.kind === "call";
  const muted = { color: "var(--color-body)" };
  const given = note.trim() || undefined;
  return (
    <div
      className="card px-4 py-3 text-sm"
      role="alertdialog"
      aria-label={before ? `allow ${request.tool}` : `use what ${request.tool} gave`}
    >
      <p className="mb-1">
        <strong>{before ? "Let the agent run this?" : "Let the agent use what this gave?"}</strong>{" "}
        <span className="font-mono">{request.tool}</span>{" "}
        <span style={muted}>
          {shown.owner
            ? "You asked in a group, which the others there read; always allowing it there allows it for each of them too"
            : "Someone other than you asked"}
          {request.conversation ? ` (${request.conversation})` : ""}.
        </span>
      </p>
      <dl className="mb-2 grid gap-1 text-xs" style={{ gridTemplateColumns: "auto 1fr" }}>
        <dt style={muted}>from</dt>
        <dd className="font-mono wrap-anywhere">{shown.from || "?"}</dd>
        <dt style={muted}>they wrote</dt>
        <dd className="wrap-anywhere" style={{ whiteSpace: "pre-wrap" }}>{shown.said || "?"}</dd>
      </dl>
      <pre className="mb-2 max-h-40 overflow-auto whitespace-pre-wrap text-xs font-mono" style={muted}>
        {JSON.stringify(shown.input ?? {}, null, 2)}
      </pre>
      {!before && (
        <>
          <p className="mb-1 text-xs" style={muted}>
            It gave, which is exactly what the agent gets:
          </p>
          <pre className="mb-2 max-h-64 overflow-auto whitespace-pre-wrap text-xs font-mono" style={muted}>
            {shown.found}
          </pre>
        </>
      )}
      <label className="mb-2 block text-xs" style={muted}>
        What not to say (optional), such as &quot;not my address&quot;; with always allow it stays the
        conversation&apos;s rule
        <textarea
          className="input mt-1 w-full"
          rows={2}
          maxLength={1000}
          value={note}
          onChange={(e) => setNote(e.target.value)}
        />
      </label>
      <div className="flex flex-wrap gap-2">
        <button type="button" className="btn-primary" disabled={disabled} onClick={() => onDecide(true, false, given)}>
          {before ? "allow once" : "use"}
        </button>
        <button
          type="button"
          className="btn-ghost"
          disabled={disabled}
          onClick={() => onDecide(true, true, given)}
          title={
            before
              ? "Let this exact call run in this conversation from now on, without asking"
              : `Let ${request.tool} answer in this conversation from now on, without asking`
          }
        >
          {before ? "always allow this call there" : "always allow there"}
        </button>
        <button type="button" className="btn-ghost" disabled={disabled} onClick={() => onDecide(false, false)}>
          deny
        </button>
      </div>
    </div>
  );
}

/** The reply as the model writes it, before the reply itself comes. */
function LiveBubble({ text }: { text: string }) {
  const inner = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState<number | undefined>(undefined);
  useEffect(() => {
    const el = inner.current;
    if (!el) return;
    const watch = new ResizeObserver(() => setHeight(el.offsetHeight));
    watch.observe(el);
    return () => watch.disconnect();
  }, []);
  return (
    <div className="enter flex flex-col items-start">
      <div className="live-grow w-full" style={{ height }}>
        <div ref={inner} style={{ color: "var(--color-deep-navy)" }} aria-live="polite">
          <Markdown>{text}</Markdown>
        </div>
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
    process: "Checking a background command",
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
    transcribe: "Transcribing",
    schedule: "Scheduling",
    set_timezone: "Setting the time zone",
    subagent_spawn: "Handing off a task",
    agent_query: "Asking another agent",
  };
  const verb = verbs[tool] ?? `Using ${tool}`;
  return detail ? `${verb}: ${detail}` : verb;
}

/**
 * Inline "agent is replying" placeholder. Three pulsing dots in an
 * agent-shaped bubble — same styling as a real agent message so the
 * eye doesn't have to retrain. Replaced by the actual reply when
 * `agent_message` arrives (parent flips `pending` to false). Above it,
 * the step running, which fades in as each one starts.
 */
function TypingBubble({ step }: { step: Step | null }) {
  return (
    <div className="enter flex flex-col items-start gap-1">
      <p className="wrap-anywhere px-1 text-xs" style={{ color: "var(--color-body)" }} role="status">
        {step && (
          <span key={stepText(step)} className="enter flex items-center gap-1.5">
            <span
              aria-hidden="true"
              className="shrink-0"
              style={{
                width: 6,
                height: 6,
                borderRadius: "50%",
                background: "var(--color-stripe-purple)",
                animation: "havn-breathe 1.6s ease-in-out infinite",
              }}
            />
            {stepText(step)}
          </span>
        )}
      </p>
      <div
        className="flex items-center gap-1 py-2"
        style={{ color: "var(--color-body)" }}
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
  models,
  chatModel,
  chatThinking,
  onSettings,
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
  /** The models the agent may use, and the chat's own choices. */
  models: AgentModels | null;
  chatModel: string | null;
  chatThinking: string | null;
  onSettings: (settings: { model?: string | null; thinking?: string | null }) => void;
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
  // Nothing to preview once the text is gone: back to writing.
  if (tab === "preview" && !draft.trim()) setTab("write");
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
      className="px-4 pb-4 pt-2 sm:px-8"
    >
      <div className="mx-auto max-w-3xl">
        {suggested.length > 0 && tab === "write" && (
          <ul
            className="card enter mb-2 overflow-y-auto py-1 text-sm"
            role="listbox"
            aria-label="Commands"
            style={{ maxHeight: 240, backgroundColor: "var(--color-surface)", boxShadow: "var(--shadow-ambient)" }}
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

        {/* One panel: what is attached, the text (or its preview), and a
            bar of what goes with it. */}
        <div className="composer">
          {(attached.length > 0 || uploading > 0) && (
            <div className="flex flex-wrap items-center gap-2 px-3 pt-3">
              <AttachmentCards agentId={agentId} files={attached} onRemove={(i) => onDetach(attached[i].path)} />
              {uploading > 0 && (
                <span className="inline-flex items-center gap-1 text-xs" role="status" style={{ color: "var(--color-body)" }}>
                  <Spinner /> uploading
                </span>
              )}
            </div>
          )}

          {tab === "write" ? (
            <MessageInput
              bare
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
              className="w-full overflow-y-auto px-3 pb-1 pt-3"
              style={{ minHeight: 56, maxHeight: 360, color: "var(--color-deep-navy)" }}
              aria-label="preview of the message"
            >
              <Markdown>{draft}</Markdown>
            </div>
          )}

          <div className="flex flex-wrap items-center gap-1 px-2 pb-2 sm:flex-nowrap">
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
              className="tool-button"
              onClick={() => fileInput.current?.click()}
              disabled={!ready}
              title="Attach files: they are saved in the agent's inbox/ and sent with your message (up to 25 MB each)"
              aria-label="attach files"
            >
              <Paperclip size={16} className="shrink-0" aria-hidden="true" />
            </button>
            <button
              type="button"
              className="tool-button"
              aria-pressed={tab === "preview"}
              onClick={() => setTab(tab === "write" ? "preview" : "write")}
              disabled={tab === "write" && !draft.trim()}
              title={tab === "write" ? "Preview the message as it will show (markdown)" : "Back to writing"}
            >
              {tab === "write" ? (
                <Eye size={16} className="shrink-0" aria-hidden="true" />
              ) : (
                <Pencil size={16} className="shrink-0" aria-hidden="true" />
              )}
              <span className="text-[13px]">{tab === "write" ? "preview" : "write"}</span>
            </button>
            {models && models.models.length > 0 && (
              <div className="flex min-w-0 items-center gap-1">
                <ChatModelMenus
                  models={models}
                  chatModel={chatModel}
                  chatThinking={chatThinking}
                  disabled={!ready}
                  onSettings={onSettings}
                />
              </div>
            )}
            <div className="ml-auto flex shrink-0 items-center gap-2">
              {pending && (
                <span
                  className="enter flex items-center gap-1.5 text-[13px]"
                  style={{ color: "var(--color-body)" }}
                  role="status"
                >
                  <span
                    aria-hidden="true"
                    className="shrink-0"
                    style={{
                      width: 6,
                      height: 6,
                      borderRadius: "50%",
                      background: "var(--color-stripe-purple)",
                      animation: "havn-breathe 1.6s ease-in-out infinite",
                    }}
                  />
                  replying
                  {waiting > 1 && `, ${waiting - 1} queued`}
                </span>
              )}
              {pending && (
                <button
                  type="button"
                  className="tool-button"
                  onClick={onStop}
                  disabled={wsState !== "open"}
                  title="Stop the reply in progress; queued messages still get answered"
                >
                  <Square size={14} className="shrink-0" aria-hidden="true" />
                  <span className="text-[13px]">stop</span>
                </button>
              )}
              <button
                type="submit"
                className="btn-primary inline-flex items-center gap-1.5"
                disabled={!ready || !canSend}
                style={{ padding: "7px 14px", fontSize: 14 }}
              >
                <ArrowUp size={16} className="shrink-0" aria-hidden="true" />
                {pending ? "queue" : "send"}
              </button>
            </div>
          </div>
        </div>
      </div>
    </form>
  );
}

/** The most messages an export reads: the newest of a longer chat. */
const EXPORT_LIMIT = 500;

/** A chat's messages as a file to keep (issue #229). */
type ExportedChat = {
  chat: { id: string; title: string };
  agent: { id: string; name: string };
  exported_at: string;
  /** Fewer than the chat holds: its newest only. */
  newest_only: boolean;
  messages: { role: "you" | "agent"; at: string; text: string; files: { name: string; path: string }[] }[];
  sent_files: { name: string; caption?: string; at: string }[];
};

/** Chat `chat` of agent `agentId`, its branch on screen, read for export. */
async function readChat(
  agentId: string,
  agentName: string,
  chat: string,
  title: string,
  scope: string | null,
): Promise<ExportedChat> {
  const [history, sent] = await Promise.all([
    api.conversation.list(agentId, EXPORT_LIMIT, chat, scope ?? undefined),
    api.outbox.list(agentId, chat),
  ]);
  const messages = history.turns
    .filter((t) => t.role === "user" || t.role === "assistant")
    .map((t) => {
      const { text, files } = t.role === "user" ? splitMessage(t.content) : { text: t.content, files: [] };
      return {
        role: t.role === "user" ? ("you" as const) : ("agent" as const),
        at: t.created_at,
        text,
        files: files.map((f) => ({ name: f.name, path: f.path })),
      };
    });
  return {
    chat: { id: chat, title },
    agent: { id: agentId, name: agentName },
    exported_at: new Date().toISOString(),
    newest_only: history.turns.length >= EXPORT_LIMIT,
    messages,
    sent_files: sent.map((f) => ({ name: f.name, caption: f.caption, at: f.created_at })),
  };
}

/** An exported chat as Markdown: a heading per message, its files listed. */
function chatMarkdown(c: ExportedChat): string {
  const when = (at: string) => new Date(at).toLocaleString();
  const lines = [
    `# ${c.chat.title}`,
    "",
    `A chat with ${c.agent.name}, exported ${when(c.exported_at)}.` +
      (c.newest_only ? ` Its newest ${EXPORT_LIMIT} messages only.` : ""),
  ];
  for (const m of c.messages) {
    lines.push("", `## ${m.role === "you" ? "You" : c.agent.name} · ${when(m.at)}`, "", m.text.trim());
    if (m.files.length > 0) {
      lines.push("", ...m.files.map((f) => `- Attached: ${f.name} (${f.path})`));
    }
  }
  if (c.sent_files.length > 0) {
    lines.push("", "## Files the agent sent", "");
    for (const f of c.sent_files) {
      lines.push(`- ${f.name}${f.caption ? `: ${f.caption}` : ""} (${when(f.at)})`);
    }
  }
  return `${lines.join("\n")}\n`;
}

/** Hand `text` to the browser as a file named `name`. */
function saveFile(name: string, type: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  document.body.appendChild(link);
  link.click();
  link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

/** A file name from a chat's title: its letters and digits, joined by dashes. */
function exportName(title: string, ext: string): string {
  const dashed = title.normalize("NFKC").replace(/[^\p{L}\p{M}\p{N}]+/gu, "-");
  // Cut by characters, not UTF-16 units, then trimmed of dashes.
  const base = Array.from(dashed).slice(0, 60).join("").replace(/^-+|-+$/g, "");
  return `${base || "chat"}.${ext}`;
}

/**
 * The chat on screen, saved as a file: Markdown to read, or JSON with each
 * message, the files it carried and those the agent sent. Its branch on
 * screen, up to its newest 500 messages.
 */
function ExportMenu({
  agentId,
  agentName,
  chat,
  title,
  scope,
}: {
  agentId: string;
  agentName: string;
  chat: string | null;
  title: string;
  scope: string | null;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function save(format: "md" | "json") {
    if (!chat) return;
    setBusy(true);
    setError(null);
    try {
      const read = await readChat(agentId, agentName, chat, title, scope);
      if (format === "md") saveFile(exportName(title, "md"), "text/markdown;charset=utf-8", chatMarkdown(read));
      else saveFile(exportName(title, "json"), "application/json", `${JSON.stringify(read, null, 2)}\n`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  return (
    <span className="inline-flex items-center gap-1" title="Save this chat as a file">
      <Download size={16} className="shrink-0" aria-hidden="true" style={{ color: "var(--color-body)" }} />
      <select
        className="composer-select"
        aria-label="export this chat"
        value=""
        disabled={!chat || busy}
        onChange={(e) => {
          const format = e.target.value;
          e.target.value = "";
          if (format === "md" || format === "json") void save(format);
        }}
      >
        <option value="">{busy ? "exporting…" : "export"}</option>
        <option value="md">as Markdown</option>
        <option value="json">as JSON</option>
      </select>
      {error && (
        <span role="alert" className="text-xs" style={{ color: "var(--color-warning-text)" }}>
          {error}
        </span>
      )}
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
