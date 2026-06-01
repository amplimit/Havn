"use client";

/**
 * `/teams/[id]/audit` — admin-only audit log viewer (spec §10.3).
 *
 * Pagination is `before=<rfc3339>` cursor-style — older pages hang off
 * the oldest visible row. Filters (action prefix, agent, user) compose
 * with AND. Spec §10.3 promises "JSON via API; CSV is jq user-side";
 * we render JSON inline.
 */

import { use, useEffect, useState } from "react";
import { ApiError, api, type AuditEntryView } from "@/lib/api";

const ACTION_PREFIXES = [
  "(all)",
  "agent.",
  "credential.",
  "team.",
  "member.",
  "role.",
  "memory.",
  "skill.",
  "team_credential.",
];

export default function TeamAuditPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const [entries, setEntries] = useState<AuditEntryView[]>([]);
  const [nextBefore, setNextBefore] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [actionFilter, setActionFilter] = useState<string>("(all)");
  // Note: the gateway's filter builder takes optional `action_prefix`,
  // `user_id`, `agent_id`, `before`, `limit`. Rather than rebuilding
  // a typed query layer, we POST the filter via a manual fetch when
  // the prefix changes (the lib client only exposes `(before,limit)`).
  // Promote to `lib/api.ts` when a third caller appears.

  async function loadFirst() {
    setLoading(true);
    try {
      const res = await fetchPage(id, { actionPrefix: prefixOrEmpty(actionFilter) });
      setEntries(res.entries);
      setNextBefore(res.next_before);
      setError(null);
    } catch (e) {
      if (e instanceof ApiError && e.status === 403) {
        setError(
          "You don't have admin permission on this team — audit log is admin-only.",
        );
      } else {
        setError(e instanceof Error ? e.message : String(e));
      }
    } finally {
      setLoading(false);
    }
  }

  async function loadMore() {
    if (!nextBefore || loading) return;
    setLoading(true);
    try {
      const res = await fetchPage(id, {
        actionPrefix: prefixOrEmpty(actionFilter),
        before: nextBefore,
      });
      setEntries((prev) => [...prev, ...res.entries]);
      setNextBefore(res.next_before);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => {
    loadFirst();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id, actionFilter]);

  return (
    <div className="grid gap-4">
      {error && (
        <div
          className="card px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {error}
        </div>
      )}

      <div className="flex items-center gap-2">
        <span className="text-xs uppercase tracking-wider" style={{ color: "var(--color-body)" }}>
          filter:
        </span>
        <select
          value={actionFilter}
          onChange={(e) => setActionFilter(e.target.value)}
          className="bg-transparent text-sm outline-none"
          style={{ borderBottom: "1px solid var(--color-border-default)" }}
        >
          {ACTION_PREFIXES.map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
      </div>

      {entries.length === 0 && !loading ? (
        <p className="card px-4 py-6 text-sm" style={{ color: "var(--color-body)" }}>
          no entries.
        </p>
      ) : (
        <ul className="grid gap-2">
          {entries.map((e) => (
            <li key={e.id} className="card px-4 py-3">
              <div className="flex items-center justify-between gap-3">
                <div>
                  <span
                    className="font-mono text-sm"
                    style={{ color: "var(--color-purple)" }}
                  >
                    {e.action}
                  </span>
                  <span
                    className="ml-3 text-xs"
                    style={{ color: "var(--color-body)" }}
                  >
                    by {e.user_id.slice(0, 8)}…
                    {e.agent_id && ` · agent ${e.agent_id.slice(0, 8)}…`}
                  </span>
                </div>
                <span
                  className="text-xs font-mono"
                  style={{ color: "var(--color-body)" }}
                >
                  {new Date(e.created_at).toUTCString().slice(5, 25)}
                </span>
              </div>
              {Object.keys(e.details).length > 0 && (
                <details className="mt-2">
                  <summary
                    className="cursor-pointer text-xs"
                    style={{ color: "var(--color-body)" }}
                  >
                    details
                  </summary>
                  <pre
                    className="mt-1 overflow-x-auto text-xs leading-relaxed"
                    style={{
                      background: "rgba(100, 116, 141, 0.05)",
                      padding: "8px",
                      borderRadius: "4px",
                    }}
                  >
                    {JSON.stringify(e.details, null, 2)}
                  </pre>
                </details>
              )}
            </li>
          ))}
        </ul>
      )}

      {nextBefore && (
        <button
          type="button"
          onClick={loadMore}
          disabled={loading}
          className="btn-ghost mx-auto"
        >
          {loading ? "loading…" : "load older"}
        </button>
      )}
    </div>
  );
}

function prefixOrEmpty(p: string): string | undefined {
  return p === "(all)" ? undefined : p;
}

async function fetchPage(
  teamId: string,
  opts: { actionPrefix?: string; before?: string },
): Promise<{ entries: AuditEntryView[]; next_before: string | null }> {
  const q = new URLSearchParams();
  if (opts.actionPrefix) q.set("action_prefix", opts.actionPrefix);
  if (opts.before) q.set("before", opts.before);
  q.set("limit", "100");
  const res = await fetch(`/api/teams/${teamId}/audit-log?${q.toString()}`, {
    cache: "no-store",
  });
  if (!res.ok) {
    throw new ApiError(res.status, await res.text());
  }
  return res.json();
}
