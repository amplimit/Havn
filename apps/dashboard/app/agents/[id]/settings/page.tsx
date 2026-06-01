"use client";

/**
 * `/agents/[id]/settings` — per-agent configuration.
 *
 * Editable today: name, model, heartbeat interval. The PATCH endpoint
 * shallow-merges config keys, so saving "model" doesn't disturb
 * "heartbeat" or any per-agent policy override the user set elsewhere.
 *
 * Changes apply on the agent's NEXT spawn. Running runtime keeps its
 * frozen system prompt + Welcome-snapshot policy (spec §9.4 frozen-
 * prompt invariant). The page surfaces a small notice when the agent
 * is currently running so the user knows to expect that.
 */

import { use, useEffect, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";
import {
  ApiError,
  api,
  type AgentView,
  type BootstrapView,
  type EmbeddingStatus,
  type McpServerConfig,
  type McpView,
} from "@/lib/api";

// Latest Anthropic models as of 2026-05; user can also type a custom
// id if they want something not on this list (a model the gateway's
// allowed_models policy permits).
const MODEL_PRESETS = [
  { id: "claude-opus-4-7", label: "Opus 4.7 — best reasoning, ~5× cost" },
  { id: "claude-opus-4-6", label: "Opus 4.6 — prior generation Opus" },
  { id: "claude-sonnet-4-6", label: "Sonnet 4.6 — fast + cheap default" },
  { id: "claude-haiku-4-5-20251001", label: "Haiku 4.5 — cheapest" },
];

export default function SettingsPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const router = useRouter();

  const [agent, setAgent] = useState<AgentView | null>(null);
  const [embedding, setEmbedding] = useState<EmbeddingStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);

  // Form state, seeded from the agent on load. Local-only until Save.
  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [heartbeatMin, setHeartbeatMin] = useState<string>("");

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const [a, e] = await Promise.all([api.agents.get(id), api.embedding()]);
        if (cancelled) return;
        setAgent(a);
        setEmbedding(e);
        setName(a.name);
        const cfg = a.config as Record<string, unknown>;
        setModel(typeof cfg.model === "string" ? cfg.model : "");
        const hb =
          (cfg.heartbeat as Record<string, unknown> | undefined) ?? {};
        const secs = typeof hb.interval_seconds === "number"
          ? hb.interval_seconds
          : null;
        setHeartbeatMin(secs ? String(Math.round(secs / 60)) : "");
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

  async function onSave(e: React.FormEvent) {
    e.preventDefault();
    if (!agent) return;
    if (!name.trim()) {
      setError("Name can't be empty.");
      return;
    }
    setBusy(true);
    setError(null);
    try {
      // Build a minimal config patch — shallow-merged server-side, so
      // keys we don't touch stay untouched.
      const config: Record<string, unknown> = {};
      if (model.trim()) {
        config.model = model.trim();
      } else {
        // null clears the key — runtime will fall back to its compiled
        // default (currently claude-opus-4-6).
        config.model = null;
      }
      const hbNum = Number(heartbeatMin);
      if (heartbeatMin.trim() && Number.isFinite(hbNum) && hbNum > 0) {
        config.heartbeat = { interval_seconds: Math.round(hbNum * 60) };
      } else if (!heartbeatMin.trim()) {
        config.heartbeat = null;
      }
      const updated = await api.agents.patch(id, {
        name: name.trim() === agent.name ? undefined : name.trim(),
        config,
      });
      setAgent(updated);
      setSavedAt(Date.now());
    } catch (e) {
      if (e instanceof ApiError) setError(`${e.status}: ${e.body}`);
      else setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function onDelete() {
    if (!agent) return;
    if (
      !confirm(
        `Delete "${agent.name}"? Workspace, conversations, memory, and skills are all removed. Unrecoverable.`,
      )
    )
      return;
    try {
      await api.agents.delete(id);
      router.push("/");
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  if (error && !agent) {
    return (
      <div className="mx-auto max-w-3xl px-8 py-12">
        <div
          className="card px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {error}
        </div>
      </div>
    );
  }
  if (!agent) {
    return (
      <div className="px-8 py-12" style={{ color: "var(--color-body)" }}>
        loading…
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-3xl px-8 py-10">
      <header className="mb-6">
        <Link
          href={`/agents/${id}`}
          className="mb-2 inline-block text-sm"
          style={{ color: "var(--color-body)" }}
        >
          ← back to chat
        </Link>
        <h1 className="display-h1">{agent.name} — settings</h1>
        <p
          className="mt-2 text-sm"
          style={{ color: "var(--color-body)" }}
        >
          Changes apply on the agent's next start. The currently-running
          runtime keeps its frozen system prompt + Welcome-snapshot
          policy until it's restarted (spec §9.4).
        </p>
      </header>

      {error && (
        <div
          className="card mb-4 px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {error}
        </div>
      )}

      <form onSubmit={onSave} className="grid gap-6">
        <Field
          label="Name"
          hint="Shown in the sidebar and conversation header."
        >
          <input
            type="text"
            value={name}
            onChange={(e) => setName(e.target.value)}
            className="input"
            placeholder="e.g. dev assistant"
            required
          />
        </Field>

        <Field
          label="Model"
          hint="Anthropic model id used for every LLM call. Pick a preset or paste any id allowed by your role's policy."
        >
          <input
            type="text"
            value={model}
            onChange={(e) => setModel(e.target.value)}
            list="model-presets"
            className="input font-mono"
            placeholder="claude-sonnet-4-6"
          />
          <datalist id="model-presets">
            {MODEL_PRESETS.map((m) => (
              <option key={m.id} value={m.id}>
                {m.label}
              </option>
            ))}
          </datalist>
          <div className="mt-2 grid gap-1">
            {MODEL_PRESETS.map((m) => (
              <button
                key={m.id}
                type="button"
                onClick={() => setModel(m.id)}
                className="text-left text-xs"
                style={{
                  color:
                    model === m.id
                      ? "var(--color-stripe-purple)"
                      : "var(--color-body)",
                  textDecoration: model === m.id ? "underline" : "none",
                }}
              >
                <code className="font-mono">{m.id}</code>
                <span className="ml-2">{m.label.replace(/^[^—]+— /, "")}</span>
              </button>
            ))}
          </div>
        </Field>

        <Field
          label="Heartbeat interval (minutes)"
          hint="How often the agent's heartbeat tick fires (spec §9.6 — proactive self-tick reading HEARTBEAT.md). Empty = use the default 30 min. Lower bound 1 min."
        >
          <input
            type="number"
            value={heartbeatMin}
            onChange={(e) => setHeartbeatMin(e.target.value)}
            min={1}
            placeholder="30"
            className="input"
            style={{ maxWidth: 160 }}
          />
        </Field>

        <div className="flex items-center gap-3">
          <button
            type="submit"
            className="btn-primary"
            disabled={busy}
          >
            {busy ? "saving…" : "save"}
          </button>
          {savedAt && (
            <span
              className="text-xs"
              style={{ color: "var(--color-success-text)" }}
            >
              ✓ saved · changes apply on next spawn
            </span>
          )}
        </div>
      </form>

      <BootstrapEditor agentId={id} />

      <McpEditor agentId={id} />

      <RetrievalEditor
        embedding={embedding}
        onChange={(next) => setEmbedding(next)}
      />


      <section
        className="mt-10 card px-4 py-4"
        style={{ borderColor: "rgba(234, 34, 97, 0.3)" }}
      >
        <h2
          className="display-h3 mb-2"
          style={{ color: "var(--color-ruby)" }}
        >
          danger zone
        </h2>
        <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
          Delete this agent and everything it owns: workspace files,
          conversation history, typed memory, skills, cron jobs.
          Unrecoverable.
        </p>
        <button
          type="button"
          onClick={onDelete}
          className="btn-primary"
          style={{
            background: "var(--color-ruby)",
            borderColor: "var(--color-ruby)",
          }}
        >
          delete agent
        </button>
      </section>
    </div>
  );
}

/**
 * Bootstrap-file editor: SYSTEM.md / USER.md / HEARTBEAT.md (spec
 * §5.3, §9.4 layer 2). Each file gets its own textarea + save button
 * because (a) the three have different effect timing and the user
 * benefits from a clear "saved!" beat per file, (b) one big form
 * makes a typo in HEARTBEAT.md block a SYSTEM.md save the user really
 * wanted to land. Independent saves keep it simple.
 *
 * Effect timing on the hint lines mirrors spec §9.4: SYSTEM/USER are
 * frozen into the system prompt at session start, so edits land on
 * the agent's next start. HEARTBEAT.md is the deliberate exception —
 * it's re-read on every tick (§9.6), so edits land on the next tick
 * (≤ 30 min by default).
 */
function BootstrapEditor({ agentId }: { agentId: string }) {
  const [loaded, setLoaded] = useState<BootstrapView | null>(null);
  const [system, setSystem] = useState("");
  const [user, setUser] = useState("");
  const [heartbeat, setHeartbeat] = useState("");
  const [saving, setSaving] = useState<string | null>(null);
  const [savedAt, setSavedAt] = useState<Record<string, number>>({});
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const b = await api.bootstrap.get(agentId);
        if (cancelled) return;
        setLoaded(b);
        setSystem(b.system ?? "");
        setUser(b.user ?? "");
        setHeartbeat(b.heartbeat ?? "");
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  async function save(field: "system" | "user" | "heartbeat", value: string) {
    setSaving(field);
    setError(null);
    try {
      const next = await api.bootstrap.put(agentId, { [field]: value });
      setLoaded(next);
      setSavedAt((m) => ({ ...m, [field]: Date.now() }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(null);
    }
  }

  return (
    <section className="mt-10 card px-4 py-4">
      <h2 className="display-h3 mb-2">bootstrap files</h2>
      <p className="mb-4 text-sm" style={{ color: "var(--color-body)" }}>
        Three plain-text files in the agent's workspace that drive
        persona, durable user-facing facts, and heartbeat behaviour
        (spec §5.3, §9.4 layer 2). Each saves independently. Empty
        body deletes the file.
      </p>
      {error && (
        <div
          className="mb-3 px-3 py-2 text-sm"
          style={{
            background: "rgba(234, 34, 97, 0.05)",
            border: "1px solid rgba(234, 34, 97, 0.3)",
            borderRadius: 4,
            color: "var(--color-ruby)",
          }}
        >
          {error}
        </div>
      )}
      <BootstrapFileEditor
        title="SYSTEM.md — persona + identity"
        hint="Tone, values, name, purpose. Frozen into the system prompt at session start; edits take effect on the agent's next start."
        value={system}
        onChange={setSystem}
        loaded={loaded?.system ?? ""}
        onSave={() => save("system", system)}
        saving={saving === "system"}
        savedAt={savedAt.system ?? null}
      />
      <BootstrapFileEditor
        title="USER.md — what the agent knows about you"
        hint="Durable user-facing facts the agent should always have in scope. Frozen-prompt rules apply: edits take effect on next start."
        value={user}
        onChange={setUser}
        loaded={loaded?.user ?? ""}
        onSave={() => save("user", user)}
        saving={saving === "user"}
        savedAt={savedAt.user ?? null}
      />
      <BootstrapFileEditor
        title="HEARTBEAT.md — periodic self-tick instructions"
        hint="Re-read on every heartbeat tick (the deliberate exception to the frozen-prompt invariant — spec §9.4 / §9.6). Edits take effect on the next tick."
        value={heartbeat}
        onChange={setHeartbeat}
        loaded={loaded?.heartbeat ?? ""}
        onSave={() => save("heartbeat", heartbeat)}
        saving={saving === "heartbeat"}
        savedAt={savedAt.heartbeat ?? null}
      />
    </section>
  );
}

function BootstrapFileEditor({
  title,
  hint,
  value,
  onChange,
  loaded,
  onSave,
  saving,
  savedAt,
}: {
  title: string;
  hint: string;
  value: string;
  onChange: (v: string) => void;
  loaded: string;
  onSave: () => void;
  saving: boolean;
  savedAt: number | null;
}) {
  const dirty = value !== loaded;
  const justSaved = savedAt !== null && Date.now() - savedAt < 4000;
  return (
    <div className="mb-5">
      <label className="block">
        <div
          className="mb-1.5 text-sm"
          style={{ color: "var(--color-label)", fontWeight: 400 }}
        >
          {title}
        </div>
        <textarea
          value={value}
          onChange={(e) => onChange(e.target.value)}
          className="input font-mono"
          rows={6}
          placeholder="(empty)"
          style={{ minHeight: 90, fontSize: 12, resize: "vertical" }}
        />
        <div
          className="mt-1 text-xs"
          style={{ color: "var(--color-body)" }}
        >
          {hint}
        </div>
      </label>
      <div className="mt-2 flex items-center gap-2">
        <button
          type="button"
          onClick={onSave}
          disabled={saving || !dirty}
          className="btn-primary"
          style={{
            opacity: saving || !dirty ? 0.5 : 1,
            cursor: saving || !dirty ? "not-allowed" : "pointer",
          }}
        >
          {saving ? "saving…" : "save"}
        </button>
        {justSaved && (
          <span
            className="text-xs"
            style={{ color: "var(--color-stripe-green)" }}
          >
            ✓ saved
          </span>
        )}
        {dirty && !saving && !justSaved && (
          <span className="text-xs" style={{ color: "var(--color-body)" }}>
            unsaved
          </span>
        )}
      </div>
    </div>
  );
}

/**
 * MCP servers section (spec §13 Phase 3). Lets the owner toggle the
 * master gate (`policy.permissions.can_use_mcp`) and edit the
 * whitelist (`policy.mcp_servers`) without leaving the dashboard. The
 * gateway re-validates everything on PATCH; we still pre-validate the
 * binary name client-side so obvious mistakes surface immediately.
 *
 * Edits land on the agent's NEXT start (frozen-prompt invariant — the
 * runtime materialises the MCP server set in its boot sequence and
 * holds it for the rest of the session).
 */
function McpEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<McpView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [savedAt, setSavedAt] = useState<number | null>(null);

  // "draft" form for adding a new server. We don't allow inline rename;
  // remove + re-add is enough.
  const [newName, setNewName] = useState("");
  const [newBinary, setNewBinary] = useState("");
  const [newArgs, setNewArgs] = useState(""); // space-separated; empty = none
  const [newPathsRw, setNewPathsRw] = useState(""); // newline-separated
  const [newPathsRo, setNewPathsRo] = useState("");

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const v = await api.mcp.get(agentId);
        if (!cancelled) setView(v);
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  async function setEnabled(can_use_mcp: boolean) {
    setSaving(true);
    setError(null);
    try {
      const next = await api.mcp.patch(agentId, { can_use_mcp });
      setView(next);
      setSavedAt(Date.now());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  async function commitServers(
    next: Record<string, McpServerConfig>,
  ): Promise<void> {
    setSaving(true);
    setError(null);
    try {
      const updated = await api.mcp.patch(agentId, { servers: next });
      setView(updated);
      setSavedAt(Date.now());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  async function removeServer(name: string) {
    if (!view) return;
    const next = { ...view.servers };
    delete next[name];
    await commitServers(next);
  }

  async function toggleServerEnabled(name: string) {
    if (!view) return;
    const cur = view.servers[name];
    if (!cur) return;
    const next = {
      ...view.servers,
      [name]: { ...cur, enabled: !cur.enabled },
    };
    await commitServers(next);
  }

  async function addServer(e: React.FormEvent) {
    e.preventDefault();
    if (!view) return;
    const trimmed = newName.trim();
    if (!trimmed) {
      setError("server name is required");
      return;
    }
    if (view.servers[trimmed]) {
      setError(`server name "${trimmed}" already exists`);
      return;
    }
    if (!newBinary) {
      setError("pick a binary (operator must install it under /usr/share/havn/mcp-servers/ first)");
      return;
    }
    const cfg: McpServerConfig = {
      binary: newBinary,
      args: newArgs.trim() ? newArgs.trim().split(/\s+/) : [],
      env: {},
      extra_paths_rw: splitPaths(newPathsRw),
      extra_paths_ro: splitPaths(newPathsRo),
      timeout_seconds: 60,
      enabled: true,
    };
    const next = { ...view.servers, [trimmed]: cfg };
    await commitServers(next);
    // reset form on success (state already advanced via commitServers)
    if (!error) {
      setNewName("");
      setNewBinary("");
      setNewArgs("");
      setNewPathsRw("");
      setNewPathsRo("");
    }
  }

  const justSaved = savedAt !== null && Date.now() - savedAt < 4000;
  const serverEntries = view ? Object.entries(view.servers) : [];

  return (
    <section className="mt-10 card px-4 py-4">
      <h2 className="display-h3 mb-2">MCP servers</h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Model Context Protocol servers (spec §13 Phase 3) — operator-
        installed stdio binaries the agent can invoke as
        <code className="font-mono"> mcp__&lt;server&gt;__&lt;tool&gt;</code>.
        Only binaries placed under{" "}
        <code className="font-mono">/usr/share/havn/mcp-servers/</code>{" "}
        on the host are reachable; "no marketplace" means agents can't
        install new ones. Changes apply on the agent's next start.
      </p>
      {error && (
        <div
          className="mb-3 px-3 py-2 text-sm"
          style={{
            background: "rgba(234, 34, 97, 0.05)",
            border: "1px solid rgba(234, 34, 97, 0.3)",
            borderRadius: 4,
            color: "var(--color-ruby)",
          }}
        >
          {error}
        </div>
      )}
      {!view ? (
        <p style={{ color: "var(--color-body)" }}>loading…</p>
      ) : (
        <>
          <label className="mb-4 flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={view.can_use_mcp}
              disabled={saving}
              onChange={(e) => setEnabled(e.target.checked)}
            />
            <span>
              <strong>can_use_mcp</strong> — master gate. When off, the
              runtime ignores every server below (defence-in-depth: the
              policy gate makes the tool registry unaware of MCP, even
              if servers are configured).
            </span>
          </label>

          {serverEntries.length === 0 ? (
            <p
              className="mb-4 text-sm"
              style={{ color: "var(--color-body)" }}
            >
              No servers configured yet.{" "}
              {view.available_binaries.length === 0
                ? "No binaries detected at /usr/share/havn/mcp-servers/ either — install one first."
                : `${view.available_binaries.length} binary/-ies detected on host.`}
            </p>
          ) : (
            <table className="mb-4 w-full text-sm">
              <thead>
                <tr style={{ color: "var(--color-body)", textAlign: "left" }}>
                  <th className="py-1 pr-2">name</th>
                  <th className="py-1 pr-2">binary</th>
                  <th className="py-1 pr-2">args</th>
                  <th className="py-1 pr-2">paths</th>
                  <th className="py-1 pr-2">enabled</th>
                  <th className="py-1"></th>
                </tr>
              </thead>
              <tbody>
                {serverEntries.map(([name, cfg]) => (
                  <tr
                    key={name}
                    style={{
                      borderTop: "1px solid var(--color-border)",
                    }}
                  >
                    <td className="py-2 pr-2">
                      <code className="font-mono">{name}</code>
                    </td>
                    <td className="py-2 pr-2">
                      <code
                        className="font-mono"
                        style={{ fontSize: 11 }}
                      >
                        {cfg.binary}
                      </code>
                    </td>
                    <td
                      className="py-2 pr-2"
                      style={{ fontSize: 11, color: "var(--color-body)" }}
                    >
                      {cfg.args.length > 0 ? cfg.args.join(" ") : "—"}
                    </td>
                    <td
                      className="py-2 pr-2"
                      style={{ fontSize: 11, color: "var(--color-body)" }}
                    >
                      {cfg.extra_paths_rw.length === 0 &&
                      cfg.extra_paths_ro.length === 0
                        ? "—"
                        : `${cfg.extra_paths_rw.length} rw / ${cfg.extra_paths_ro.length} ro`}
                    </td>
                    <td className="py-2 pr-2">
                      <input
                        type="checkbox"
                        checked={cfg.enabled}
                        disabled={saving}
                        onChange={() => toggleServerEnabled(name)}
                      />
                    </td>
                    <td className="py-2">
                      <button
                        type="button"
                        onClick={() => removeServer(name)}
                        disabled={saving}
                        className="text-xs"
                        style={{
                          color: "var(--color-ruby)",
                          background: "none",
                          border: "none",
                          cursor: saving ? "not-allowed" : "pointer",
                          padding: 0,
                        }}
                      >
                        remove
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}

          <details className="mt-2">
            <summary
              className="cursor-pointer text-sm"
              style={{ color: "var(--color-stripe-purple)" }}
            >
              add a server
            </summary>
            <form
              onSubmit={addServer}
              className="mt-3 grid gap-3 px-3 py-3"
              style={{
                background: "var(--color-surface-muted)",
                borderRadius: 4,
              }}
            >
              <Field
                label="name"
                hint="Operator-chosen alias. Becomes the prefix in mcp__<name>__<tool>; must be unique within this agent."
              >
                <input
                  type="text"
                  value={newName}
                  onChange={(e) => setNewName(e.target.value)}
                  className="input font-mono"
                  placeholder="e.g. filesystem"
                />
              </Field>
              <Field
                label="binary"
                hint={
                  view.available_binaries.length === 0
                    ? "No binaries detected. Install one at /usr/share/havn/mcp-servers/<name> on the host before configuring."
                    : "File name under /usr/share/havn/mcp-servers/ — never a path."
                }
              >
                <select
                  value={newBinary}
                  onChange={(e) => setNewBinary(e.target.value)}
                  className="input font-mono"
                  disabled={view.available_binaries.length === 0}
                >
                  <option value="">—</option>
                  {view.available_binaries.map((b) => (
                    <option key={b} value={b}>
                      {b}
                    </option>
                  ))}
                </select>
              </Field>
              <Field
                label="args"
                hint="Space-separated. Leave empty for none. Quoting / shell expansion is NOT supported — args go straight to execve."
              >
                <input
                  type="text"
                  value={newArgs}
                  onChange={(e) => setNewArgs(e.target.value)}
                  className="input font-mono"
                  placeholder="--root /workspace/data"
                />
              </Field>
              <Field
                label="extra_paths_rw"
                hint="Newline-separated host paths the server may read+write. Unioned into the agent's Landlock allowlist (spec §13). Typical: /workspace/data."
              >
                <textarea
                  value={newPathsRw}
                  onChange={(e) => setNewPathsRw(e.target.value)}
                  className="input font-mono"
                  rows={2}
                  style={{ minHeight: 50, fontSize: 12 }}
                />
              </Field>
              <Field
                label="extra_paths_ro"
                hint="Newline-separated host paths the server may read but not write."
              >
                <textarea
                  value={newPathsRo}
                  onChange={(e) => setNewPathsRo(e.target.value)}
                  className="input font-mono"
                  rows={2}
                  style={{ minHeight: 50, fontSize: 12 }}
                />
              </Field>
              <div>
                <button
                  type="submit"
                  className="btn-primary"
                  disabled={saving}
                >
                  {saving ? "saving…" : "add server"}
                </button>
                {justSaved && (
                  <span
                    className="ml-2 text-xs"
                    style={{ color: "var(--color-stripe-green)" }}
                  >
                    ✓ saved
                  </span>
                )}
              </div>
            </form>
          </details>
        </>
      )}
    </section>
  );
}

/** Newline-separated path list parser. Trims, drops empties. */
function splitPaths(raw: string): string[] {
  return raw
    .split(/\r?\n/)
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

/**
 * Retrieval-layer editor (spec §9.4 / §13). Lets an operator switch
 * the embedding provider at runtime — one config drives both typed
 * memory AND skill discovery (mirrors the backend, where both go
 * through the shared `HybridSource` pipeline).
 *
 * Persistence semantics, surfaced in the hint text (per Linux-service
 * convention — see spec §1.6 "infrastructure not SaaS"):
 *
 * - PATCH is **in-memory** — the gateway swaps an `Arc<ArcSwap>` so
 *   subsequent agent spawns Welcome with the new config.
 * - **Already-running** agents keep their snapshot (frozen-prompt
 *   invariant); restart them to pick up the change.
 * - The change is lost on **gateway restart** unless the operator
 *   also edits `~/.config/havn/config.toml` (the canonical source of
 *   truth — file-driven so IaC / backups / Linux-service expectations
 *   keep working).
 */
function RetrievalEditor({
  embedding,
  onChange,
}: {
  embedding: EmbeddingStatus | null;
  onChange: (next: EmbeddingStatus) => void;
}) {
  const [provider, setProvider] = useState<string>("disabled");
  const [openaiModel, setOpenaiModel] = useState("text-embedding-3-small");
  const [openaiDimensions, setOpenaiDimensions] = useState<string>("1536");
  const [openaiApiKeyEnv, setOpenaiApiKeyEnv] = useState("OPENAI_API_KEY");
  const [openaiBaseUrl, setOpenaiBaseUrl] = useState(
    "https://api.openai.com/v1",
  );
  const [localModel, setLocalModel] = useState("BAAI/bge-small-en-v1.5");
  const [localDimensions, setLocalDimensions] = useState<string>("384");
  const [hrrDimensions, setHrrDimensions] = useState<string>("1024");

  const [saving, setSaving] = useState(false);
  const [savedAt, setSavedAt] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Seed form fields from the live config when it loads.
  useEffect(() => {
    if (!embedding) return;
    setProvider(embedding.provider);
    const cfg = embedding.config as Record<string, unknown>;
    const oa = (cfg.openai as Record<string, unknown> | undefined) ?? {};
    if (typeof oa.model === "string") setOpenaiModel(oa.model);
    if (typeof oa.dimensions === "number")
      setOpenaiDimensions(String(oa.dimensions));
    if (typeof oa.api_key_env === "string") setOpenaiApiKeyEnv(oa.api_key_env);
    if (typeof oa.base_url === "string") setOpenaiBaseUrl(oa.base_url);
    const lo = (cfg.local as Record<string, unknown> | undefined) ?? {};
    if (typeof lo.model === "string") setLocalModel(lo.model);
    if (typeof lo.dimensions === "number")
      setLocalDimensions(String(lo.dimensions));
    const hr = (cfg.hrr as Record<string, unknown> | undefined) ?? {};
    if (typeof hr.dimensions === "number")
      setHrrDimensions(String(hr.dimensions));
  }, [embedding]);

  async function onSave() {
    setSaving(true);
    setError(null);
    try {
      const body: Record<string, unknown> = { provider };
      if (provider === "openai") {
        const dim = Number.parseInt(openaiDimensions, 10);
        if (!Number.isFinite(dim) || dim <= 0) {
          throw new Error("dimensions must be a positive integer");
        }
        body.openai = {
          model: openaiModel.trim() || "text-embedding-3-small",
          dimensions: dim,
          api_key_env: openaiApiKeyEnv.trim() || "OPENAI_API_KEY",
          base_url: openaiBaseUrl.trim() || "https://api.openai.com/v1",
        };
      } else if (provider === "local") {
        const dim = Number.parseInt(localDimensions, 10);
        if (!Number.isFinite(dim) || dim <= 0) {
          throw new Error("dimensions must be a positive integer");
        }
        body.local = {
          model: localModel.trim() || "BAAI/bge-small-en-v1.5",
          dimensions: dim,
        };
      } else if (provider === "hrr") {
        const dim = Number.parseInt(hrrDimensions, 10);
        if (!Number.isFinite(dim) || dim <= 0) {
          throw new Error("dimensions must be a positive integer");
        }
        body.hrr = { dimensions: dim };
      }
      const next = await api.patchEmbedding(
        body as { provider: string; [k: string]: unknown },
      );
      onChange(next);
      setSavedAt(Date.now());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  const justSaved = savedAt !== null && Date.now() - savedAt < 4000;

  return (
    <section className="mt-10 card px-4 py-4">
      <h2 className="display-h3 mb-2">retrieval layer</h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Hybrid retrieval (vector + BM25 keyword) drives both typed
        memory and skill discovery — one config, both surfaces (spec
        §9.4 / §13). Switch the provider here; the change applies to
        the <strong>next agent spawn</strong> immediately. Already-
        running agents keep their snapshot (frozen-prompt invariant —
        restart them to pick up the change). The override is held{" "}
        <strong>in-memory</strong>: a gateway restart reverts to the
        value in <code className="font-mono">~/.config/havn/config.toml</code>
        , the canonical source of truth — edit there too if you want
        the change to stick.
      </p>
      {!embedding ? (
        <p style={{ color: "var(--color-body)" }}>loading…</p>
      ) : (
        <>
          {error && (
            <div
              className="mb-3 px-3 py-2 text-sm"
              style={{
                background: "rgba(234, 34, 97, 0.05)",
                border: "1px solid rgba(234, 34, 97, 0.3)",
                borderRadius: 4,
                color: "var(--color-ruby)",
              }}
            >
              {error}
            </div>
          )}
          <div className="grid gap-4">
            <Field
              label="provider"
              hint="disabled = FTS5 keyword-only (v0.6 default). openai = remote semantic embeddings (recommended). local = fastembed-rs (requires --features local-embed at build time). hrr = pure-Rust deterministic bag-of-tokens (zero deps, weakest semantic recall)."
            >
              <select
                value={provider}
                onChange={(e) => setProvider(e.target.value)}
                className="input font-mono"
              >
                <option value="disabled">disabled</option>
                <option value="openai">openai</option>
                <option value="local">local</option>
                <option value="hrr">hrr</option>
              </select>
            </Field>

            {provider === "openai" && (
              <>
                <Field
                  label="model"
                  hint="text-embedding-3-small (1536d, $0.02/1M tokens) recommended. text-embedding-3-large (3072d) for higher quality at ~6× the cost."
                >
                  <input
                    type="text"
                    value={openaiModel}
                    onChange={(e) => setOpenaiModel(e.target.value)}
                    className="input font-mono"
                  />
                </Field>
                <Field
                  label="dimensions"
                  hint="Output vector dim. text-embedding-3-* supports server-side dimensionality reduction — set this lower than the model's native dim to save storage."
                >
                  <input
                    type="number"
                    value={openaiDimensions}
                    onChange={(e) => setOpenaiDimensions(e.target.value)}
                    className="input font-mono"
                    min={1}
                  />
                </Field>
                <Field
                  label="api_key_env"
                  hint="Env var name on the gateway host whose value is the API key. Default OPENAI_API_KEY. The key value itself is never sent to the dashboard."
                >
                  <input
                    type="text"
                    value={openaiApiKeyEnv}
                    onChange={(e) => setOpenaiApiKeyEnv(e.target.value)}
                    className="input font-mono"
                  />
                </Field>
                <Field
                  label="base_url"
                  hint="Default https://api.openai.com/v1. Override for Azure OpenAI / Together / vLLM / any OpenAI-compatible endpoint."
                >
                  <input
                    type="text"
                    value={openaiBaseUrl}
                    onChange={(e) => setOpenaiBaseUrl(e.target.value)}
                    className="input font-mono"
                  />
                </Field>
              </>
            )}

            {provider === "local" && (
              <>
                <Field
                  label="model"
                  hint="Hugging Face id. Tested: BAAI/bge-small-en-v1.5 (384d), BAAI/bge-base-en-v1.5 (768d), BAAI/bge-large-en-v1.5 (1024d), nomic-ai/nomic-embed-text-v1.5 (768d), intfloat/multilingual-e5-small (384d). Requires the runtime built with --features local-embed."
                >
                  <input
                    type="text"
                    value={localModel}
                    onChange={(e) => setLocalModel(e.target.value)}
                    className="input font-mono"
                  />
                </Field>
                <Field
                  label="dimensions"
                  hint="Must match the model's native dim — e.g. bge-small = 384, bge-base = 768."
                >
                  <input
                    type="number"
                    value={localDimensions}
                    onChange={(e) => setLocalDimensions(e.target.value)}
                    className="input font-mono"
                    min={1}
                  />
                </Field>
              </>
            )}

            {provider === "hrr" && (
              <Field
                label="dimensions"
                hint="1024 mirrors Hermes's HRR default and balances recall + cosine stability. HRR has no semantic recall — use only when openai/local are off the table (air-gapped, no model files allowed)."
              >
                <input
                  type="number"
                  value={hrrDimensions}
                  onChange={(e) => setHrrDimensions(e.target.value)}
                  className="input font-mono"
                  min={1}
                />
              </Field>
            )}

            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={onSave}
                disabled={saving}
                className="btn-primary"
                style={{
                  opacity: saving ? 0.5 : 1,
                  cursor: saving ? "not-allowed" : "pointer",
                }}
              >
                {saving ? "saving…" : "save"}
              </button>
              {justSaved && (
                <span
                  className="text-xs"
                  style={{ color: "var(--color-stripe-green)" }}
                >
                  ✓ swapped — affects next agent spawn
                </span>
              )}
              {embedding.hybrid_enabled ? (
                <span
                  className="pill ml-auto"
                  style={{
                    background: "rgba(83, 58, 253, 0.08)",
                    color: "var(--color-stripe-purple)",
                  }}
                  title={`Active: ${embedding.provider}`}
                >
                  hybrid on ({embedding.provider})
                </span>
              ) : (
                <span className="pill pill-stopped ml-auto">
                  keyword-only
                </span>
              )}
            </div>
          </div>
        </>
      )}
    </section>
  );
}

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint: string;
  children: React.ReactNode;
}) {
  return (
    <label className="grid gap-1.5">
      <span
        className="text-sm"
        style={{ color: "var(--color-label)", fontWeight: 400 }}
      >
        {label}
      </span>
      {children}
      <span className="text-xs" style={{ color: "var(--color-body)" }}>
        {hint}
      </span>
    </label>
  );
}
