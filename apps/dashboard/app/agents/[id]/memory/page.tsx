"use client";

import { use, useEffect, useState } from "react";
import Link from "next/link";
import { ApiError, api, type MemoryEntry, type MemoryKind } from "@/lib/api";

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

/**
 * `/agents/[id]/memory` — view + prune what the agent remembers.
 * Reads go straight to agent.db RO (works even when the agent is
 * offline). Delete buttons route through the agent socket as
 * `MemoryForgetRequest` frames so agent.db stays single-writer
 * (spec §5.2). The "delete" button shows a 409-aware message when
 * the agent isn't running.
 */
export default function MemoryPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);

  const [entries, setEntries] = useState<MemoryEntry[] | null>(null);
  const [uninitialised, setUninitialised] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [pendingForget, setPendingForget] = useState<string | null>(null);
  const [forgetError, setForgetError] = useState<string | null>(null);

  async function load() {
    try {
      const r = await api.memory.list(id);
      setEntries(r.entries);
      setUninitialised(r.uninitialised);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    let cancelled = false;
    (async () => {
      const r = await api.memory.list(id).catch((e) => {
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
            "Start the agent before deleting memory — agent.db is single-writer (spec §5.2).",
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

  const grouped = groupByKind(entries ?? []);

  return (
    <div className="mx-auto max-w-5xl px-8 py-12">
      <header className="mb-8">
        <Link
          href={`/agents/${id}`}
          className="mb-2 inline-block text-sm"
          style={{ color: "var(--color-body)" }}
        >
          ← back to chat
        </Link>
        <h1 className="display-h1">what the agent remembers about you</h1>
        <p
          className="mt-2 max-w-2xl"
          style={{ color: "var(--color-body)", fontSize: 18, lineHeight: 1.4 }}
        >
          Everything below was written by the agent or you, grouped by kind.
          Values are exactly what the agent sees on its next turn — no
          summary, no transformation.
        </p>
      </header>

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
        <div className="card-elevated px-8 py-12 text-center">
          <p className="display-h3 mb-2">no memory yet</p>
          <p style={{ color: "var(--color-body)" }}>
            The agent hasn't been started yet, so its database hasn't been
            initialised. Start the agent and have a conversation; come back
            here to see what it remembered.
          </p>
        </div>
      )}

      {entries !== null && !uninitialised && entries.length === 0 && (
        <div className="card-elevated px-8 py-12 text-center">
          <p className="display-h3 mb-2">no memory yet</p>
          <p style={{ color: "var(--color-body)" }}>
            The agent has been running but hasn't written any memory yet.
            Tell it something to remember, or have a conversation — it'll
            infer what's worth keeping during the next heartbeat.
          </p>
        </div>
      )}

      {entries !== null && !uninitialised && entries.length > 0 && (
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
                  {rows.map((e) => (
                    <li key={e.key} className="card px-4 py-3">
                      <div className="flex items-start justify-between gap-4">
                        <div className="min-w-0 flex-1">
                          <code
                            className="font-mono text-xs"
                            style={{ color: "var(--color-body)" }}
                          >
                            {e.key}
                          </code>
                          <p className="mt-1">{e.value}</p>
                        </div>
                        <div className="shrink-0 text-right text-xs">
                          <SourcePill source={e.source} />
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
                        </div>
                      </div>
                    </li>
                  ))}
                </ul>
              </section>
            );
          })}
        </div>
      )}
    </div>
  );
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
