"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { Globe, Lock } from "lucide-react";
import {
  api,
  isSearchCredential,
  LLM_PROVIDERS,
  SEARCH_PROVIDERS,
  type CredentialView,
  type SearchStatus,
} from "@/lib/api";
import { SkeletonList } from "@/components/SkeletonList";

export default function CredentialsPage() {
  const [creds, setCreds] = useState<CredentialView[] | null>(null);
  // Null until loaded, and when the gateway predates GET /search.
  const [search, setSearch] = useState<SearchStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Form state.
  const [provider, setProvider] = useState("anthropic");
  const [apiKey, setApiKey] = useState("");
  const [priority, setPriority] = useState(10);
  const [maxTokensPerDay, setMaxTokensPerDay] = useState<string>(""); // empty = unlimited
  const searchKey = isSearchCredential(provider);

  async function reload() {
    try {
      setCreds(await api.credentials.list());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
    setSearch(await api.search.get().catch(() => null));
  }

  useEffect(() => {
    let cancelled = false;
    api.credentials.list().then(
      (c) => {
        if (cancelled) return;
        setCreds(c);
        setError(null);
      },
      (e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    api.search.get().then(
      (s) => {
        if (!cancelled) setSearch(s);
      },
      () => {},
    );
    return () => {
      cancelled = true;
    };
  }, []);

  async function onAdd(e: React.FormEvent) {
    e.preventDefault();
    const limits: Record<string, unknown> = {};
    // Token caps apply to model calls only.
    if (!searchKey && maxTokensPerDay.trim()) {
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
    <div className="mx-auto max-w-4xl px-4 py-8 sm:px-8 sm:py-12">
      <header className="mb-8">
        <h1 className="display-h1">credentials</h1>
        <p className="mt-2" style={{ color: "var(--color-body)", fontSize: 18 }}>
          LLM provider keys, with optional daily token caps, and web search
          keys. The gateway makes every model call and search with these;
          agents never see the keys.
        </p>
        <p className="mt-2 max-w-3xl text-sm" style={{ color: "var(--color-body)" }}>
          Keys here are yours: only your agents use them. To share a key, a
          team admin adds it on the team&apos;s credentials page under{" "}
          <Link href="/teams" className="underline" style={{ color: "var(--color-stripe-purple)" }}>
            teams
          </Link>
          ; then every member&apos;s personal agents, and the team&apos;s agents,
          can use it.
        </p>
        <div
          className="mt-3 inline-flex items-center gap-2 rounded px-3 py-1.5 text-sm"
          style={{
            background: "rgba(83, 58, 253, 0.06)",
            color: "var(--color-stripe-purple)",
            border: "1px solid rgba(83, 58, 253, 0.18)",
          }}
          title="Stored as age-encrypted ciphertext. The gateway needs HAVN_AGE_KEY in its environment to decrypt at use time; a stolen DB file alone is useless without the key."
        >
          <Lock size={14} className="shrink-0" aria-hidden="true" />
          keys are encrypted at rest with age (HAVN_AGE_KEY)
        </div>
      </header>

      {search && <SearchNote status={search} />}

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
              <optgroup label="LLM">
                {LLM_PROVIDERS.map((p) => (
                  <option key={p} value={p}>
                    {p}
                  </option>
                ))}
              </optgroup>
              <optgroup label="web search">
                {SEARCH_PROVIDERS.map((p) => (
                  <option key={p} value={`search:${p}`}>
                    {p}
                  </option>
                ))}
              </optgroup>
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
              placeholder={searchKey ? "search API key" : "sk-…"}
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
          {!searchKey && (
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
              when this is used up, the gateway falls through to the next credential by priority.
            </p>
          </div>
          )}
          <div className={`flex items-end justify-end ${searchKey ? "md:col-span-4" : "md:col-span-2"}`}>
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
        <SkeletonList />
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
            <li key={c.id} className="card flex flex-wrap items-center justify-between gap-3 px-5 py-4">
              <div className="min-w-[12rem] flex-1">
                <div className="flex items-center gap-3">
                  <span className="display-h3">{c.provider}</span>
                  <span className="pill pill-stopped tabular">priority {c.priority}</span>
                  {isSearchCredential(c.provider) ? (
                    <span className="pill pill-stopped">web search</span>
                  ) : cap ? (
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

// Whether agents can search the web, and how to make them.
function SearchNote({ status }: { status: SearchStatus }) {
  let text: React.ReactNode;
  if (status.backend === "openrouter") {
    text = (
      <>
        Web search: your agents search through your OpenRouter key (Exa, about
        $0.007 per search). A Brave, Tavily or Exa key, added below as a web
        search key, takes precedence.
      </>
    );
  } else if (status.backend) {
    text = <>Web search: your agents search with {status.backend}.</>;
  } else if (status.provider === "off") {
    text = <>Web search is turned off in the gateway&apos;s config ([search] provider).</>;
  } else {
    text = (
      <>
        Web search is not set up: your agents have no web_search tool. Add a
        Brave, Tavily or Exa key below (each has a free tier), or an OpenRouter
        key{status.provider === "auto" ? ", or have the operator set [search] searxng_url" : ""}.
        Agents started before you add one get the tool on their next start.
      </>
    );
  }
  const ready = status.backend !== null;
  return (
    <div
      className="card mb-6 flex items-start gap-3 px-4 py-3 text-sm"
      style={{ color: "var(--color-body)" }}
      role="status"
    >
      <Globe
        size={16}
        className="mt-0.5 shrink-0"
        style={{ color: ready ? "var(--color-success-text)" : "var(--color-warning-text)" }}
        aria-hidden="true"
      />
      <p>{text}</p>
    </div>
  );
}
