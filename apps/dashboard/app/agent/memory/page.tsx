"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { useSearchParams } from "next/navigation";
import { Layers, Search } from "lucide-react";
import {
  ApiError,
  api,
  type EmbeddingStatus,
  type MemoryEntry,
  type MemoryExpiry,
  type MemoryKind,
  type MemoryVisibility,
} from "@/lib/api";

const KIND_ORDER: MemoryKind[] = ["identity", "preference", "project", "event"];

const KIND_LABEL: Record<MemoryKind, string> = {
  identity: "Identity",
  preference: "Preferences",
  project: "Project",
  event: "Recent events",
};

const KIND_HINT: Record<MemoryKind, string> = {
  identity: "Stable facts about you. Never auto-expires.",
  preference: "Durable preferences and corrections you've given.",
  project: "Facts about current work; expire after a while if not reinforced.",
  event:
    "Discrete things that happened. Auto-expire after a month unless cited recently.",
};

/** Longest memory key the gateway accepts. */
const MAX_KEY_CHARS = 256;
/** Entries loaded, and exported: the most the gateway lists. */
const LIST_LIMIT = 1000;

/** Days a kind keeps an unused entry by default, by source (null: until
 * forgotten); mirrors `Kind::default_ttl_days` in havn-db. */
const DEFAULT_DAYS: Record<MemoryKind, Record<MemoryEntry["source"], number | null>> = {
  identity: { user_told: null, agent_inferred: null },
  preference: { user_told: null, agent_inferred: 180 },
  project: { user_told: 180, agent_inferred: 90 },
  event: { user_told: 90, agent_inferred: 30 },
};

/**
 * `/agent/memory?id=` — view, edit, add and export what the agent
 * remembers.
 * Reads are answered by the running agent from its own agent.db (409
 * when it is stopped). Delete buttons route through the agent socket as
 * `MemoryForgetRequest` frames so agent.db stays single-writer
 * (spec §5.2). The "delete" button shows a 409-aware message when
 * the agent isn't running.
 */
export default function MemoryPage() {
  const id = useSearchParams().get("id") ?? "";
  return <Memory key={id} id={id} />;
}

