"use client";

/**
 * `/agent/settings?id=` — per-agent configuration.
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

import { useCallback, useEffect, useRef, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import {
  ApiError,
  api,
  type AgentView,
  type AgentWebPatch,
  type AgentWebView,
  type BootstrapView,
  type EmbeddingStatus,
  type McpServerConfig,
  type McpView,
  type ModelsView,
  type CronJobView,
  type ApprovalsView,
  type TitlesView,
  type CapabilitiesView,
  type Access,
  type WorkspaceAccess,
  type PairingCode,
  type ChannelLinkView,
  type MyDomainsView,
  type TimezoneView,
} from "@/lib/api";

function useFlash(ms = 4000): [boolean, () => void] {
  const [on, setOn] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );
  const flash = useCallback(() => {
    setOn(true);
    if (timer.current) clearTimeout(timer.current);
    timer.current = setTimeout(() => setOn(false), ms);
  }, [ms]);
  return [on, flash];
}

export default function SettingsPage() {
  const id = useSearchParams().get("id") ?? "";
  return <Settings key={id} id={id} />;
}

function Settings({ id }: { id: string }) {
  const router = useRouter();

  const [agent, setAgent] = useState<AgentView | null>(null);
  const [embedding, setEmbedding] = useState<EmbeddingStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saved, flashSaved] = useFlash();
  const [busy, setBusy] = useState(false);

  // Form state, seeded from the agent on load. Local-only until Save.
  const [name, setName] = useState("");
  const [model, setModel] = useState("");
  const [models, setModels] = useState<ModelsView | null>(null);
  const [heartbeatMin, setHeartbeatMin] = useState<string>("");
  const [language, setLanguage] = useState<"en" | "zh">("en");

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
        setLanguage(cfg.language === "zh" ? "zh" : "en");
        // Without the list the picker only offers the current model.
        api.models
          .get()
          .then((m) => {
            if (!cancelled) setModels(m);
          })
          .catch(() => {});
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
      // Only a changed model is sent: an agent whose model was taken off
      // the list can still be renamed. null clears the key, and the agent
      // then follows the gateway's default model.
      const saved = (agent.config as Record<string, unknown>).model;
      if (model !== (typeof saved === "string" ? saved : "")) {
        config.model = model || null;
      }
      const savedLanguage = (agent.config as Record<string, unknown>).language === "zh" ? "zh" : "en";
      if (language !== savedLanguage) {
        config.language = language === "en" ? null : language;
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
      flashSaved();
    } catch (e) {
      if (e instanceof ApiError) setError(e.message);
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
      <div className="mx-auto max-w-3xl px-4 py-8 sm:px-8 sm:py-12">
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
      <div className="px-4 py-8 sm:px-8 sm:py-12" style={{ color: "var(--color-body)" }}>
        loading…
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-3xl px-4 py-8 sm:px-8 sm:py-10">
      <header className="mb-6">
        <Link
          href={`/agent?id=${id}`}
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
          Changes apply on the agent&apos;s next start. A running agent keeps
          the system prompt and policy it started with until it restarts.
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
          hint="Used for every LLM call of this agent. The list is the gateway's [[llm.models]]; your role's policy may allow only some of them."
        >
          <select
            value={model}
            onChange={(e) => setModel(e.target.value)}
            className="input font-mono"
          >
            <option value="">
              {models?.effective_default
                ? `gateway default (${models.effective_default})`
                : "gateway default"}
            </option>
            {(models?.models ?? []).map((m) => (
              <option key={m.id} value={m.id}>
                {m.label ? `${m.label} (${m.id})` : m.id}
              </option>
            ))}
            {model && !models?.models.some((m) => m.id === model) && (
              <option value={model}>{model} (not in the model list)</option>
            )}
          </select>
          <p className="mt-1.5 text-xs" style={{ color: "var(--color-body)" }}>
            <Link href="/models" className="underline">
              Manage the model list
            </Link>
          </p>
        </Field>

        <Field
          label="Language"
          hint="What Havn writes for this agent: the parts of the prompt it adds, and its own messages, such as replies to /model or /new and approval notices. The agent answers people in their own language either way; tool descriptions stay English."
        >
          <select
            value={language}
            onChange={(e) => setLanguage(e.target.value === "zh" ? "zh" : "en")}
            className="input"
            style={{ maxWidth: 200 }}
          >
            <option value="en">English</option>
            <option value="zh">中文</option>
          </select>
        </Field>

        <Field
          label="Heartbeat interval (minutes)"
          hint="How often the agent's heartbeat tick fires (a proactive tick that reads HEARTBEAT.md). Empty = use the default 30 min. Lower bound 1 min."
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
          {saved && (
            <span
              className="text-xs"
              style={{ color: "var(--color-success-text)" }}
            >
              saved · changes apply on next spawn
            </span>
          )}
        </div>
      </form>

      <BootstrapEditor agentId={id} />

      <WebAccessEditor agentId={id} />
      <TimezoneEditor />
      <ScheduledTasks agentId={id} />
      <TitlesEditor agentId={id} />
      <CapabilitiesEditor agentId={id} />
      <AccessEditor agentId={id} />
      <WorkspaceAccessEditor agentId={id} />
      <ChannelPairing agentId={id} />
      <ApprovalsEditor agentId={id} />

      <McpEditor agentId={id} />

      {embedding && (
        <RetrievalEditor embedding={embedding} onChange={(next) => setEmbedding(next)} />
      )}


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
 * Bootstrap-file editor: SOUL.md / USER.md / HEARTBEAT.md (spec
 * §5.3, §9.4 layer 2). Each file gets its own textarea + save button
 * because (a) the three have different effect timing and the user
 * benefits from a clear "saved!" beat per file, (b) one big form
 * makes a typo in HEARTBEAT.md block a SOUL.md save the user really
 * wanted to land. Independent saves keep it simple.
 *
 * Effect timing on the hint lines mirrors spec §9.4: SOUL/USER are
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
  const [savedSystem, flashSystem] = useFlash();
  const [savedUser, flashUser] = useFlash();
  const [savedHeartbeat, flashHeartbeat] = useFlash();
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
      ({ system: flashSystem, user: flashUser, heartbeat: flashHeartbeat })[field]();
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
        Three plain-text files in the agent&apos;s workspace that drive
        persona, durable user-facing facts, and heartbeat behaviour.
        Each saves independently. Empty
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
        title="SOUL.md — persona + identity"
        hint="Tone, values, name, purpose. Frozen into the system prompt at session start; edits take effect on the agent's next start."
        value={system}
        onChange={setSystem}
        loaded={loaded?.system ?? ""}
        onSave={() => save("system", system)}
        saving={saving === "system"}
        justSaved={savedSystem}
      />
      <BootstrapFileEditor
        title="USER.md — what the agent knows about you"
        hint="Durable user-facing facts the agent should always have in scope. Frozen-prompt rules apply: edits take effect on next start."
        value={user}
        onChange={setUser}
        loaded={loaded?.user ?? ""}
        onSave={() => save("user", user)}
        saving={saving === "user"}
        justSaved={savedUser}
      />
      <BootstrapFileEditor
        title="HEARTBEAT.md — periodic self-tick instructions"
        hint="Re-read on every heartbeat tick (unlike the system prompt, which is fixed while the agent runs). Edits take effect on the next tick."
        value={heartbeat}
        onChange={setHeartbeat}
        loaded={loaded?.heartbeat ?? ""}
        onSave={() => save("heartbeat", heartbeat)}
        saving={saving === "heartbeat"}
        justSaved={savedHeartbeat}
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
  justSaved,
}: {
  title: string;
  hint: string;
  value: string;
  onChange: (v: string) => void;
  loaded: string;
  onSave: () => void;
  saving: boolean;
  justSaved: boolean;
}) {
  const dirty = value !== loaded;
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
            style={{ color: "var(--color-success-text)" }}
          >
            saved
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
 * Connect the caller's chat-app accounts (Telegram, ...) to their Havn
 * account through this agent's bots, and see or remove those links.
 */
