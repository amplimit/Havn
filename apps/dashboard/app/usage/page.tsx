"use client";

/**
 * `/usage`: what the caller's model calls used (issue #235), by day, agent
 * and model, in tokens, and in US dollars for models with a `price` in
 * `[[llm.models]]`. The gateway's owner can look at anyone's, or everyone's.
 */

import { useEffect, useState } from "react";
import { api, type UsageBucket, type UsageView } from "@/lib/api";

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

function Table({ title, buckets }: { title: string; buckets: UsageBucket[] }) {
  if (buckets.length === 0) return null;
  return (
    <section className="card px-4 py-3">
      <h2 className="mb-2 text-xs uppercase tracking-wider" style={muted}>
        {title}
      </h2>
      <table className="w-full text-sm tabular">
        <tbody>
          {buckets.map((b) => (
            <tr key={b.key}>
              <td className="py-1 pr-3 font-mono wrap-anywhere">{b.label}</td>
              <td className="py-1 pr-3 text-right" style={muted}>
                {b.calls} {b.calls === 1 ? "call" : "calls"}
              </td>
              <td className="py-1 pr-3 text-right">{tokens(b.tokens)}</td>
              <td className="py-1 text-right">
                {dollars(b.cost)}
                {b.unpriced && (
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
  );
}

export default function UsagePage() {
  const [days, setDays] = useState(30);
  const [user, setUser] = useState<string | undefined>(undefined);
  // What was loaded, and for which window and user: other data than the
  // one asked for is not shown as if it were.
  const [loaded, setLoaded] = useState<{ key: string; view: UsageView } | null>(null);
  const [failed, setFailed] = useState<{ key: string; error: string } | null>(null);
  const key = `${days}|${user ?? ""}`;
  const error = failed?.key === key ? failed.error : null;
  const view = loaded?.key === key ? loaded.view : null;
  const everyone = loaded?.view.everyone ?? false;
  const users = loaded?.view.users ?? [];

  useEffect(() => {
    let cancelled = false;
    api
      .usage(days, user)
      .then((v) => {
        if (!cancelled) {
          setLoaded({ key: `${days}|${user ?? ""}`, view: v });
        }
      })
      .catch((e: unknown) => {
        if (!cancelled)
          setFailed({ key: `${days}|${user ?? ""}`, error: e instanceof Error ? e.message : String(e) });
      });
    return () => {
      cancelled = true;
    };
  }, [days, user]);

  return (
    <div className="mx-auto grid max-w-4xl gap-6 px-4 py-8">
      <h1 className="display-h2">usage</h1>
      {error && (
        <p className="card px-4 py-3 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-3 text-sm">
        <span style={muted}>window (UTC days):</span>
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
        {everyone && (
          <select
            className="input ml-auto"
            style={{ maxWidth: 220 }}
            value={user ?? ""}
            onChange={(e) => setUser(e.target.value || undefined)}
            aria-label="whose usage"
          >
            <option value="">mine</option>
            <option value="all">everyone</option>
            {users.map((u) => (
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
              <p className="text-xl tabular">{tokens(view.tokens)}</p>
            </div>
            <div>
              <p className="text-xs uppercase tracking-wider" style={muted}>
                calls
              </p>
              <p className="text-xl tabular">{view.calls}</p>
            </div>
          </section>
          <div className="grid gap-1 text-xs" style={muted}>
            {view.unpriced_tokens > 0 && (
              <p>
                {tokens(view.unpriced_tokens)} tokens are of models without a price, and not in the cost: give a
                model a <span className="font-mono">price</span> in{" "}
                <span className="font-mono">[[llm.models]]</span> in the gateway&apos;s config.toml (dollars per
                million tokens). Costs use the prices set now.
              </p>
            )}
            {view.unsettled_calls > 0 && (
              <p>
                {view.unsettled_calls} {view.unsettled_calls === 1 ? "call is" : "calls are"} still running or got no
                usage from the provider, and {view.unsettled_calls === 1 ? "counts" : "count"} at what was reserved
                for {view.unsettled_calls === 1 ? "it" : "them"}, the most.
              </p>
            )}
            {view.searches > 0 && (
              <p>
                {view.searches} {view.searches === 1 ? "search" : "searches"} through a search backend, not counted
                here.
              </p>
            )}
          </div>
          {view.calls === 0 ? (
            <p className="card px-4 py-6 text-sm" style={muted}>
              No model calls in this window.
            </p>
          ) : (
            <>
              <Table title="by day" buckets={view.by_day} />
              <Table title="by agent" buckets={view.by_agent} />
              <Table title="by model" buckets={view.by_model} />
              <Table title="by user" buckets={view.by_user} />
            </>
          )}
        </>
      )}
    </div>
  );
}