function Memory({ id }: { id: string }) {

  const [entries, setEntries] = useState<MemoryEntry[] | null>(null);
  const [embedding, setEmbedding] = useState<EmbeddingStatus | null>(null);
  const [uninitialised, setUninitialised] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pendingForget, setPendingForget] = useState<string | null>(null);
  const [forgetError, setForgetError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [kindFilter, setKindFilter] = useState<MemoryKind | "all">("all");
  const [sourceFilter, setSourceFilter] = useState<MemoryEntry["source"] | "all">("all");
  const [visibilityFilter, setVisibilityFilter] = useState<MemoryVisibility | "suggested" | "all">(
    "all",
  );
  const [pendingSwitch, setPendingSwitch] = useState<string | null>(null);
  const [editing, setEditing] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  async function load() {
    try {
      const r = await api.memory.list(id, undefined, LIST_LIMIT);
      setEntries(r.entries);
      setUninitialised(r.uninitialised);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  // How memory is searched (spec §9.4 v0.7), the same for every agent.
  useEffect(() => {
    let cancelled = false;
    api.embedding().then(
      (e) => {
        if (!cancelled) setEmbedding(e);
      },
      () => {},
    );
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const r = await api.memory.list(id, undefined, LIST_LIMIT).catch((e) => {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
        return null;
      });
      if (cancelled || !r) return;
      setEntries(r.entries);
      setUninitialised(r.uninitialised);
      setError(null);
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

  async function onForget(key: string) {
    if (
      !confirm(
        `Forget the memory under "${key}"? The row is soft-archived (audit trail kept) but the agent stops seeing it.`,
      )
    ) {
      return;
    }
    setPendingForget(key);
    setForgetError(null);
    try {
      await api.memory.forget(id, key);
      await load();
    } catch (e) {
      if (e instanceof ApiError) {
        if (e.status === 409) {
          setForgetError(
            "Start the agent before deleting memory — agent.db is single-writer.",
          );
        } else if (e.status === 404) {
          // Already gone — refresh.
          await load();
        } else {
          setForgetError(e.message);
        }
      } else {
        setForgetError(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setPendingForget(null);
    }
  }

  // Only the owner can; making one private does not take back what others
  // already read (issue #309).
  async function onSwitch(e: MemoryEntry) {
    const next: MemoryVisibility = e.visibility === "public" ? "private" : "public";
    const ask =
      next === "public"
        ? `Make "${e.key}" public? It is then in the agent's prompt in every chat, other people's and groups' too.`
        : `Make "${e.key}" private? From now on it is in your own private chats only. What others already read cannot be taken back.`;
    if (!confirm(ask)) return;
    setPendingSwitch(e.key);
    setForgetError(null);
    try {
      await api.memory.setVisibility(id, e.key, next, e.id);
      await load();
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        setForgetError("This memory changed since, or the agent is stopped; reload and try again.");
        await load();
      } else {
        setForgetError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setPendingSwitch(null);
    }
  }

  // Save an added or edited memory; the error, if any, for the form.
  // Only the form that saved closes.
  async function onSave(
    key: string,
    entry: { value: string; kind: MemoryKind; expiry: MemoryExpiry },
    added: boolean,
  ): Promise<string | null> {
    try {
      await api.memory.save(id, key, entry);
      await load();
      if (added) setAdding(false);
      else setEditing((current) => (current === key ? null : current));
      return null;
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        return "Start the agent before changing memory — agent.db is single-writer.";
      }
      return e instanceof Error ? e.message : String(e);
    }
  }

  const needle = query.trim().toLowerCase();
  const shown = (entries ?? []).filter(
    (e) =>
      (kindFilter === "all" || e.kind === kindFilter) &&
      (sourceFilter === "all" || e.source === sourceFilter) &&
      (visibilityFilter === "all" ||
        (visibilityFilter === "suggested"
          ? e.visibility === "private" && e.suggested_public
          : e.visibility === visibilityFilter)) &&
      (!needle || e.key.toLowerCase().includes(needle) || e.value.toLowerCase().includes(needle)),
  );
  const grouped = groupByKind(shown);

  return (
    <div className="mx-auto max-w-5xl px-4 py-8 sm:px-8 sm:py-12">
      <header className="mb-8">
        <Link
          href={`/agent?id=${id}`}
          className="mb-2 inline-block text-sm"
          style={{ color: "var(--color-body)" }}
        >
          ← back to chat
        </Link>
        <div className="flex flex-wrap items-center gap-3">
          <h1 className="display-h1">what the agent remembers about you</h1>
          <RetrievalPill embedding={embedding} />
        </div>
        <p
          className="mt-2 max-w-2xl"
          style={{ color: "var(--color-body)", fontSize: 18, lineHeight: 1.4 }}
        >
          Everything below was written by the agent or you, grouped by kind.
          Values are exactly what the agent sees on its next turn — no
          summary, no transformation. What you add or edit here counts as
          something you said.
        </p>
        <p className="mt-2 max-w-2xl text-sm" role="note" style={{ color: "var(--color-body)" }}>
          A private memory is in the agent&apos;s prompt in your own private chats only;
          elsewhere the agent reaches it only by searching memory, which waits for your
          review. A public one is in its prompt in every chat, other people&apos;s and
          groups&apos; too: your name, how to address you, your language. Everything the
          agent saves is private; in your own private chats it may suggest that one be
          public, and only you make it public, here. When the agent changes a public memory,
          the new text is private again until you approve it. Making one private again stops it being shown
          from then on; what others already read cannot be taken back.
        </p>
      </header>

      {entries !== null && !uninitialised && (
        <div className="mb-6 flex flex-wrap items-center gap-2">
          <input
            type="search"
            className="input min-w-0 flex-1"
            placeholder="search memory"
            aria-label="search memory"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          <select
            className="input w-auto"
            aria-label="kind"
            value={kindFilter}
            onChange={(e) => setKindFilter(e.target.value as MemoryKind | "all")}
          >
            <option value="all">all kinds</option>
            {KIND_ORDER.map((k) => (
              <option key={k} value={k}>
                {KIND_LABEL[k]}
              </option>
            ))}
          </select>
          <select
            className="input w-auto"
            aria-label="source"
            value={sourceFilter}
            onChange={(e) => setSourceFilter(e.target.value as MemoryEntry["source"] | "all")}
          >
            <option value="all">anyone</option>
            <option value="user_told">you said</option>
            <option value="agent_inferred">inferred</option>
          </select>
          <select
            className="input w-auto"
            aria-label="visibility"
            value={visibilityFilter}
            onChange={(e) =>
              setVisibilityFilter(e.target.value as MemoryVisibility | "suggested" | "all")
            }
          >
            <option value="all">private and public</option>
            <option value="private">private</option>
            <option value="suggested">suggested public</option>
            <option value="public">public</option>
          </select>
          <button type="button" className="btn-primary" onClick={() => setAdding(true)} disabled={adding}>
            add memory
          </button>
          <button
            type="button"
            className="btn-ghost"
            onClick={() => download(`memory-${id}.json`, "application/json", JSON.stringify(entries, null, 2))}
            disabled={entries.length === 0}
            title="Every active memory, as JSON"
          >
            export JSON
          </button>
          <button
            type="button"
            className="btn-ghost"
            onClick={() => download(`memory-${id}.md`, "text/markdown", asMarkdown(entries))}
            disabled={entries.length === 0}
            title="Every active memory, as Markdown"
          >
            export Markdown
          </button>
        </div>
      )}

      {adding && (
        <div className="card mb-6 px-4 py-4">
          <MemoryForm onSave={onSave} onCancel={() => setAdding(false)} />
        </div>
      )}

      {error && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {error}
        </div>
      )}

      {forgetError && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {forgetError}
        </div>
      )}

      {entries === null && !error && (
        <div
          className="card px-4 py-6 text-sm"
          style={{ color: "var(--color-body)" }}
        >
          loading…
        </div>
      )}

      {entries !== null && uninitialised && (
        <div className="card-elevated px-6 py-12 text-center sm:px-8">
          <p className="display-h3 mb-2">no memory yet</p>
          <p style={{ color: "var(--color-body)" }}>
            The agent hasn&apos;t been started yet, so its database hasn&apos;t been
            initialised. Start the agent and have a conversation; come back
            here to see what it remembered.
          </p>
        </div>
      )}

      {entries !== null && !uninitialised && entries.length === 0 && (
        <div className="card-elevated px-6 py-12 text-center sm:px-8">
          <p className="display-h3 mb-2">no memory yet</p>
          <p style={{ color: "var(--color-body)" }}>
            The agent has been running but hasn&apos;t written any memory yet.
            Tell it something to remember, or have a conversation — it&apos;ll
            infer what&apos;s worth keeping during the next heartbeat.
          </p>
        </div>
      )}

      {entries !== null && !uninitialised && entries.length > 0 && shown.length === 0 && (
        <p className="text-sm" style={{ color: "var(--color-body)" }}>
          Nothing matches.
        </p>
      )}

      {entries !== null && !uninitialised && shown.length > 0 && (
        <div className="grid gap-8">
          {KIND_ORDER.map((kind) => {
            const rows = grouped[kind] ?? [];
            if (rows.length === 0) return null;
            return (
              <section key={kind}>
                <h2 className="display-h3 mb-1">{KIND_LABEL[kind]}</h2>
                <p
                  className="mb-3 text-sm"
                  style={{ color: "var(--color-body)" }}
                >
                  {KIND_HINT[kind]}
                </p>
                <ul className="grid gap-2">
                  {rows.map((e) =>
                    editing === e.key ? (
                      <li key={e.key} className="card px-4 py-4">
                        <MemoryForm entry={e} onSave={onSave} onCancel={() => setEditing(null)} />
                      </li>
                    ) : (
                    <li key={e.key} className="card px-4 py-3">
                      <div className="flex items-start justify-between gap-4">
                        <div className="min-w-0 flex-1">
                          <code
                            className="font-mono text-xs"
                            style={{ color: "var(--color-body)" }}
                          >
                            {e.key}
                          </code>
                          <p className="mt-1" style={{ whiteSpace: "pre-wrap" }}>
                            {e.value}
                          </p>
                          <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
                            {lifetime(e)}
                          </p>
                        </div>
                        <div className="shrink-0 text-right text-xs">
                          <SourcePill source={e.source} />
                          <button
                            type="button"
                            role="switch"
                            aria-checked={e.visibility === "public"}
                            aria-label={`public: ${e.key}`}
                            onClick={() => onSwitch(e)}
                            disabled={pendingSwitch === e.key}
                            className="pill ml-1 disabled:opacity-40"
                            title={
                              e.visibility === "public"
                                ? "Public: in the prompt of every chat, other people's and groups' too. Click to make it private."
                                : "Private: in your own private chats only. Click to make it public, for anyone who talks to the agent."
                            }
                            style={
                              e.visibility === "public"
                                ? { background: "rgba(234, 179, 8, 0.15)", color: "var(--color-warning-text)" }
                                : { background: "rgba(100, 116, 141, 0.1)", color: "var(--color-body)" }
                            }
                          >
                            {pendingSwitch === e.key
                              ? "switching…"
                              : e.visibility === "private" && e.suggested_public
                                ? "suggested public: make public"
                                : e.visibility}
                          </button>
                          <p
                            className="mt-1 font-mono"
                            style={{ color: "var(--color-body)" }}
                          >
                            cited {e.recall_count}×
                          </p>
                          <button
                            type="button"
                            onClick={() => onForget(e.key)}
                            disabled={pendingForget === e.key}
                            className="mt-2 text-xs underline disabled:opacity-40"
                            style={{ color: "var(--color-ruby)" }}
                            title="Soft-archive this row. The agent stops seeing it; the audit chain remains."
                          >
                            {pendingForget === e.key ? "forgetting…" : "forget"}
                          </button>
                          <button
                            type="button"
                            onClick={() => setEditing(e.key)}
                            className="mt-2 ml-3 text-xs underline"
                            style={{ color: "var(--color-purple)" }}
                            title="Change its text, kind or how long it is kept. The old version is kept in the audit chain."
                          >
                            edit
                          </button>
                        </div>
                      </div>
                    </li>
                    ),
                  )}
                </ul>
              </section>
            );
          })}
        </div>
      )}
    </div>
  );
}

/** How long `e` is kept, in words. */
function lifetime(e: MemoryEntry): string {
  if (e.ttl_days === null) return "kept until forgotten";
  if (e.visibility === "public") return "kept until forgotten while public";
  return `forgotten after ${e.ttl_days} day${e.ttl_days === 1 ? "" : "s"} unused`;
}

/** The lifetime an edit starts from: the kind's own unless the entry's
 * differs from what its kind keeps it for. */
function expiryOf(e: MemoryEntry | undefined): { mode: "kind_default" | "never" | "days"; days: string } {
  if (!e || e.ttl_days === DEFAULT_DAYS[e.kind][e.source]) return { mode: "kind_default", days: "30" };
  if (e.ttl_days === null) return { mode: "never", days: "30" };
  return { mode: "days", days: String(e.ttl_days) };
}

function MemoryForm({
  entry,
  onSave,
  onCancel,
}: {
  /** The memory edited; absent when adding one. */
  entry?: MemoryEntry;
  onSave: (
    key: string,
    entry: { value: string; kind: MemoryKind; expiry: MemoryExpiry },
    added: boolean,
  ) => Promise<string | null>;
  onCancel: () => void;
}) {
  const initial = expiryOf(entry);
  const [key, setKey] = useState(entry?.key ?? "");
  const [value, setValue] = useState(entry?.value ?? "");
  const [kind, setKind] = useState<MemoryKind>(entry?.kind ?? "preference");
  const [mode, setMode] = useState(initial.mode);
  const [days, setDays] = useState(initial.days);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function submit(ev: React.FormEvent) {
    ev.preventDefault();
    const count = Math.round(Number(days));
    if (mode === "days" && !(count >= 1 && count <= 3650)) {
      setError("Keep it for 1 to 3650 days.");
      return;
    }
    setSaving(true);
    const expiry: MemoryExpiry = mode === "days" ? { days: count } : mode;
    // A new memory without a key gets one; the agent finds memories by
    // what they say, not by key.
    const chosen = entry ? entry.key : key.trim() || `note.${crypto.getRandomValues(new Uint32Array(1))[0].toString(16)}`;
    const failed = await onSave(chosen, { value, kind, expiry }, !entry);
    setSaving(false);
    setError(failed);
  }

  return (
    <form className="grid gap-3" onSubmit={submit}>
      {entry ? (
        <code className="font-mono text-xs" style={{ color: "var(--color-body)" }}>
          {entry.key}
        </code>
      ) : (
        <input
          className="input font-mono text-sm"
          placeholder="key, e.g. user.editor (optional)"
          aria-label="key"
          maxLength={MAX_KEY_CHARS}
          value={key}
          onChange={(e) => setKey(e.target.value)}
        />
      )}
      <textarea
        className="input min-h-20 text-sm"
        placeholder="what the agent should remember"
        aria-label="memory"
        value={value}
        onChange={(e) => setValue(e.target.value)}
        autoFocus
      />
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <select
          className="input w-auto"
          aria-label="kind"
          value={kind}
          onChange={(e) => setKind(e.target.value as MemoryKind)}
        >
          {KIND_ORDER.map((k) => (
            <option key={k} value={k}>
              {KIND_LABEL[k]}
            </option>
          ))}
        </select>
        <select
          className="input w-auto"
          aria-label="kept for"
          value={mode}
          onChange={(e) => setMode(e.target.value as typeof mode)}
        >
          <option value="kind_default">kept as long as its kind keeps it</option>
          <option value="never">kept until forgotten</option>
          <option value="days">forgotten after days unused…</option>
        </select>
        {mode === "days" && (
          <input
            type="number"
            className="input w-24"
            aria-label="days"
            min={1}
            max={3650}
            value={days}
            onChange={(e) => setDays(e.target.value)}
          />
        )}
      </div>
      {error && (
        <p className="text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <button type="button" className="btn-ghost" onClick={onCancel}>
          cancel
        </button>
        <button type="submit" className="btn-primary" disabled={saving || !value.trim()}>
          {saving ? "saving…" : "save"}
        </button>
      </div>
    </form>
  );
}

function download(name: string, type: string, text: string) {
  const url = URL.createObjectURL(new Blob([text], { type }));
  const a = document.createElement("a");
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  // Some browsers start the download after this returns.
  setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

function asMarkdown(entries: MemoryEntry[]): string {
  const grouped = groupByKind(entries);
  let out = "# Memory\n";
  for (const kind of KIND_ORDER) {
    if (grouped[kind].length === 0) continue;
    out += `\n## ${KIND_LABEL[kind]}\n\n`;
    for (const e of grouped[kind]) {
      const who = e.source === "user_told" ? "you said" : "inferred";
      const shown = e.visibility === "private" && e.suggested_public ? "private, suggested public" : e.visibility;
      out += `- **${e.key}**: ${e.value.replace(/\n/g, " ")} (${who}; ${shown}; ${lifetime(e)})\n`;
    }
  }
  return out;
}

function groupByKind(entries: MemoryEntry[]): Record<MemoryKind, MemoryEntry[]> {
  const out: Record<MemoryKind, MemoryEntry[]> = {
    identity: [],
    preference: [],
    project: [],
    event: [],
  };
  for (const e of entries) {
    out[e.kind].push(e);
  }
  return out;
}

function SourcePill({ source }: { source: MemoryEntry["source"] }) {
  if (source === "user_told") {
    return (
      <span
        className="pill"
        title="you said this directly"
        style={{
          background: "rgba(83, 58, 253, 0.1)",
          color: "var(--color-purple)",
        }}
      >
        you said
      </span>
    );
  }
  return (
    <span
      className="pill"
      title="the agent inferred this from context"
      style={{
        background: "rgba(100, 116, 141, 0.1)",
        color: "var(--color-body)",
      }}
    >
      inferred
    </span>
  );
}

/**
 * Whether retrieval (memory **and** skills — they share the same embedding
 * provider per spec §13 Phase 3) is hybrid (vector + keyword) or
 * keyword-only: whether semantic-near queries work, or the words of the
 * saved fact or skill description are needed.
 */
function RetrievalPill({ embedding }: { embedding: EmbeddingStatus | null }) {
  if (!embedding) return null;
  const hybrid = embedding.hybrid_enabled;
  const LabelIcon = hybrid ? Layers : Search;
  const tip = hybrid
    ? `Hybrid retrieval (memory + skills): vector (${embedding.provider}) + BM25 keyword. Semantic-near queries work even with no shared words.`
    : "FTS5 keyword-only (memory + skills). Set [embedding] in config.toml to enable hybrid retrieval.";
  return (
    <span
      className="pill pill-stopped"
      title={tip}
      style={{
        background: hybrid ? "rgba(83, 58, 253, 0.08)" : "var(--color-surface-muted)",
        color: hybrid ? "var(--color-stripe-purple)" : "var(--color-body)",
      }}
    >
      <LabelIcon size={12} className="shrink-0" aria-hidden="true" />
      {hybrid ? `search: ${embedding.provider} + keywords` : "search: keywords only"}
    </span>
  );
}
