"use client";

/**
 * `/usage`: what the caller's model calls used (issue #235), by day, agent
 * and model, in tokens, and in US dollars for models with a `price` in
 * `[[llm.models]]`. The gateway's owner can look at anyone's, or everyone's.
 */

import { useEffect, useMemo, useState } from "react";
import { api, type UsageRow, type UsageView } from "@/lib/api";

const WINDOWS: Array<{ days: number; label: string }> = [
  { days: 7, label: "7d" },
  { days: 30, label: "30d" },
  { days: 90, label: "90d" },
  { days: 365, label: "1y" },
];

const muted = { color: "var(--color-body)" };

function tokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

function dollars(n: number): string {
  return n >= 1 ? `$${n.toFixed(2)}` : `$${n.toFixed(4)}`;
}

/** Rows summed by `key`, the most costly (then most used) first. */
function byKey(rows: UsageRow[], key: (r: UsageRow) => string) {
  const out = new Map<string, { calls: number; tokens: number; cost: number; unpriced: boolean }>();
  for (const r of rows) {
    const k = key(r);
    const sum = out.get(k) ?? { calls: 0, tokens: 0, cost: 0, unpriced: false };
    sum.calls += r.calls;
    sum.tokens += r.tokens_in + r.tokens_out;
    if (r.cost === null) sum.unpriced = true;
    else sum.cost += r.cost;
    out.set(k, sum);
  }
  return [...out.entries()].sort((a, b) => b[1].cost - a[1].cost || b[1].tokens - a[1].tokens);
}

export default function UsagePage() {
  const [days, setDays] = useState(30);
  const [user, setUser] = useState<string | undefined>(undefined);
  const [view, setView] = useState<UsageView | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    api
      .usage(days, user)
      .then((v) => {
        if (!cancelled) {
          setView(v);
          setError(null);
        }
      })
      .catch((e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      });
    return () => {
      cancelled = true;
    };
  }, [days, user]);

  const rows = useMemo(() => view?.rows ?? [], [view]);
  const tables: Array<[string, Array<[string, { calls: number; tokens: number; cost: number; unpriced: boolean }]>]> =
    useMemo(
      () => [
        ["by day", byKey(rows, (r) => r.day).sort((a, b) => (a[0] < b[0] ? 1 : -1))],
        ["by agent", byKey(rows, (r) => r.agent_name ?? "(no agent)")],
        ["by model", byKey(rows, (r) => r.model)],
        ...(view?.user === "all"
          ? [["by user", byKey(rows, (r) => r.display_name)] as [string, ReturnType<typeof byKey>]]
          : []),
      ],
      [rows, view?.user],
    );
  const calls = rows.reduce((n, r) => n + r.calls, 0);
  const used = rows.reduce((n, r) => n + r.tokens_in + r.tokens_out, 0);

  return (
    <div className="grid gap-6">
      <h1 className="display-h2">usage</h1>
      {error && (
        <p className="card px-4 py-3 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-3 text-sm">
        <span style={muted}>window:</span>
        {WINDOWS.map((w) => (
          <button
            key={w.days}
            type="button"
            onClick={() => setDays(w.days)}
            style={{
              color: w.days === days ? "var(--color-purple)" : "var(--color-body)",
              textDecoration: w.days === days ? "underline" : "none",
            }}
          >
            {w.label}
          </button>
        ))}
        {view?.everyone && (
          <select
            className="input ml-auto"
            style={{ maxWidth: 220 }}
            value={user ?? ""}
            onChange={(e) => setUser(e.target.value || undefined)}
            aria-label="whose usage"
          >
            <option value="">mine</option>
            <option value="all">everyone</option>
            {view.users.map((u) => (
              <option key={u.user_id} value={u.user_id}>
                {u.display_name}
              </option>
            ))}
          </select>
        )}
      </div>
      {view === null ? (
        !error && (
          <p className="card px-4 py-6 text-sm" style={muted}>
            loading…
          </p>
        )
      ) : (
        <>
          <section className="card-elevated grid grid-cols-3 gap-4 px-4 py-3">
            <div>
              <p className="text-xs uppercase tracking-wider" style={muted}>
                cost
              </p>
              <p className="text-xl tabular">{dollars(view.cost)}</p>
            </div>
            <div>
              <p className="text-xs uppercase tracking-wider" style={muted}>
                tokens
              </p>
              <p className="text-xl tabular">{tokens(used)}</p>
            </div>
            <div>
              <p className="text-xs uppercase tracking-wider" style={muted}>
                calls
              </p>
              <p className="text-xl tabular">{calls}</p>
            </div>
          </section>
          {view.unpriced_tokens > 0 && (
            <p className="text-xs" style={muted}>
              {tokens(view.unpriced_tokens)} tokens are of models without a price, and not in the cost: give a model
              a <span className="font-mono">price</span> in <span className="font-mono">[[llm.models]]</span> in the
              gateway&apos;s config.toml (dollars per million tokens). Costs use the prices set now.
            </p>
          )}
          {rows.length === 0 ? (
            <p className="card px-4 py-6 text-sm" style={muted}>
              No model calls in this window.
            </p>
          ) : (
            tables.map(([title, entries]) => (
              <section key={title} className="card px-4 py-3">
                <h2 className="mb-2 text-xs uppercase tracking-wider" style={muted}>
                  {title}
                </h2>
                <table className="w-full text-sm tabular">
                  <tbody>
                    {entries.map(([name, sum]) => (
                      <tr key={name}>
                        <td className="py-1 pr-3 font-mono wrap-anywhere">{name}</td>
                        <td className="py-1 pr-3 text-right" style={muted}>
                          {sum.calls} {sum.calls === 1 ? "call" : "calls"}
                        </td>
                        <td className="py-1 pr-3 text-right">{tokens(sum.tokens)}</td>
                        <td className="py-1 text-right">
                          {dollars(sum.cost)}
                          {sum.unpriced && (
                            <span style={muted} title="some of it is of a model without a price">
                              {" "}
                              +
                            </span>
                          )}
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </section>
            ))
          )}
        </>
      )}
    </div>
  );
}