function ChannelPairing({ agentId }: { agentId: string }) {
  const [codes, setCodes] = useState<PairingCode[] | null>(null);
  const [links, setLinks] = useState<ChannelLinkView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const loadLinks = useCallback(() => {
    api.channels.links().then(setLinks, (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
    );
  }, []);
  useEffect(loadLinks, [loadLinks]);
  // While codes are shown: a code redeemed in the chat app shows up as a
  // link, and one past its time as expired.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!codes) return;
    const timer = setInterval(() => {
      setNow(Date.now());
      loadLinks();
    }, 5000);
    return () => clearInterval(timer);
  }, [codes, loadLinks]);
  async function connect() {
    setBusy(true);
    setError(null);
    try {
      setCodes(await api.channels.pair(agentId));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  async function remove(link: ChannelLinkView) {
    setError(null);
    try {
      await api.channels.unlink(link.channel, link.sender_id);
      loadLinks();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  const muted = { color: "var(--color-body)" };
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="pairing-title">
      <h2 id="pairing-title" className="display-h3 mb-2">
        chat apps
      </h2>
      <p className="mb-3 text-sm" style={muted}>
        Connect your Telegram account to your Havn account: then this agent&apos;s bot (and your
        other agents&apos; bots) answers you without a code from the server, and you can use the
        admin commands there, such as /model and /approve.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <button type="button" className="btn-ghost" disabled={busy} onClick={() => void connect()}>
        {codes ? "make new codes" : "connect"}
      </button>
      {codes && (
        <ul className="mt-3 grid gap-2 text-sm" aria-label="pairing codes">
          {codes.map((c) => (
            <li key={`${c.channel}/${c.account_id}`} className="rounded border px-3 py-2" style={{ borderColor: "var(--color-hairline)" }}>
              <p className="font-mono text-xs" style={muted}>
                {c.channel} / {c.account_id}
              </p>
              {c.link ? (
                <p>
                  Open{" "}
                  <a href={c.link} target="_blank" rel="noreferrer" className="underline">
                    {c.link}
                  </a>{" "}
                  and press Start, or send the bot <span className="font-mono">/start</span> and this
                  code:
                </p>
              ) : (
                <p>
                  Send the bot <span className="font-mono">/start</span> and this code:
                </p>
              )}
              <p className="font-mono text-base tracking-wider" aria-label="pairing code">
                {c.code}
              </p>
              <p className="text-xs" style={muted}>
                {Date.parse(c.expires_at) > now
                  ? `Works once, until ${new Date(c.expires_at).toLocaleTimeString()}.`
                  : "Expired: make new codes."}
              </p>
            </li>
          ))}
        </ul>
      )}
      {links && links.length > 0 && (
        <div className="mt-4">
          <p className="mb-1 text-sm" style={muted}>
            Connected to your Havn account (for all your agents):
          </p>
          <ul className="grid gap-1 text-sm" aria-label="connected accounts">
            {links.map((l) => (
              <li key={`${l.channel}/${l.sender_id}`} className="flex flex-wrap items-center gap-3">
                <span className="font-mono">{l.sender_id}</span>
                <span style={muted}>{l.channel}</span>
                <button type="button" className="text-xs underline" onClick={() => void remove(l)}>
                  disconnect
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </section>
  );
}

/** Whether the agent's workspace can be browsed from the dashboard. */
function WorkspaceAccessEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<WorkspaceAccess | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api.workspace.access(agentId).then(
      (v) => {
        if (!cancelled) setView(v);
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);
  async function toggle(enabled: boolean) {
    setSaving(true);
    setError(null);
    try {
      setView(await api.workspace.setAccess(agentId, enabled));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  if (!view) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="workspace-title">
      <h2 id="workspace-title" className="display-h3 mb-2">
        workspace browsing
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        A read-only page of the files in the agent&apos;s workspace: folders, previews of text
        and images, downloads. Only you see it.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <label className="inline-flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={view.agent}
          disabled={saving || (!view.gateway && !view.agent)}
          onChange={(e) => void toggle(e.target.checked)}
        />
        let me browse this agent&apos;s workspace
      </label>
      {!view.gateway && (
        <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
          Workspace browsing is off for this gateway; it is turned on in the gateway&apos;s
          settings ([workspace] browse in config.toml). You can still turn it off here, so
          this agent stays closed when the gateway allows it.
        </p>
      )}
    </section>
  );
}

/** Whether a model titles the agent's new chats. */
function TitlesEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<TitlesView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api.titles.get(agentId).then(
      (v) => {
        if (!cancelled) setView(v);
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);
  async function toggle(enabled: boolean) {
    setSaving(true);
    setError(null);
    try {
      setView(await api.titles.put(agentId, enabled));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  if (!view) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="titles-title">
      <h2 id="titles-title" className="display-h3 mb-2">
        chat titles
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        A model names each new chat from its first message, while the agent answers it.
        Off, a chat is named after the start of its first message. Written by{" "}
        {view.model ? <span className="font-mono">{view.model}</span> : "the agent's own model"}
        ; the gateway&apos;s title model is set on the models page. Applies on the
        agent&apos;s next start.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <label className="inline-flex items-center gap-2 text-sm">
        <input
          type="checkbox"
          checked={view.enabled}
          disabled={saving || (view.locked && !view.enabled)}
          onChange={(e) => void toggle(e.target.checked)}
        />
        let a model title new chats
      </label>
      {view.locked && (
        <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
          Your role has chat titles off; a team admin can turn them on.
        </p>
      )}
    </section>
  );
}

/** One permission's key, for the rules being edited. */
function accessKey(a: Pick<Access, "scope" | "tool" | "input">): string {
  return JSON.stringify([a.scope, a.tool, a.input]);
}

/**
 * What is always allowed in conversations not the owner's (issue #255): a
 * tool's results, or one exact call, each with its rule for what not to say.
 */
function AccessEditor({ agentId }: { agentId: string }) {
  const [allowed, setAllowed] = useState<Access[] | null>(null);
  const [rules, setRules] = useState<Record<string, string>>({});
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  function show(list: Access[]) {
    setAllowed(list);
    setRules(Object.fromEntries(list.map((a) => [accessKey(a), a.rule])));
  }
  useEffect(() => {
    let cancelled = false;
    api.access.get(agentId).then(
      (v) => {
        if (!cancelled) show(v.allowed);
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);
  async function save(a: Access, rule: string | null) {
    setSaving(true);
    setError(null);
    try {
      show((await api.access.put(agentId, a, rule)).allowed);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  const muted = { color: "var(--color-body)" };
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="access-title">
      <h2 id="access-title" className="display-h3 mb-2">
        tools in other people&apos;s conversations
      </h2>
      <p className="mb-3 text-sm" style={muted}>
        When someone other than you is talking to the agent, you decide before it runs or changes
        anything, and before it uses what it read (memory, files). These are always allowed, with your
        rule for what not to say there. Revoke one to be asked again.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      {allowed === null ? null : allowed.length === 0 ? (
        <p className="text-sm" style={muted}>
          None yet.
        </p>
      ) : (
        <ul className="flex flex-col gap-3">
          {allowed.map((a) => {
            const key = accessKey(a);
            return (
              <li key={key} className="text-sm">
                <div className="font-mono wrap-anywhere">
                  {a.channel ? `${a.channel} · ${a.account} · ${a.sender}` : a.scope}
                </div>
                <div className="text-xs wrap-anywhere" style={muted}>
                  <span className="font-mono">{a.tool}</span>
                  {a.input ? (
                    <>
                      {" "}
                      only with <span className="font-mono">{a.input}</span>
                    </>
                  ) : (
                    " results"
                  )}
                </div>
                <textarea
                  className="input mt-1 w-full"
                  rows={2}
                  maxLength={1000}
                  placeholder="what not to say here (optional)"
                  value={rules[key] ?? ""}
                  onChange={(e) => setRules({ ...rules, [key]: e.target.value })}
                />
                <div className="mt-1 flex gap-2">
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={saving || (rules[key] ?? "") === a.rule}
                    onClick={() => void save(a, rules[key] ?? "")}
                  >
                    save rule
                  </button>
                  <button type="button" className="btn-ghost" disabled={saving} onClick={() => void save(a, null)}>
                    revoke
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}

/** What the agent may do beyond its own turn: subagents, and asking other agents. */
function CapabilitiesEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<CapabilitiesView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api.capabilities.get(agentId).then(
      (v) => {
        if (!cancelled) setView(v);
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);
  async function toggle(key: keyof CapabilitiesView, enabled: boolean) {
    setSaving(true);
    setError(null);
    try {
      setView(await api.capabilities.put(agentId, { [key]: enabled }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  if (!view) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  const rows: { key: keyof CapabilitiesView; label: string; about: string; locked: string }[] = [
    {
      key: "subagents",
      label: "hand tasks to subagents",
      about:
        "The agent can split work into tasks that run at once, each on a model and thinking it picks, with or without the chat so far. They show in the chat's steps, not as chats of their own. Each costs model calls.",
      locked: "Your role has subagents off; a team admin can turn them on.",
    },
    {
      key: "agent_queries",
      label: "ask your other agents",
      about:
        "The agent can put a question to another agent of yours and use its answer. The other agent answers from its own memory and tools, on its own model.",
      locked: "Your role does not let agents ask each other; a team admin can allow it.",
    },
  ];
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="capabilities-title">
      <h2 id="capabilities-title" className="display-h3 mb-2">
        working with other agents
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Applies on the agent&apos;s next start.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <div className="flex flex-col gap-4">
        {rows.map((row) => (
          <div key={row.key}>
            <label className="inline-flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                checked={view[row.key].enabled}
                disabled={saving || (view[row.key].locked && !view[row.key].enabled)}
                onChange={(e) => void toggle(row.key, e.target.checked)}
              />
              {row.label}
            </label>
            <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
              {row.about}
            </p>
            {view[row.key].locked && (
              <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
                {row.locked}
              </p>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}

/**
 * Tools whose every call waits for your approval in the chat (or /approve
 * in a channel). A name ending in * covers a prefix, such as all tools of
 * one MCP server. Applies on the agent's next start.
 */
function ApprovalsEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<ApprovalsView | null>(null);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api.approvals.get(agentId).then(
      (v) => {
        if (cancelled) return;
        setView(v);
        setText(v.tools.join("\n"));
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);
  async function save() {
    setSaving(true);
    setError(null);
    try {
      const tools = text
        .split(/[\s,]+/)
        .map((t) => t.trim())
        .filter((t) => t.length > 0);
      const v = await api.approvals.put(agentId, tools);
      setView(v);
      setText(v.tools.join("\n"));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  if (!view) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="approvals-title">
      <h2 id="approvals-title" className="display-h3 mb-2">
        tools that need your approval
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Every call of these tools waits until you approve it in the chat, or with
        /approve in a channel; unanswered calls are refused after 10 minutes, and
        scheduled runs cannot use them. One name per line, such as{" "}
        <code className="font-mono">shell</code> or{" "}
        <code className="font-mono">mcp__github__*</code>. Applies on the agent&apos;s next
        start.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <textarea
        className="input w-full font-mono text-xs"
        rows={3}
        value={text}
        disabled={saving}
        onChange={(e) => setText(e.target.value)}
        aria-label="tools that need approval"
      />
      {view.inherited.length > 0 && (
        <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
          Also, from your role: <span className="font-mono">{view.inherited.join(", ")}</span>
        </p>
      )}
      <div className="mt-2">
        <button
          type="button"
          className="btn-ghost"
          disabled={saving || text === view.tools.join("\n")}
          onClick={save}
        >
          save
        </button>
      </div>
    </section>
  );
}

/**
 * The agent's scheduled tasks: the ones it made from chat with its
 * `schedule` tool and the ones created over the API. Pause, resume or
 * delete them here.
 */
function ScheduledTasks({ agentId }: { agentId: string }) {
  const [jobs, setJobs] = useState<CronJobView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const load = useCallback(() => {
    api.cron.list(agentId).then(setJobs, (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
    );
  }, [agentId]);
  useEffect(load, [load]);
  async function act(work: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await work();
      load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }
  if (!jobs) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="tasks-title">
      <h2 id="tasks-title" className="display-h3 mb-2">
        scheduled tasks
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Reminders and recurring jobs. Ask the agent in chat to create one, such as
        &quot;remind me at 17:00 to call Anna&quot; or &quot;every weekday at 9, summarise
        the news&quot;.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      {jobs.length === 0 ? (
        <p className="text-sm" style={{ color: "var(--color-body)" }}>
          No scheduled tasks.
        </p>
      ) : (
        <ul className="grid gap-3 text-sm">
          {jobs.map((job) => (
            <li key={job.id} className="flex flex-wrap items-start justify-between gap-2">
              <div className="min-w-0">
                <strong>{job.name}</strong>{" "}
                <span style={{ color: "var(--color-body)" }}>{job.summary}</span>
                <div className="text-xs" style={{ color: "var(--color-body)" }}>
                  {job.enabled
                    ? `next ${new Date(job.next_run_at).toLocaleString()}`
                    : job.summary.startsWith("once") && new Date(job.next_run_at) <= new Date()
                      ? "done"
                      : "paused"}
                  {job.last_run &&
                    (job.last_run.error
                      ? `; last run failed: ${job.last_run.error}`
                      : `; last run ${new Date(job.last_run.started_at).toLocaleString()}`)}
                </div>
                <div className="truncate text-xs" title={job.prompt}>
                  {job.prompt}
                </div>
              </div>
              <div className="flex gap-2">
                <button
                  type="button"
                  className="btn-ghost"
                  disabled={busy}
                  onClick={() => act(() => api.cron.setEnabled(agentId, job.id, !job.enabled))}
                >
                  {job.enabled ? "pause" : "resume"}
                </button>
                <button
                  type="button"
                  className="btn-ghost"
                  disabled={busy}
                  onClick={() => act(() => api.cron.remove(agentId, job.id))}
                >
                  delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** The caller's time zone, which all their agents tell the time in. */
function TimezoneEditor() {
  const [view, setView] = useState<TimezoneView | null>(null);
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const zones = useState<string[]>(() => {
    try {
      return Intl.supportedValuesOf("timeZone");
    } catch {
      return [];
    }
  })[0];
  const browser = Intl.DateTimeFormat().resolvedOptions().timeZone;
  useEffect(() => {
    let cancelled = false;
    api.timezone().then(
      (v) => {
        if (cancelled) return;
        setView(v);
        setDraft(v.timezone);
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);
  async function save(timezone: string | null) {
    setSaving(true);
    setError(null);
    try {
      const v = await api.setTimezone(timezone);
      setView(v);
      setDraft(v.timezone);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  if (!view) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="timezone-title">
      <h2 id="timezone-title" className="display-h3 mb-2">
        your time zone
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        All your agents tell the time in it, and read &quot;tomorrow at 9&quot; in it. It
        applies at once.{" "}
        {view.chosen ? "" : `Not set: the gateway's zone, ${view.host}, is used.`}
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <input
          className="input font-mono"
          list="timezone-names"
          value={draft}
          disabled={saving}
          onChange={(e) => setDraft(e.target.value)}
          aria-label="time zone"
          style={{ minWidth: 240 }}
        />
        <datalist id="timezone-names">
          {zones.map((z) => (
            <option key={z} value={z} />
          ))}
        </datalist>
        <button
          type="button"
          className="btn-primary"
          disabled={saving || !draft.trim() || draft === view.timezone}
          onClick={() => save(draft.trim())}
        >
          save
        </button>
        {browser && browser !== view.timezone && (
          <button
            type="button"
            className="btn-ghost"
            disabled={saving}
            onClick={() => save(browser)}
          >
            use this browser&apos;s ({browser})
          </button>
        )}
        {view.chosen && (
          <button
            type="button"
            className="btn-ghost"
            disabled={saving}
            onClick={() => save(null)}
          >
            use the gateway&apos;s
          </button>
        )}
      </div>
    </section>
  );
}

// Web search and network egress, two switches in the agent's policy.
function WebAccessEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<AgentWebView | null>(null);
  const [error, setError] = useState<string | null>(null);
  // A gateway older than these settings has no /agents/{id}/web.
  const [unsupported, setUnsupported] = useState(false);
  const [saving, setSaving] = useState(false);
  const [justSaved, flashSaved] = useFlash();

  useEffect(() => {
    let cancelled = false;
    api.search.agent(agentId).then(
      (v) => {
        if (!cancelled) setView(v);
      },
      (e) => {
        if (cancelled) return;
        if (e instanceof ApiError && e.status === 404) setUnsupported(true);
        else setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  async function save(patch: AgentWebPatch) {
    setSaving(true);
    setError(null);
    try {
      setView(await api.search.patchAgent(agentId, patch));
      flashSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  if (unsupported) return null;
  const searchLocked = saving || (view !== null && !view.search_permitted && !view.can_search_web);
  const fetchLocked = saving || (view !== null && !view.fetch_permitted && !view.can_fetch_web);
  const egressLocked = saving || (view !== null && !view.egress_permitted && !view.egress_allowed);

  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="web-access-title">
      <h2 id="web-access-title" className="display-h3 mb-2">
        web access
      </h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Changes apply on the agent&apos;s next start.
        <span className="ml-2" style={{ color: "var(--color-success-text)" }} role="status">
          {justSaved ? "saved" : ""}
        </span>
      </p>
      {error && (
        <div
          role="alert"
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
        !error && <p style={{ color: "var(--color-body)" }}>loading…</p>
      ) : (
        <div className="grid gap-4">
          <label className="flex items-start gap-3 text-sm">
            <input
              type="checkbox"
              className={`mt-1 h-4 w-4 shrink-0 ${searchLocked ? "cursor-not-allowed" : "cursor-pointer"}`}
              checked={view.can_search_web}
              disabled={searchLocked}
              onChange={(e) => save({ can_search_web: e.target.checked })}
            />
            <span>
              <strong>web search</strong>: the gateway searches for the agent, so it
              works without network access and the search key stays out of the
              sandbox. Queries go to the search provider.{" "}
              {view.search_backend ? (
                <>Searches use {view.search_backend}.</>
              ) : (
                <>
                  No search backend is set up yet: add a key on the{" "}
                  <Link href="/credentials" className="underline" style={{ color: "var(--color-stripe-purple)" }}>
                    credentials
                  </Link>{" "}
                  page.
                </>
              )}
              {!view.search_permitted && " Your role does not allow it."}
            </span>
          </label>
          <label className="flex items-start gap-3 text-sm">
            <input
              type="checkbox"
              className={`mt-1 h-4 w-4 shrink-0 ${fetchLocked ? "cursor-not-allowed" : "cursor-pointer"}`}
              checked={view.can_fetch_web}
              disabled={fetchLocked}
              onChange={(e) => save({ can_fetch_web: e.target.checked })}
            />
            <span>
              <strong>read web pages</strong>: the gateway reads a page or PDF for the
              agent and answers its question about it with a model call, so it works
              without network access. The agent can read only links you sent it, search
              results and links on pages it read.
              {!view.fetch_permitted && " Your role does not allow it."}
            </span>
          </label>
          <label className="grid gap-1 text-sm">
            <span>
              <strong>network access</strong> of the agent&apos;s shell tools (pip, npm, git,
              curl). Private, link-local and gateway-host addresses stay blocked.
              {!view.egress_permitted && " Your role does not allow it."}
            </span>
            <select
              className="input"
              value={!view.egress_allowed ? "off" : view.egress_proxy ? "allowlist" : "all"}
              disabled={egressLocked}
              onChange={(e) => {
                const mode = e.target.value;
                save(
                  mode === "off"
                    ? { egress_allowed: false }
                    : { egress_allowed: true, egress_proxy: mode === "allowlist" },
                );
              }}
              style={{ maxWidth: 420 }}
            >
              <option value="off">off</option>
              <option value="allowlist">
                allowed domains; ask me about each new one
              </option>
              <option value="all">every public address</option>
            </select>
          </label>
          <DomainLists
            key={`${view.allowed_domains.join(",")}|${view.blocked_domains.join(",")}`}
            title="this agent"
            allowed={view.allowed_domains}
            blocked={view.blocked_domains}
            inheritedAllowed={view.inherited_allowed_domains}
            inheritedBlocked={view.inherited_blocked_domains}
            inheritedLabel="from you, your role and the operator"
            saving={saving}
            onSave={(allowed_domains, blocked_domains) =>
              save({ allowed_domains, blocked_domains })
            }
          />
          <MyDomainsEditor
            onSaved={() => {
              // The agent's inherited lines include the user's lists.
              void api.search.agent(agentId).then(setView, () => undefined);
            }}
          />
        </div>
      )}
    </section>
  );
}

function parseDomains(text: string): string[] {
  return text
    .split(/[\s,]+/)
    .map((d) => d.trim().toLowerCase().replace(/\.$/, ""))
    .filter((d) => d.length > 0);
}

/**
 * Allowed and blocked domains at one level. Allowed domains can be read
 * by web_fetch without appearing in the conversation first; blocked ones
 * are never read and win over allowed ones. Inherited lists are shown
 * read-only: this level can add to them, not remove them. Callers key it
 * by the saved lists, so a save resets the text.
 */
function DomainLists({
  title,
  allowed,
  blocked,
  inheritedAllowed,
  inheritedBlocked,
  inheritedLabel,
  saving,
  onSave,
}: {
  title: string;
  allowed: string[];
  blocked: string[];
  inheritedAllowed: string[];
  inheritedBlocked: string[];
  inheritedLabel: string;
  saving: boolean;
  onSave: (allowed: string[], blocked: string[]) => void | Promise<void>;
}) {
  const [allowedText, setAllowedText] = useState(allowed.join("\n"));
  const [blockedText, setBlockedText] = useState(blocked.join("\n"));
  const changed =
    parseDomains(allowedText).join("\n") !== allowed.join("\n") ||
    parseDomains(blockedText).join("\n") !== blocked.join("\n");
  const inherited = (list: string[]) =>
    list.length > 0 && (
      <p className="mt-1 text-xs" style={{ color: "var(--color-body)" }}>
        Also, {inheritedLabel}: <span className="font-mono">{list.join(", ")}</span>
      </p>
    );
  return (
    <fieldset className="grid gap-2 text-sm">
      <legend className="mb-1">
        <strong>domains for {title}</strong>: one per line; a domain covers its
        subdomains. Allowed domains can be read without appearing in the
        conversation first; blocked domains are never read, even when allowed.
      </legend>
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="grid gap-1">
          <span>allowed</span>
          <textarea
            className="input font-mono text-xs"
            rows={4}
            value={allowedText}
            disabled={saving}
            onChange={(e) => setAllowedText(e.target.value)}
            placeholder="docs.example.com"
          />
          {inherited(inheritedAllowed)}
        </label>
        <label className="grid gap-1">
          <span>blocked</span>
          <textarea
            className="input font-mono text-xs"
            rows={4}
            value={blockedText}
            disabled={saving}
            onChange={(e) => setBlockedText(e.target.value)}
            placeholder="ads.example.com"
          />
          {inherited(inheritedBlocked)}
        </label>
      </div>
      <div>
        <button
          type="button"
          className="btn-ghost"
          disabled={saving || !changed}
          onClick={() => onSave(parseDomains(allowedText), parseDomains(blockedText))}
        >
          save domains
        </button>
      </div>
    </fieldset>
  );
}

/** The caller's own domain lists, added to every agent they own. */
function MyDomainsEditor({ onSaved }: { onSaved: () => void }) {
  const [view, setView] = useState<MyDomainsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api.search.myDomains().then(
      (v) => {
        if (!cancelled) setView(v);
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);
  async function save(allowed_domains: string[], blocked_domains: string[]) {
    setSaving(true);
    setError(null);
    try {
      setView(await api.search.putMyDomains({ allowed_domains, blocked_domains }));
      onSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  if (!view) return error ? <p className="text-sm" role="alert">{error}</p> : null;
  return (
    <div className="grid gap-2">
      {error && (
        <p className="text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      <DomainLists
        key={`${view.allowed_domains.join(",")}|${view.blocked_domains.join(",")}`}
        title="all your agents"
        allowed={view.allowed_domains}
        blocked={view.blocked_domains}
        inheritedAllowed={view.admin_allowed_domains}
        inheritedBlocked={view.admin_blocked_domains}
        inheritedLabel="from your role and the operator"
        saving={saving}
        onSave={save}
      />
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
  const [justSaved, flashSaved] = useFlash();

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
      flashSaved();
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
      flashSaved();
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

  const serverEntries = view ? Object.entries(view.servers) : [];

  return (
    <section className="mt-10 card px-4 py-4">
      <h2 className="display-h3 mb-2">MCP servers</h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Model Context Protocol servers: stdio binaries the operator
        installed, which the agent can invoke as
        <code className="font-mono"> mcp__&lt;server&gt;__&lt;tool&gt;</code>.
        Only binaries placed under{" "}
        <code className="font-mono">/usr/share/havn/mcp-servers/</code>{" "}
        on the host are reachable; &quot;no marketplace&quot; means agents can&apos;t
        install new ones. Changes apply on the agent&apos;s next start.
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
                hint="Newline-separated host paths the server may read+write. Unioned into the agent's Landlock allowlist. Typical: /workspace/data."
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
                    style={{ color: "var(--color-success-text)" }}
                  >
                    saved
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
function embeddingForm(embedding: EmbeddingStatus) {
  const cfg = embedding.config as Record<string, unknown>;
  const oa = (cfg.openai as Record<string, unknown> | undefined) ?? {};
  const lo = (cfg.local as Record<string, unknown> | undefined) ?? {};
  const hr = (cfg.hrr as Record<string, unknown> | undefined) ?? {};
  const str = (v: unknown, fallback: string) => (typeof v === "string" ? v : fallback);
  const num = (v: unknown, fallback: string) => (typeof v === "number" ? String(v) : fallback);
  return {
    provider: embedding.provider,
    openaiModel: str(oa.model, "text-embedding-3-small"),
    openaiDimensions: num(oa.dimensions, "1536"),
    openaiApiKeyEnv: str(oa.api_key_env, "OPENAI_API_KEY"),
    openaiBaseUrl: str(oa.base_url, "https://api.openai.com/v1"),
    localModel: str(lo.model, "BAAI/bge-small-en-v1.5"),
    localDimensions: num(lo.dimensions, "384"),
    hrrDimensions: num(hr.dimensions, "1024"),
  };
}

function RetrievalEditor({
  embedding,
  onChange,
}: {
  embedding: EmbeddingStatus;
  onChange: (next: EmbeddingStatus) => void;
}) {
  const [initial] = useState(() => embeddingForm(embedding));
  const [provider, setProvider] = useState(initial.provider);
  const [openaiModel, setOpenaiModel] = useState(initial.openaiModel);
  const [openaiDimensions, setOpenaiDimensions] = useState(initial.openaiDimensions);
  const [openaiApiKeyEnv, setOpenaiApiKeyEnv] = useState(initial.openaiApiKeyEnv);
  const [openaiBaseUrl, setOpenaiBaseUrl] = useState(initial.openaiBaseUrl);
  const [localModel, setLocalModel] = useState(initial.localModel);
  const [localDimensions, setLocalDimensions] = useState(initial.localDimensions);
  const [hrrDimensions, setHrrDimensions] = useState(initial.hrrDimensions);

  const [saving, setSaving] = useState(false);
  const [justSaved, flashSaved] = useFlash();
  const [error, setError] = useState<string | null>(null);

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
      // Show what the gateway applied, not just what was typed.
      const applied = embeddingForm(next);
      setProvider(applied.provider);
      setOpenaiModel(applied.openaiModel);
      setOpenaiDimensions(applied.openaiDimensions);
      setOpenaiApiKeyEnv(applied.openaiApiKeyEnv);
      setOpenaiBaseUrl(applied.openaiBaseUrl);
      setLocalModel(applied.localModel);
      setLocalDimensions(applied.localDimensions);
      setHrrDimensions(applied.hrrDimensions);
      flashSaved();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }


  return (
    <section className="mt-10 card px-4 py-4">
      <h2 className="display-h3 mb-2">retrieval layer</h2>
      <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
        Hybrid retrieval (vector + BM25 keyword) drives both typed
        memory and skill discovery — one config, both surfaces. Switch
        the provider here; the change applies to the{" "}
        <strong>next agent spawn</strong> immediately. Running agents keep
        the setting they started with; restart them to pick up the change. The override is held{" "}
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
              hint="disabled = FTS5 keyword-only (the default). openai = remote semantic embeddings (recommended). local = fastembed-rs (requires --features local-embed at build time). hrr = pure-Rust deterministic bag-of-tokens (zero deps, weakest semantic recall)."
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
                  style={{ color: "var(--color-success-text)" }}
                >
                  swapped — affects next agent spawn
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
