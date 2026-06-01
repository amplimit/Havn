"use client";

/**
 * `/teams/[id]/usage` — token totals per member (spec §10.3
 * "Resource Dashboard"). Spec §7.3 / v0.6: token counts only, no
 * USD. Operators compute $ in their own analytics from this data.
 *
 * Default window 30 days; the picker also offers 7 / 90 / 365.
 */

import { use, useEffect, useState } from "react";
import { api, type TeamUsageEntry } from "@/lib/api";

const WINDOWS: Array<{ days: number; label: string }> = [
  { days: 7, label: "7d" },
  { days: 30, label: "30d" },
  { days: 90, label: "90d" },
  { days: 365, label: "1y" },
];

export default function TeamUsagePage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const [days, setDays] = useState(30);
  const [entries, setEntries] = useState<TeamUsageEntry[] | null>(null);
  const [since, setSince] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const r = await api.teams.usage(id, days);
        if (!cancelled) {
          setEntries(r.entries);
          setSince(r.since);
        }
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id, days]);

  const total = (entries ?? []).reduce(
    (acc, e) => ({
      tokens_in: acc.tokens_in + e.tokens_in,
      tokens_out: acc.tokens_out + e.tokens_out,
      call_count: acc.call_count + e.call_count,
    }),
    { tokens_in: 0, tokens_out: 0, call_count: 0 },
  );

  return (
    <div className="grid gap-6">
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

      <div className="flex items-center gap-3">
        <span className="text-sm" style={{ color: "var(--color-body)" }}>
          window:
        </span>
        {WINDOWS.map((w) => (
          <button
            key={w.days}
            type="button"
            onClick={() => setDays(w.days)}
            className="text-sm"
            style={{
              color:
                w.days === days
                  ? "var(--color-purple)"
                  : "var(--color-body)",
              textDecoration: w.days === days ? "underline" : "none",
            }}
          >
            {w.label}
          </button>
        ))}
        {since && (
          <span
            className="ml-auto text-xs font-mono"
            style={{ color: "var(--color-body)" }}
          >
            since {new Date(since).toISOString().slice(0, 10)}
          </span>
        )}
      </div>

      {entries === null ? (
        <p
          className="card px-4 py-6 text-sm"
          style={{ color: "var(--color-body)" }}
        >
          loading…
        </p>
      ) : (
        <>
          <section className="card-elevated px-4 py-3">
            <p
              className="text-xs uppercase tracking-wider"
              style={{ color: "var(--color-body)" }}
            >
              team total
            </p>
            <p className="mt-1 text-lg tabular">
              {fmtNum(total.tokens_in + total.tokens_out)} tokens
              <span
                className="ml-3 text-sm"
                style={{ color: "var(--color-body)" }}
              >
                ({fmtNum(total.tokens_in)} in / {fmtNum(total.tokens_out)} out){" "}
                across {fmtNum(total.call_count)} calls
              </span>
            </p>
          </section>

          <section>
            <h2 className="display-h3 mb-3">per member</h2>
            {entries.length === 0 ? (
              <p style={{ color: "var(--color-body)" }}>
                No LLM calls in this window.
              </p>
            ) : (
              <table className="w-full text-sm">
                <thead>
                  <tr
                    className="border-b text-left text-xs uppercase tracking-wider"
                    style={{
                      borderColor: "var(--color-border-default)",
                      color: "var(--color-body)",
                    }}
                  >
                    <th className="py-2">user</th>
                    <th className="py-2 text-right">in</th>
                    <th className="py-2 text-right">out</th>
                    <th className="py-2 text-right">total</th>
                    <th className="py-2 text-right">calls</th>
                  </tr>
                </thead>
                <tbody>
                  {entries.map((e) => (
                    <tr
                      key={e.user_id}
                      className="border-b"
                      style={{ borderColor: "var(--color-border-default)" }}
                    >
                      <td className="py-2">{e.display_name}</td>
                      <td className="py-2 text-right tabular">
                        {fmtNum(e.tokens_in)}
                      </td>
                      <td className="py-2 text-right tabular">
                        {fmtNum(e.tokens_out)}
                      </td>
                      <td className="py-2 text-right tabular">
                        {fmtNum(e.tokens_in + e.tokens_out)}
                      </td>
                      <td className="py-2 text-right tabular">
                        {fmtNum(e.call_count)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
          </section>
        </>
      )}
    </div>
  );
}

function fmtNum(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return n.toString();
}
