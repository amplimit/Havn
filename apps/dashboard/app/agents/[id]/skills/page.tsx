"use client";

import { use, useEffect, useState } from "react";
import Link from "next/link";
import {
  ApiError,
  api,
  type CuratorReportFile,
  type SkillView,
} from "@/lib/api";

/**
 * `/agents/[id]/skills` — read-only audit view of:
 *   - what skills the agent has installed (bundled + workspace);
 *   - what the curator has consolidated / archived (its `.curator/`
 *     report log).
 *
 * Mirrors `/memory` in shape and data flow (gateway opens agent.db RO).
 * Pin/unpin and skill_manage CRUD live on the agent socket — surfaced
 * here once the dashboard-write proto frames land.
 */
export default function SkillsPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);

  const [active, setActive] = useState<SkillView[] | null>(null);
  const [archived, setArchived] = useState<SkillView[]>([]);
  const [uninitialised, setUninitialised] = useState(false);
  const [reports, setReports] = useState<CuratorReportFile[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [pendingPin, setPendingPin] = useState<string | null>(null);
  const [pinError, setPinError] = useState<string | null>(null);

  async function load() {
    const [s, r] = await Promise.all([
      api.skills.list(id),
      api.skills.curatorReports(id),
    ]);
    setActive(s.active);
    setArchived(s.archived);
    setUninitialised(s.uninitialised);
    setReports(r.reports);
    setError(null);
  }

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        await load();
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

  async function togglePin(name: string, currentlyPinned: boolean) {
    setPendingPin(name);
    setPinError(null);
    try {
      if (currentlyPinned) {
        await api.skills.unpin(id, name);
      } else {
        await api.skills.pin(id, name);
      }
      await load();
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        setPinError(
          "Start the agent before changing pins — skills_index is single-writer (spec §5.2).",
        );
      } else {
        setPinError(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setPendingPin(null);
    }
  }

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
        <h1 className="display-h1">skills + curator log</h1>
        <p
          className="mt-2 max-w-2xl"
          style={{ color: "var(--color-body)", fontSize: 18, lineHeight: 1.4 }}
        >
          Skills the agent has available, and the curator's history of what
          it consolidated or archived.
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

      {pinError && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {pinError}
        </div>
      )}

      {active === null && !error && (
        <div
          className="card px-4 py-6 text-sm"
          style={{ color: "var(--color-body)" }}
        >
          loading…
        </div>
      )}

      {active !== null && uninitialised && (
        <div className="card-elevated px-8 py-12 text-center">
          <p className="display-h3 mb-2">agent never started</p>
          <p style={{ color: "var(--color-body)" }}>
            Skills are indexed when the agent first runs. Start the agent and
            come back.
          </p>
        </div>
      )}

      {active !== null && !uninitialised && (
        <>
          <section className="mb-10">
            <h2 className="display-h3 mb-3">
              active skills{" "}
              <span style={{ color: "var(--color-body)", fontWeight: 300 }}>
                ({active.length})
              </span>
            </h2>
            {active.length === 0 ? (
              <p style={{ color: "var(--color-body)" }}>
                no skills installed.
              </p>
            ) : (
              <ul className="grid gap-2">
                {active.map((s) => (
                  <li key={s.name} className="card px-4 py-3">
                    <div className="flex items-start justify-between gap-4">
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <code
                            className="font-mono text-xs"
                            style={{ color: "var(--color-body)" }}
                          >
                            {s.name}
                          </code>
                          <SourcePill source={s.source} />
                          {s.pinned && <PinPill />}
                        </div>
                        <p className="mt-1 text-sm">{s.description}</p>
                      </div>
                      <div
                        className="shrink-0 text-right text-xs font-mono"
                        style={{ color: "var(--color-body)" }}
                      >
                        used {s.use_count}×
                        {s.last_used_at && (
                          <p>last: {formatDate(s.last_used_at)}</p>
                        )}
                        {s.source === "workspace" && (
                          <button
                            type="button"
                            onClick={() => togglePin(s.name, s.pinned)}
                            disabled={pendingPin === s.name}
                            className="mt-2 underline disabled:opacity-40"
                            style={{ color: "var(--color-purple)" }}
                            title={
                              s.pinned
                                ? "Allow the curator to consider this skill again."
                                : "Mark as load-bearing — the curator will skip it."
                            }
                          >
                            {pendingPin === s.name
                              ? "…"
                              : s.pinned
                                ? "unpin"
                                : "pin"}
                          </button>
                        )}
                      </div>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </section>

          {archived.length > 0 && (
            <section className="mb-10">
              <h2 className="display-h3 mb-3">
                archived (audit){" "}
                <span style={{ color: "var(--color-body)", fontWeight: 300 }}>
                  ({archived.length})
                </span>
              </h2>
              <p
                className="mb-3 text-sm"
                style={{ color: "var(--color-body)" }}
              >
                Skills the curator has retired. Files moved to
                <code className="mx-1 font-mono">.archive/</code>; rows kept
                so this trail stays visible.
              </p>
              <ul className="grid gap-2">
                {archived.map((s) => (
                  <li
                    key={s.name}
                    className="card px-4 py-3"
                    style={{ opacity: 0.65 }}
                  >
                    <div className="flex items-center gap-2">
                      <code
                        className="font-mono text-xs"
                        style={{ color: "var(--color-body)" }}
                      >
                        {s.name}
                      </code>
                      <SourcePill source={s.source} />
                    </div>
                    <p className="mt-1 text-sm">{s.description}</p>
                  </li>
                ))}
              </ul>
            </section>
          )}

          <section>
            <h2 className="display-h3 mb-3">
              curator log{" "}
              <span style={{ color: "var(--color-body)", fontWeight: 300 }}>
                ({reports.length})
              </span>
            </h2>
            {reports.length === 0 ? (
              <p style={{ color: "var(--color-body)" }}>
                The curator hasn't run yet for this agent. It runs every 7
                days when the agent has been idle for at least 2 hours.
              </p>
            ) : (
              <div className="grid gap-4">
                {reports.map((r) => (
                  <details key={r.name} className="card px-4 py-3">
                    <summary className="cursor-pointer">
                      <span className="font-mono text-sm">{r.name}</span>
                      <span
                        className="ml-3 text-xs"
                        style={{ color: "var(--color-body)" }}
                      >
                        {r.size_bytes} bytes
                      </span>
                    </summary>
                    <pre
                      className="mt-3 overflow-x-auto whitespace-pre-wrap text-xs leading-relaxed"
                      style={{
                        background: "rgba(100, 116, 141, 0.05)",
                        padding: "12px",
                        borderRadius: "4px",
                      }}
                    >
                      {r.body}
                    </pre>
                  </details>
                ))}
              </div>
            )}
          </section>
        </>
      )}
    </div>
  );
}

function SourcePill({ source }: { source: SkillView["source"] }) {
  if (source === "bundled") {
    return (
      <span
        className="pill"
        title="ships with havn — immune to the curator"
        style={{
          background: "rgba(100, 116, 141, 0.1)",
          color: "var(--color-body)",
        }}
      >
        bundled
      </span>
    );
  }
  return (
    <span
      className="pill"
      title="installed in this workspace"
      style={{
        background: "rgba(83, 58, 253, 0.1)",
        color: "var(--color-purple)",
      }}
    >
      workspace
    </span>
  );
}

function PinPill() {
  return (
    <span
      className="pill"
      title="pinned — the curator can't archive or merge this skill"
      style={{
        background: "rgba(15, 190, 83, 0.12)",
        color: "var(--color-success-text)",
      }}
    >
      📌 pinned
    </span>
  );
}

function formatDate(iso: string): string {
  try {
    return new Date(iso).toISOString().slice(0, 10);
  } catch {
    return iso;
  }
}
