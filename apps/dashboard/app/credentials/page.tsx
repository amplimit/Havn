"use client";

import { useEffect, useState } from "react";
import { api, type CredentialView } from "@/lib/api";

export default function CredentialsPage() {
  const [creds, setCreds] = useState<CredentialView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Form state.
  const [provider, setProvider] = useState("anthropic");
  const [apiKey, setApiKey] = useState("");
  const [priority, setPriority] = useState(10);
  const [maxTokensPerDay, setMaxTokensPerDay] = useState<string>(""); // empty = unlimited

  async function reload() {
    try {
      setCreds(await api.credentials.list());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    reload();
  }, []);

  async function onAdd(e: React.FormEvent) {
    e.preventDefault();
    const limits: Record<string, unknown> = {};
    if (maxTokensPerDay.trim()) {
      const n = Number(maxTokensPerDay);
      if (!Number.isInteger(n) || n <= 0) {
        setError("max tokens / day must be a whole number above 0, or blank for unlimited");
        return;
      }
      limits.max_tokens_per_day = n;
    }
    setBusy(true);
    setError(null);
    try {
      await api.credentials.create(provider.trim(), apiKey.trim(), priority, limits);
      setApiKey("");
      setMaxTokensPerDay("");
      await reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function onDelete(id: string) {
    if (!confirm("Delete this credential? Calls relying on it will fall back to the next priority.")) return;
    try {
      await api.credentials.delete(id);
      await reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <div className="mx-auto max-w-4xl px-8 py-12">
      <header className="mb-8">
        <h1 className="display-h1">credentials</h1>
        <p className="mt-2" style={{ color: "var(--color-body)", fontSize: 18 }}>
          LLM provider keys, with optional daily token caps. The gateway proxies
          every model call through these — agent processes never see the raw
          keys (spec §7).
        </p>
        <div
          className="mt-3 inline-flex items-center gap-2 rounded px-3 py-1.5 text-sm"
          style={{
            background: "rgba(83, 58, 253, 0.06)",
            color: "var(--color-stripe-purple)",
            border: "1px solid rgba(83, 58, 253, 0.18)",
          }}
          title="Stored as age-encrypted ciphertext (spec §13). The gateway needs HAVN_AGE_KEY in its environment to decrypt at use time; a stolen DB file alone is useless without the key."
        >
          🔐 keys are encrypted at rest with age (HAVN_AGE_KEY)
        </div>
      </header>

      {error && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{ color: "var(--color-ruby)", borderColor: "rgba(234, 34, 97, 0.3)" }}
        >
          {error}
        </div>
      )}

      <div className="card-elevated mb-8 p-6">
        <h2 className="display-h3 mb-4">add a key</h2>
        <form onSubmit={onAdd} className="grid grid-cols-1 gap-4 md:grid-cols-4">
          <div className="md:col-span-1">
            <label className="label" htmlFor="provider">provider</label>
            <select
              id="provider"
              value={provider}
              onChange={(e) => setProvider(e.target.value)}
              className="input"
            >
              <option value="anthropic">anthropic</option>
              <option value="openai">openai</option>
              <option value="openrouter">openrouter</option>
            </select>
          </div>
          <div className="md:col-span-2">
            <label className="label" htmlFor="apikey">api key</label>
            <input
              id="apikey"
              type="password"
              required
              autoComplete="off"
              spellCheck={false}
              placeholder="sk-…"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
              className="input font-mono"
            />
          </div>
          <div>
            <label className="label" htmlFor="priority">priority</label>
            <input
              id="priority"
              type="number"
              min={0}
              max={1000}
              value={priority}
              onChange={(e) => setPriority(Number(e.target.value))}
              className="input tabular"
            />
          </div>
          <div className="md:col-span-2">
            <label className="label" htmlFor="cap">max tokens / day (blank = unlimited)</label>
            <input
              id="cap"
              type="number"
              min={1}
              step={1}
              placeholder="1000000"
              value={maxTokensPerDay}
              onChange={(e) => setMaxTokensPerDay(e.target.value)}
              className="input tabular"
            />
            <p className="mt-1.5 text-xs" style={{ color: "var(--color-body)" }}>
              gateway falls through to the next-priority credential when this is exhausted (spec §7.4).
            </p>
          </div>
          <div className="flex items-end justify-end md:col-span-2">
            <button
              type="submit"
              className="btn-primary"
              disabled={busy || !apiKey.trim()}
            >
              {busy ? "saving…" : "add credential"}
            </button>
          </div>
        </form>
      </div>

      <h2 className="display-h2 mb-4">configured ({creds?.length ?? "…"})</h2>

      {creds === null && !error && (
        <div className="card px-4 py-6 text-sm" style={{ color: "var(--color-body)" }}>
          loading…
        </div>
      )}

      {creds?.length === 0 && (
        <div
          className="card px-5 py-10 text-center"
          style={{ color: "var(--color-body)" }}
        >
          no credentials yet — add one above so agents can call models.
        </div>
      )}

      <ul className="grid gap-3">
        {creds?.map((c) => {
          // Only `max_tokens_per_day` is enforced; a `max_usd_per_day` left
          // by older dashboards is ignored by the gateway.
          const raw = c.limits?.max_tokens_per_day;
          const cap = typeof raw === "number" && raw > 0 ? raw : null;
          const usdCap = c.limits?.max_usd_per_day !== undefined;
          return (
            <li key={c.id} className="card flex items-center justify-between px-5 py-4">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-3">
                  <span className="display-h3">{c.provider}</span>
                  <span className="pill pill-stopped tabular">priority {c.priority}</span>
                  {cap ? (
                    <span className="pill pill-running tabular">
                      cap {cap.toLocaleString()} tokens/day
                    </span>
                  ) : (
                    <span className="pill pill-stopped">unlimited</span>
                  )}
                  {!c.enabled && <span className="pill pill-error">disabled</span>}
                </div>
                <p
                  className="mt-1 font-mono text-xs"
                  style={{ color: "var(--color-body)" }}
                >
                  {c.id}
                </p>
                {usdCap && (
                  <p className="mt-1 text-xs" style={{ color: "var(--color-ruby)" }}>
                    this key has a USD/day cap, which the gateway does not enforce; delete and re-add it with a token cap
                  </p>
                )}
              </div>
              <button
                type="button"
                className="btn-danger"
                onClick={() => onDelete(c.id)}
              >
                delete
              </button>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
