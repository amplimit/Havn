"use client";

/**
 * `/teams/[id]/credentials` — team-shared LLM credentials with
 * per-user caps (spec §7, §10.3 admin view).
 *
 * Members see the list (so they understand what fallback keys exist).
 * Admins create / update / delete and configure the per-user caps that
 * stop one heavy user from draining the daily quota.
 *
 * The form lets admins set both:
 *   - `max_tokens_per_day` — credential-wide
 *   - `per_user.max_tokens_per_day` / `per_user.max_requests_per_minute`
 * — both in one panel rather than nested JSON, since per-user clamps
 * are the single most-asked-for use case for team credentials.
 */

import { use, useEffect, useState } from "react";
import { ApiError, api, type CredentialView, type TeamView } from "@/lib/api";

export default function TeamCredentialsPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const [team, setTeam] = useState<TeamView | null>(null);
  const [creds, setCreds] = useState<CredentialView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [provider, setProvider] = useState("anthropic");
  const [apiKey, setApiKey] = useState("");
  const [priority, setPriority] = useState(10);
  const [maxTokensPerDay, setMaxTokensPerDay] = useState<string>("");
  const [perUserTokens, setPerUserTokens] = useState<string>("");
  const [perUserRpm, setPerUserRpm] = useState<string>("");

  async function load() {
    try {
      const [t, c] = await Promise.all([
        api.teams.get(id),
        api.teams.credentials.list(id),
      ]);
      setTeam(t);
      setCreds(c);
      setError(null);
    } catch (e) {
      if (e instanceof ApiError && e.status === 403) {
        setError("Members only — you're not in this team.");
      } else {
        setError(e instanceof Error ? e.message : String(e));
      }
    }
  }

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  async function onCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!apiKey.trim() || !provider.trim()) return;
    setAdding(true);
    try {
      const limits: Record<string, unknown> = {};
      if (maxTokensPerDay) limits.max_tokens_per_day = Number(maxTokensPerDay);
      if (perUserTokens || perUserRpm) {
        const per_user: Record<string, unknown> = {};
        if (perUserTokens) per_user.max_tokens_per_day = Number(perUserTokens);
        if (perUserRpm) per_user.max_requests_per_minute = Number(perUserRpm);
        limits.per_user = per_user;
      }
      await api.teams.credentials.create(
        id,
        provider.trim(),
        apiKey.trim(),
        priority,
        limits,
      );
      setApiKey("");
      setMaxTokensPerDay("");
      setPerUserTokens("");
      setPerUserRpm("");
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setAdding(false);
    }
  }

  async function onToggle(c: CredentialView) {
    try {
      await api.teams.credentials.patch(id, c.id, { enabled: !c.enabled });
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function onDelete(c: CredentialView) {
    if (!confirm(`Delete this ${c.provider} credential? Members lose this fallback immediately.`)) {
      return;
    }
    try {
      await api.teams.credentials.delete(id, c.id);
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

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

      <div
        className="inline-flex items-center gap-2 self-start rounded px-3 py-1.5 text-sm"
        style={{
          background: "rgba(83, 58, 253, 0.06)",
          color: "var(--color-stripe-purple)",
          border: "1px solid rgba(83, 58, 253, 0.18)",
        }}
        title="Stored as age-encrypted ciphertext (spec §13). The gateway needs HAVN_AGE_KEY in its environment to decrypt at use time."
      >
        🔐 keys are encrypted at rest with age (HAVN_AGE_KEY)
      </div>

      {team?.is_admin && (
        <section className="card px-4 py-4">
          <h2 className="display-h3 mb-3">add team credential</h2>
          <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
            Falls back AFTER each member's personal credentials per spec
            §7.2. Per-user caps clamp one user's burn so they don't
            exhaust the team's daily quota.
          </p>
          <form onSubmit={onCreate} className="grid gap-3">
            <div className="grid gap-3 sm:grid-cols-3">
              <FormField
                label="provider"
                value={provider}
                onChange={setProvider}
                placeholder="anthropic"
              />
              <FormField
                label="api key"
                value={apiKey}
                onChange={setApiKey}
                placeholder="sk-..."
                type="password"
              />
              <FormField
                label="priority"
                value={String(priority)}
                onChange={(v) => setPriority(Number(v) || 0)}
                placeholder="10"
                type="number"
              />
            </div>
            <details>
              <summary
                className="cursor-pointer text-sm"
                style={{ color: "var(--color-body)" }}
              >
                limits (optional)
              </summary>
              <div className="mt-3 grid gap-3 sm:grid-cols-3">
                <FormField
                  label="max_tokens_per_day"
                  value={maxTokensPerDay}
                  onChange={setMaxTokensPerDay}
                  placeholder="0 = unlimited"
                  type="number"
                />
                <FormField
                  label="per_user max tokens / day"
                  value={perUserTokens}
                  onChange={setPerUserTokens}
                  placeholder="0 = unlimited"
                  type="number"
                />
                <FormField
                  label="per_user max requests / minute"
                  value={perUserRpm}
                  onChange={setPerUserRpm}
                  placeholder="0 = unlimited"
                  type="number"
                />
              </div>
            </details>
            <div>
              <button type="submit" className="btn-primary" disabled={adding}>
                {adding ? "creating…" : "create credential"}
              </button>
            </div>
          </form>
        </section>
      )}

      <section>
        <h2 className="display-h3 mb-3">team credentials</h2>
        {creds === null ? (
          <p style={{ color: "var(--color-body)" }}>loading…</p>
        ) : creds.length === 0 ? (
          <p style={{ color: "var(--color-body)" }}>(none)</p>
        ) : (
          <ul className="grid gap-2">
            {creds.map((c) => (
              <li
                key={c.id}
                className="card px-4 py-3"
                style={{ opacity: c.enabled ? 1 : 0.55 }}
              >
                <div className="flex items-start justify-between gap-3">
                  <div>
                    <p>
                      <span className="display-h3">{c.provider}</span>
                      <span
                        className="ml-3 text-xs"
                        style={{ color: "var(--color-body)" }}
                      >
                        priority {c.priority}
                      </span>
                    </p>
                    <p
                      className="font-mono text-xs"
                      style={{ color: "var(--color-body)" }}
                    >
                      {c.id}
                    </p>
                    <details className="mt-1">
                      <summary
                        className="cursor-pointer text-xs"
                        style={{ color: "var(--color-body)" }}
                      >
                        limits
                      </summary>
                      <pre
                        className="mt-1 overflow-x-auto text-xs"
                        style={{
                          background: "rgba(100, 116, 141, 0.05)",
                          padding: "8px",
                          borderRadius: "4px",
                        }}
                      >
                        {JSON.stringify(c.limits, null, 2)}
                      </pre>
                    </details>
                  </div>
                  {team?.is_admin && (
                    <div className="flex shrink-0 items-center gap-3 text-sm">
                      <button
                        type="button"
                        onClick={() => onToggle(c)}
                        className="underline"
                        style={{ color: "var(--color-body)" }}
                      >
                        {c.enabled ? "disable" : "enable"}
                      </button>
                      <button
                        type="button"
                        onClick={() => onDelete(c)}
                        className="underline"
                        style={{ color: "var(--color-ruby)" }}
                      >
                        delete
                      </button>
                    </div>
                  )}
                </div>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}

function FormField({
  label,
  value,
  onChange,
  placeholder,
  type = "text",
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  placeholder?: string;
  type?: string;
}) {
  return (
    <label className="flex flex-col">
      <span
        className="mb-1 text-xs uppercase tracking-wider"
        style={{ color: "var(--color-body)" }}
      >
        {label}
      </span>
      <input
        type={type}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className="bg-transparent text-sm outline-none"
        style={{ borderBottom: "1px solid var(--color-border-default)" }}
      />
    </label>
  );
}
