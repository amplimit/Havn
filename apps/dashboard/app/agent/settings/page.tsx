"use client";

/**
 * `/agent/settings?id=` — per-agent configuration.
 *
 * Editable today: name, model, heartbeat interval and where its messages go. The PATCH endpoint
 * shallow-merges config keys, so saving "model" doesn't disturb
 * "heartbeat" or any per-agent policy override the user set elsewhere.
 *
 * Changes apply on the agent's NEXT spawn. Running runtime keeps its
 * frozen system prompt + Welcome-snapshot policy (spec §9.4 frozen-
 * prompt invariant). The page surfaces a small notice when the agent
 * is currently running so the user knows to expect that.
 */

import { cloneElement, isValidElement, useCallback, useEffect, useId, useRef, useState } from "react";
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
  type ChannelAccessView,
  type AccountAccess,
  type SenderRef,
  type MyDomainsView,
  type TimezoneView,
  type WebhookView,
  type SecretsView,
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
  const [heartbeatTo, setHeartbeatTo] = useState("last");
  const [linked, setLinked] = useState<string[]>([]);
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
        api.channels
          .links()
          .then((l) => {
            if (!cancelled) setLinked([...new Set(l.map((x) => x.channel))]);
          })
          .catch(() => {});
        const hb =
          (cfg.heartbeat as Record<string, unknown> | undefined) ?? {};
        const secs = typeof hb.interval_seconds === "number"
          ? hb.interval_seconds
          : null;
        setHeartbeatMin(secs ? String(Math.round(secs / 60)) : "");
        setHeartbeatTo(typeof hb.deliver === "string" ? hb.deliver : "last");
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
      const heartbeat: Record<string, unknown> = {};
      const savedHeartbeat =
        ((agent.config as Record<string, unknown>).heartbeat as Record<string, unknown> | undefined) ?? {};
      if (heartbeatMin.trim() && Number.isFinite(hbNum) && hbNum > 0) {
        heartbeat.interval_seconds = Math.round(hbNum * 60);
      } else if (heartbeatMin.trim() && typeof savedHeartbeat.interval_seconds === "number") {
        // An interval that is not a number of minutes leaves the saved one.
        heartbeat.interval_seconds = savedHeartbeat.interval_seconds;
      }
      if (heartbeatTo !== "last") heartbeat.deliver = heartbeatTo;
      if (Object.keys(heartbeat).length > 0) {
        config.heartbeat = heartbeat;
      } else if (!heartbeatMin.trim() || typeof savedHeartbeat.deliver === "string") {
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
    <div className="mx-auto max-w-5xl px-4 py-8 sm:px-8 sm:py-10 lg:grid lg:grid-cols-[11rem_minmax(0,1fr)] lg:gap-10">
      <SettingsNav />
      <div className="min-w-0 max-w-3xl">
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

      <Group id="basics">
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

        <Field
          label="Heartbeat messages go to"
          hint="Where a heartbeat that has something to say sends it. Each one is also kept in the Heartbeat chat of your web chat. A chat app needs your account there connected (see chat apps below)."
        >
          <select
            value={heartbeatTo}
            onChange={(e) => setHeartbeatTo(e.target.value)}
            className="input"
            style={{ maxWidth: 280 }}
          >
            <option value="last">Where you wrote last</option>
            <option value="web">The web chat only</option>
            {[...new Set([...linked, ...(["last", "web"].includes(heartbeatTo) ? [] : [heartbeatTo])])].map(
              (c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ),
            )}
          </select>
        </Field>

        <div className="flex items-center gap-3">
          <button
            type="submit"
            className="btn-primary"
            disabled={busy}
          >
            {busy ? "saving…" : "save"}
          </button>
          <span className="text-xs" style={{ color: "var(--color-success-text)" }} role="status">
            {saved ? "saved · the agent uses it from its next start" : ""}
          </span>
        </div>
      </form>
      <TimezoneEditor />
      <TitlesEditor agentId={id} />
      </Group>

      <Group id="prompt">
      <BootstrapEditor agentId={id} />
      <ScheduledTasks agentId={id} />
      <Webhooks agentId={id} />
      </Group>

      <Group id="chat-apps">
      <ChannelPairing agentId={id} />
      <BotAccess agentId={id} />
      </Group>

      <Group id="tools">
      <ApprovalsEditor agentId={id} />
      <AccessEditor agentId={id} />
      <WebAccessEditor agentId={id} />
      <WorkspaceAccessEditor agentId={id} />
      <CapabilitiesEditor agentId={id} />
      <McpEditor agentId={id} />
      </Group>

      <Group id="secrets">
      <SecretsEditor agentId={id} />
      </Group>

      <Group id="advanced">
      {embedding && (
        <RetrievalEditor embedding={embedding} onChange={(next) => setEmbedding(next)} />
      )}


      <section
        className="mt-10 card px-4 py-4"
        style={{ borderColor: "rgba(234, 34, 97, 0.3)" }}
      >
        <h3
          className="display-h3 mb-2"
          style={{ color: "var(--color-ruby)" }}
        >
          danger zone
        </h3>
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
      </Group>
      </div>
    </div>
  );
}

/** The page's groups of settings, in order, as its navigation lists them. */
const GROUPS = [
  { id: "basics", title: "Basics" },
  { id: "prompt", title: "Prompt and schedule" },
  { id: "chat-apps", title: "Chat apps" },
  { id: "tools", title: "Tools and permissions" },
  { id: "secrets", title: "Secrets" },
  { id: "advanced", title: "Advanced" },
] as const;

/** One group of settings, under its heading, which the navigation links to. */
function Group({ id, children }: { id: (typeof GROUPS)[number]["id"]; children: React.ReactNode }) {
  const title = GROUPS.find((g) => g.id === id)?.title ?? id;
  return (
    <section id={id} aria-labelledby={`${id}-title`} className="settings-group scroll-mt-28 pt-12 first-of-type:pt-0 md:scroll-mt-16 lg:scroll-mt-10">
      <h2
        id={`${id}-title`}
        className="border-b pb-2 text-xs font-semibold uppercase tracking-wider"
        style={{ color: "var(--color-body)", borderColor: "var(--color-border-default)" }}
      >
        {title}
      </h2>
      {children}
    </section>
  );
}

/**
 * The groups of the page as links: beside it on a wide screen, the one in
 * view marked; a row above it on a narrow one.
 */
function SettingsNav() {
  const [current, setCurrent] = useState<string>(GROUPS[0].id);
  // The group in view: the last whose heading has reached the top part of
  // the window; at the end of the page, the last group.
  useEffect(() => {
    let frame = 0;
    const look = () => {
      frame = 0;
      const line = window.innerHeight * 0.25;
      let found: string = GROUPS[0].id;
      for (const g of GROUPS) {
        const el = document.getElementById(g.id);
        if (el && el.getBoundingClientRect().top <= line) found = g.id;
      }
      const end = window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 2;
      setCurrent(end ? GROUPS[GROUPS.length - 1].id : found);
    };
    const onScroll = () => {
      if (!frame) frame = requestAnimationFrame(look);
    };
    onScroll();
    window.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", onScroll);
    // A group the address names is shown, and kept in view while what is
    // above it loads, until the reader moves the page or two seconds pass.
    const asked = window.location.hash.slice(1);
    let holding = GROUPS.some((g) => g.id === asked);
    const hold = () => {
      if (holding) document.getElementById(asked)?.scrollIntoView();
    };
    const release = () => {
      holding = false;
    };
    hold();
    const until = window.setTimeout(release, 2000);
    for (const kind of ["wheel", "touchstart", "keydown"]) window.addEventListener(kind, release, { passive: true });
    const grows = new ResizeObserver(() => {
      hold();
      onScroll();
    });
    grows.observe(document.body);
    return () => {
      cancelAnimationFrame(frame);
      window.clearTimeout(until);
      for (const kind of ["wheel", "touchstart", "keydown"]) window.removeEventListener(kind, release);
      grows.disconnect();
      window.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", onScroll);
    };
  }, []);
  const row = useRef<HTMLElement>(null);
  useEffect(() => {
    const nav = row.current;
    const link = nav?.querySelector<HTMLElement>('[aria-current="location"]');
    if (!nav || !link || nav.scrollWidth <= nav.clientWidth) return;
    // Along the row only: the page does not move.
    nav.scrollTo({ left: link.offsetLeft - nav.clientWidth / 2 + link.offsetWidth / 2 });
  }, [current]);
  return (
    <nav
      ref={row}
      aria-label="Settings"
      className="sticky top-15 z-10 -mx-4 md:top-0 mb-6 flex gap-1 overflow-x-auto border-b px-4 py-2 lg:top-10 lg:mx-0 lg:mb-0 lg:flex-col lg:self-start lg:overflow-visible lg:border-0 lg:px-0 lg:py-0"
      style={{ backgroundColor: "var(--color-surface)", borderColor: "var(--color-border-default)" }}
    >
      {GROUPS.map((g) => (
        <a
          key={g.id}
          href={`#${g.id}`}
          aria-current={current === g.id ? "location" : undefined}
          className="shrink-0 rounded px-2 py-1.5 text-sm transition-colors hover:bg-[rgba(83,58,253,0.05)]"
          style={{
            backgroundColor: current === g.id ? "rgba(83, 58, 253, 0.07)" : undefined,
            color: current === g.id ? "var(--color-stripe-purple)" : "var(--color-deep-navy)",
          }}
        >
          {g.title}
        </a>
      ))}
    </nav>
  );
}

/**
 * Bootstrap-file editor: SOUL.md / HEARTBEAT.md (spec §5.3, §9.4
 * layer 2). Each file gets its own textarea + save button
 * because (a) the two have different effect timing and the user
 * benefits from a clear "saved!" beat per file, (b) one big form
 * makes a typo in HEARTBEAT.md block a SOUL.md save the user really
 * wanted to land. Independent saves keep it simple.
 *
 * Effect timing on the hint lines mirrors spec §9.4: SOUL is
 * frozen into the system prompt at session start, so edits land on
 * the agent's next start. HEARTBEAT.md is the deliberate exception —
 * it's re-read on every tick (§9.6), so edits land on the next tick
 * (≤ 30 min by default). What USER.md held is public memory now
 * (issue #309): the memory page edits it.
 */
function BootstrapEditor({ agentId }: { agentId: string }) {
  const [loaded, setLoaded] = useState<BootstrapView | null>(null);
  const [system, setSystem] = useState("");
  const [heartbeat, setHeartbeat] = useState("");
  const [saving, setSaving] = useState<string | null>(null);
  const [savedSystem, flashSystem] = useFlash();
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

  async function save(field: "system" | "heartbeat", value: string) {
    setSaving(field);
    setError(null);
    try {
      const next = await api.bootstrap.put(agentId, { [field]: value });
      setLoaded(next);
      ({ system: flashSystem, heartbeat: flashHeartbeat })[field]();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(null);
    }
  }

  return (
    <section className="mt-10 card px-4 py-4">
      <h3 className="display-h3 mb-2">bootstrap files</h3>
      <p className="mb-4 text-sm" style={{ color: "var(--color-body)" }}>
        Two plain-text files in the agent&apos;s workspace that drive
        persona and heartbeat behaviour.
        Each saves independently. Empty
        body deletes the file.
      </p>
      <p className="mb-4 text-sm" role="note" style={{ color: "var(--color-warning-text)" }}>
        SOUL.md is in the agent&apos;s prompt in every chat: other people&apos;s it is open
        to, and groups&apos;, as well as yours. Write in it only what anyone who can talk to
        the bot may know.
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
      <div className="mb-5">
        <div className="mb-1.5 text-sm" style={{ color: "var(--color-label)", fontWeight: 400 }}>
          what the agent knows about you
        </div>
        <p className="text-xs" style={{ color: "var(--color-body)" }}>
          Facts about you are in the agent&apos;s memory, each private (in your own private
          chats only) or public (in every chat, other people&apos;s and groups&apos; too: your
          name, how to address you, your language). If the agent had a USER.md, it became
          a public memory under the key USER.md, and the old file is kept in its workspace as
          USER.md.bak, where the agent&apos;s file tools can still read it in any chat: delete it
          if what it says should not stay there.{" "}
          <Link href={`/agent/memory?id=${agentId}`} className="underline" style={{ color: "var(--color-purple)" }}>
            see and edit them on the memory page
          </Link>
        </p>
      </div>
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

/** How long an invite made here lasts, in hours, and how it is shown. */
const INVITE_HOURS: [number, string][] = [
  [1, "1 hour"],
  [24, "1 day"],
  [24 * 7, "7 days"],
  [24 * 30, "30 days"],
];

/**
 * Who can reach this agent's bots (issue #231): invite codes, the senders
 * let in, those turned away, and whether the owner is told about them.
 */
function BotAccess({ agentId }: { agentId: string }) {
  const [view, setView] = useState<ChannelAccessView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [made, setMade] = useState<PairingCode | null>(null);
  const [hours, setHours] = useState<Record<string, number>>({});
  const [typed, setTyped] = useState<Record<string, string>>({});
  useEffect(() => {
    let cancelled = false;
    api.channels.access(agentId).then(
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
  async function run(change: () => Promise<ChannelAccessView | void>) {
    setBusy(true);
    setError(null);
    try {
      const next = await change();
      if (next) setView(next);
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(false);
    }
  }
  async function invite(account: AccountAccess, lasts: number) {
    await run(async () => {
      setMade(await api.channels.invite(agentId, account.channel, account.account_id, lasts));
      return api.channels.access(agentId);
    });
  }
  async function setNotify(on: boolean) {
    await run(async () => {
      await api.agents.patch(agentId, { config: { notify_unpaired: on ? null : false } });
      return api.channels.access(agentId);
    });
  }
  const muted = { color: "var(--color-body)" };
  const when = (at: string) => new Date(at).toLocaleString();
  if (view && view.accounts.length === 0) return null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="bot-access-title">
      <h3 id="bot-access-title" className="display-h3 mb-2">
        who can reach the bot
      </h3>
      <p className="mb-3 text-sm" style={muted}>
        On an allowlist bot, only the people let in reach the agent: those in config.toml, those who
        sent an invite code, and those you let in here. Someone who writes without being let in is
        turned away and listed below.
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      {view && view.accounts.some((a) => a.dm_policy === "allowlist" || a.group_policy !== "open") && (
        <label className="mb-4 flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={view.notify_unpaired}
            disabled={busy}
            onChange={(e) => void setNotify(e.target.checked)}
          />
          Tell me in my connected chat with the bot when someone not let in writes, with a button to let
          them in (once a day per person, a few an hour at most; you must have started that bot)
        </label>
      )}
      {made && (
        <div className="mb-4 rounded border px-3 py-2 text-sm" role="status">
          <div>
            Invite code for {made.channel} · {made.account_id}, shown once:{" "}
            <span className="font-mono text-base tracking-wider">{made.code}</span>
          </div>
          {made.link && (
            <div className="wrap-anywhere">
              or the link{" "}
              <a className="underline" href={made.link} target="_blank" rel="noreferrer">
                {made.link}
              </a>
            </div>
          )}
          <div className="text-xs" style={muted}>
            Works once, until {when(made.expires_at)}. Send it to the person over a channel you trust.
          </div>
        </div>
      )}
      {(view?.accounts ?? []).map((account) => {
        const key = `${account.channel}|${account.account_id}`;
        const who = (sender_id: string, group = false): SenderRef => ({
          channel: account.channel,
          account_id: account.account_id,
          sender_id,
          group,
        });
        // A server let in already is not offered again.
        const inAlready = (space: string) =>
          account.allow_groups.includes(space) || account.groups.some((g) => g.group_id === space);
        const refusedBlock = (
          <>
            <h4 className="mt-3 text-xs uppercase" style={muted}>
              turned away recently
            </h4>
            {account.refused.length === 0 ? (
              <p className="text-sm" style={muted}>
                No one.
              </p>
            ) : (
              <ul className="flex flex-col gap-2 text-sm">
                {account.refused.map((r) => (
                  <li key={r.sender_id}>
                    {r.group && (
                      <span className="text-xs uppercase">{r.space ? "channel " : "group "}</span>
                    )}
                    {r.title && <span>{r.title} </span>}
                    <span className="font-mono">{r.sender_id}</span>{" "}
                    <span className="text-xs" style={muted}>
                      {r.count} {r.count === 1 ? "message" : "messages"}, last {when(r.last_at)}
                    </span>
                    {r.space && (
                      <div className="text-xs" style={muted}>
                        in the server {r.space.title || r.space.id}
                        {r.space.title && <span className="font-mono"> ({r.space.id})</span>}
                      </div>
                    )}
                    {r.said && <div className="text-xs wrap-anywhere">&ldquo;{r.said}&rdquo;</div>}
                    <div className="mt-1 flex flex-wrap gap-2">
                      {r.space && !inAlready(r.space.id) && (
                        <button
                          type="button"
                          className="btn-ghost"
                          disabled={busy || account.group_policy === "off"}
                          title={
                            account.group_policy === "off"
                              ? "turn groups on first"
                              : "Every channel of the server, those made later too, for everyone in it"
                          }
                          onClick={() => {
                            const space = r.space;
                            if (space) void run(() => api.channels.allow(agentId, who(space.id, true)));
                          }}
                        >
                          let in the whole server
                        </button>
                      )}
                      <button
                        type="button"
                        className="btn-ghost"
                        disabled={busy || (r.group && account.group_policy === "off")}
                        title={r.group && account.group_policy === "off" ? "turn groups on first" : undefined}
                        onClick={() => void run(() => api.channels.allow(agentId, who(r.sender_id, r.group)))}
                      >
                        {r.space ? "let in this channel" : "let in"}
                      </button>
                      <button
                        type="button"
                        className="btn-ghost"
                        disabled={busy}
                        onClick={() => void run(() => api.channels.forget(agentId, who(r.sender_id, r.group)))}
                      >
                        dismiss
                      </button>
                    </div>
                  </li>
                ))}
              </ul>
            )}
          </>
        );
        const groups = (
          <>
            <h4 className="mt-3 text-xs uppercase" style={muted}>
              groups (
              {account.group_policy === "open"
                ? "open: every group it is added to"
                : account.group_policy === "allowlist"
                  ? "those let in"
                  : "off: group messages are not used"}
              )
            </h4>
            {account.groups.some((g) => g.sees_all === false) && (
              <p className="text-sm" style={muted}>
                In the groups marked &quot;mentions do not reach it&quot;, the bot does not see every message, so a
                mention that is not a reply does not reach it: members reply to its messages or send it commands. On
                Telegram, turn its privacy mode off with BotFather (/setprivacy), then remove the bot from each such
                group and add it again.
              </p>
            )}
            {account.allow_groups.length === 0 && account.groups.length === 0 ? (
              <p className="text-sm" style={muted}>
                None yet.
              </p>
            ) : (
              <ul className="flex flex-col gap-1 text-sm">
                {account.allow_groups.map((g) => (
                  <li key={`toml-group-${g}`}>
                    <span className="font-mono">{g}</span>{" "}
                    <span className="text-xs" style={muted}>
                      in config.toml (allow_groups)
                    </span>
                  </li>
                ))}
                {account.groups.map((g) => (
                  <li key={`group-${g.group_id}`} className="flex flex-wrap items-center gap-2">
                    {g.space && (
                      <span className="pill pill-stopped" title="Every channel of the server, those made later too">
                        server
                      </span>
                    )}
                    <span className="font-mono">{g.group_id}</span>
                    {g.title && <span>{g.title}</span>}
                    {g.in_space && (
                      <span className="text-xs" style={muted}>
                        in the server {g.in_space.title || g.in_space.id}
                      </span>
                    )}
                    {g.sees_all === false && (
                      <span className="text-xs" style={muted}>
                        mentions do not reach it
                      </span>
                    )}
                    <span className="text-xs" style={muted}>
                      {g.source === "owner"
                        ? "let in by you"
                        : account.group_policy === "open"
                          ? "used while open"
                          : "used while open; no longer counts"}{" "}
                      since {when(g.since)}
                    </span>
                    <button
                      type="button"
                      className="btn-ghost"
                      disabled={busy}
                      onClick={() => void run(() => api.channels.remove(agentId, who(g.group_id, true)))}
                    >
                      remove
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </>
        );
        return (
          <div key={key} className="mb-6">
            <h4 className="mb-1 font-mono text-sm">
              {account.channel} · {account.account_id}
            </h4>
            <div className="mb-2 flex flex-wrap items-center gap-3 text-sm">
              <label className="flex items-center gap-1">
                private chats
                <select
                  className="input"
                  style={{ maxWidth: 200 }}
                  value={account.dm_policy}
                  disabled={busy || !view?.editable}
                  onChange={(e) => {
                    const next = e.target.value as AccountAccess["dm_policy"];
                    if (next === "open" && !window.confirm("Let anyone on the platform write to this bot?")) return;
                    void run(() => api.channels.policy(agentId, account, { dm_policy: next }));
                  }}
                >
                  <option value="allowlist">those let in</option>
                  <option value="open">anyone</option>
                </select>
              </label>
              <label className="flex items-center gap-1">
                groups
                <select
                  className="input"
                  style={{ maxWidth: 220 }}
                  value={account.group_policy}
                  disabled={busy || !view?.editable}
                  onChange={(e) => {
                    const next = e.target.value as AccountAccess["group_policy"];
                    if (next === "open" && !window.confirm("Let every group this bot is added to use it?")) return;
                    void run(() => api.channels.policy(agentId, account, { group_policy: next }));
                  }}
                >
                  <option value="off">off</option>
                  <option value="allowlist">those let in</option>
                  <option value="open">any group it is added to</option>
                </select>
              </label>
              {!view?.editable && (
                <span className="text-xs" style={muted}>
                  set in config.toml (trust_header is on)
                </span>
              )}
            </div>
            {account.dm_policy === "open" ? (
              <>
                <p className="text-sm" style={muted}>
                  Open: anyone can write to this bot.
                </p>
                {groups}
                {account.refused.length > 0 && refusedBlock}
              </>
            ) : (
              <>
                <div className="mb-2 flex flex-wrap items-center gap-2 text-sm">
                  <select
                    className="input"
                    style={{ maxWidth: 140 }}
                    value={hours[key] ?? 24}
                    onChange={(e) => {
                      const lasts = Number(e.target.value);
                      setHours((h) => ({ ...h, [key]: lasts }));
                    }}
                    aria-label="how long the invite lasts"
                  >
                    {INVITE_HOURS.map(([h, label]) => (
                      <option key={h} value={h}>
                        {label}
                      </option>
                    ))}
                  </select>
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={busy}
                    onClick={() => void invite(account, hours[key] ?? 24)}
                  >
                    make an invite code
                  </button>
                </div>
                <div className="mb-2 flex flex-wrap items-center gap-2 text-sm">
                  <input
                    className="input font-mono"
                    style={{ maxWidth: 220 }}
                    placeholder="sender id, such as tg:123"
                    value={typed[key] ?? ""}
                    onChange={(e) => {
                      const value = e.target.value;
                      setTyped((t) => ({ ...t, [key]: value }));
                    }}
                  />
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={busy || !(typed[key] ?? "").trim()}
                    onClick={() =>
                      void run(() => api.channels.allow(agentId, who((typed[key] ?? "").trim()))).then((ok) => {
                        if (ok) setTyped((t) => ({ ...t, [key]: "" }));
                      })
                    }
                  >
                    let in
                  </button>
                </div>

                {refusedBlock}

                <h5 className="mt-3 text-xs uppercase" style={muted}>
                  let in
                </h5>
                {account.allow_from.length === 0 && account.allowed.length === 0 ? (
                  <p className="text-sm" style={muted}>
                    No one yet.
                  </p>
                ) : (
                  <ul className="flex flex-col gap-1 text-sm">
                    {account.allow_from.map((s) => (
                      <li key={`toml-${s}`}>
                        <span className="font-mono">{s}</span>{" "}
                        <span className="text-xs" style={muted}>
                          in config.toml (allow_from)
                        </span>
                      </li>
                    ))}
                    {account.allowed.map((a) => (
                      <li key={a.sender_id} className="flex flex-wrap items-center gap-2">
                        <span className="font-mono">{a.sender_id}</span>
                        <span className="text-xs" style={muted}>
                          {a.yours ? "you, connected; " : a.connected ? "connected to a Havn account, " : ""}
                          {a.by_code ? "by a code" : "by you"} since {when(a.since)}
                        </span>
                        {!a.yours && (
                          <button
                            type="button"
                            className="btn-ghost"
                            disabled={busy}
                            onClick={() => void run(() => api.channels.remove(agentId, who(a.sender_id)))}
                          >
                            remove
                          </button>
                        )}
                      </li>
                    ))}
                  </ul>
                )}

                {groups}

                {account.invites.length > 0 && (
                  <>
                    <h5 className="mt-3 text-xs uppercase" style={muted}>
                      invite codes
                    </h5>
                    <ul className="flex flex-col gap-1 text-sm">
                      {account.invites.map((i) => (
                        <li key={i.id} className="flex flex-wrap items-center gap-2">
                          <span>{i.status}</span>
                          <span className="text-xs" style={muted}>
                            made {when(i.created_at)}
                            {i.status === "active" && `, until ${when(i.expires_at)}`}
                            {i.used_by && `, used by ${i.used_by}`}
                          </span>
                          {i.status === "active" && (
                            <button
                              type="button"
                              className="btn-ghost"
                              disabled={busy}
                              onClick={() =>
                                void run(() => api.channels.revokeInvite(agentId, i.id)).then((ok) => {
                                  if (ok) setMade((m) => (m?.id === i.id ? null : m));
                                })
                              }
                            >
                              revoke
                            </button>
                          )}
                        </li>
                      ))}
                    </ul>
                  </>
                )}
              </>
            )}
          </div>
        );
      })}
    </section>
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
      <h3 id="pairing-title" className="display-h3 mb-2">
        chat apps
      </h3>
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
      <h3 id="workspace-title" className="display-h3 mb-2">
        workspace browsing
      </h3>
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
      <h3 id="titles-title" className="display-h3 mb-2">
        chat titles
      </h3>
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

/** The scope of what is allowed in every conversation with the agent (issue #268). */
const EVERY_CONVERSATION = "*";

/** Tools that read, which can be allowed in every conversation: those `Review::of` in the runtime's approvals.rs reviews after they run. */
const READING_TOOLS = ["memory_search", "file_read", "file_find", "file_search"];

/**
 * What is always allowed in conversations not the owner's (issue #255): a
 * tool's results, or one exact call, each with its rule for what not to say.
 */
function AccessEditor({ agentId }: { agentId: string }) {
  const [allowed, setAllowed] = useState<Access[] | null>(null);
  const [rules, setRules] = useState<Record<string, string>>({});
  const [everyTool, setEveryTool] = useState(READING_TOOLS[0]);
  const [everyRule, setEveryRule] = useState("");
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
  /** Whether it was saved. */
  async function save(a: Pick<Access, "scope" | "tool" | "input">, rule: string | null): Promise<boolean> {
    setSaving(true);
    setError(null);
    try {
      show((await api.access.put(agentId, a, rule)).allowed);
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setSaving(false);
    }
  }
  const allowedEverywhere = (allowed ?? []).some(
    (a) => a.scope === EVERY_CONVERSATION && a.tool === everyTool && a.input === "",
  );
  const muted = { color: "var(--color-body)" };
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="access-title">
      <h3 id="access-title" className="display-h3 mb-2">
        tools in other people&apos;s conversations
      </h3>
      <p className="mb-3 text-sm" style={muted}>
        When someone other than you is talking to the agent, you decide before it runs or changes
        anything, and before it uses what it read (memory, files). These are always allowed, with your
        rule for what not to say there. Revoke one to be asked again.
      </p>
      <div className="mb-4 text-sm">
        <div className="mb-1">Allow a reading tool in every conversation with this agent:</div>
        <div className="flex flex-wrap items-start gap-2">
          <select
            className="input"
            style={{ maxWidth: 200 }}
            value={everyTool}
            onChange={(e) => setEveryTool(e.target.value)}
            aria-label="tool to allow everywhere"
          >
            {READING_TOOLS.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>
          <input
            className="input"
            style={{ flex: "1 1 220px" }}
            maxLength={1000}
            placeholder="what not to say (optional)"
            value={everyRule}
            onChange={(e) => setEveryRule(e.target.value)}
          />
          <button
            type="button"
            className="btn-ghost"
            disabled={saving || allowed === null || allowedEverywhere}
            onClick={() =>
              void save({ scope: EVERY_CONVERSATION, tool: everyTool, input: "" }, everyRule).then((saved) => {
                if (saved) setEveryRule("");
              })
            }
          >
            allow everywhere
          </button>
        </div>
        {allowedEverywhere && (
          <div className="mt-1 text-xs" style={muted}>
            Already allowed everywhere: change its rule below.
          </div>
        )}
      </div>
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
                  {a.scope === EVERY_CONVERSATION
                    ? "every conversation"
                    : a.channel
                      ? `${a.channel} · ${a.account} · ${a.sender}`
                      : a.scope}
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
      <h3 id="capabilities-title" className="display-h3 mb-2">
        working with other agents
      </h3>
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
      <h3 id="approvals-title" className="display-h3 mb-2">
        tools that need your approval
      </h3>
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
      <h3 id="tasks-title" className="display-h3 mb-2">
        scheduled tasks
      </h3>
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
                  title="run it now; a recurring task keeps its schedule, a one-shot is done"
                  onClick={() => act(() => api.cron.runNow(agentId, job.id))}
                >
                  run now
                </button>
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

/** Where a webhook's results go, as its select names them. */
const WEBHOOK_DELIVER: [string, string][] = [
  ["web", "its chat in the web chat"],
  ["last", "also where you wrote last"],
];

/** An agent's webhooks (issue #236): addresses outside systems call. */
function Webhooks({ agentId }: { agentId: string }) {
  const [hooks, setHooks] = useState<WebhookView[] | null>(null);
  const [channels, setChannels] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [made, setMade] = useState<WebhookView | null>(null);
  const [name, setName] = useState("");
  const [instructions, setInstructions] = useState("");
  const [deliver, setDeliver] = useState("web");
  const load = useCallback(() => {
    api.webhooks.list(agentId).then(setHooks, (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
    );
    api.channels.access(agentId).then(
      (view) => setChannels([...new Set(view.accounts.map((a) => a.channel))]),
      () => setChannels([]),
    );
  }, [agentId]);
  useEffect(load, [load]);
  async function act(work: () => Promise<unknown>) {
    setBusy(true);
    setError(null);
    try {
      await work();
      load();
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(false);
    }
  }
  const muted = { color: "var(--color-body)" };
  const options = [...WEBHOOK_DELIVER, ...channels.map((c): [string, string] => [c, `also to you on ${c}`])];
  const origin = typeof window === "undefined" ? "" : window.location.origin;
  if (!hooks) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  return (
    <section className="mt-10 card px-4 py-4" aria-labelledby="webhooks-title">
      <h3 id="webhooks-title" className="display-h3 mb-2">
        webhooks
      </h3>
      <p className="mb-3 text-sm" style={muted}>
        An address that other systems (an alert, a CI build, a form) call to have the agent act on what they send.
        What a call brings is treated as from someone other than you: reading tools and calls that run things wait
        for your approval, nothing is saved to your memory, and it cannot make tasks or ask other agents. Each
        webhook has its own chat in the web chat. It is served under the gateway&apos;s{" "}
        <span className="font-mono">listen</span> address (put it behind your proxy for outside callers); a caller
        sends the secret as <span className="font-mono">Authorization: Bearer</span> or{" "}
        <span className="font-mono">X-Havn-Token</span>, or signs the body with it (
        <span className="font-mono">X-Hub-Signature-256</span>, as GitHub does).
      </p>
      {error && (
        <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      {made && (
        <div className="mb-4 rounded border px-3 py-2 text-sm" role="status">
          <div>
            Webhook <span className="font-mono">{made.name}</span>: call{" "}
            <span className="font-mono wrap-anywhere">POST {made.path}</span>
          </div>
          <div>
            Secret, shown once: <span className="font-mono wrap-anywhere">{made.secret}</span>
          </div>
          <div className="text-xs" style={muted}>
            For example:{" "}
            <span className="font-mono wrap-anywhere">
              curl -X POST -H &quot;Authorization: Bearer {made.secret}&quot; --data &quot;hello&quot; http://
              {"<listen>"}
              {made.path}
            </span>
          </div>
        </div>
      )}
      <div className="mb-4 flex flex-col gap-2 text-sm">
        <div className="flex flex-wrap items-center gap-2">
          <input
            className="input"
            style={{ maxWidth: 200 }}
            placeholder="name, such as alerts"
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
          <select className="input" style={{ maxWidth: 240 }} value={deliver} onChange={(e) => setDeliver(e.target.value)}>
            {options.map(([value, label]) => (
              <option key={value} value={value}>
                {label}
              </option>
            ))}
          </select>
        </div>
        <textarea
          className="input"
          rows={2}
          placeholder="what to do with a call, such as: tell me which service is down and why"
          value={instructions}
          onChange={(e) => setInstructions(e.target.value)}
        />
        <div>
          <button
            type="button"
            className="btn-ghost"
            disabled={busy || !name.trim()}
            onClick={() =>
              void act(async () => {
                setMade(await api.webhooks.create(agentId, { name: name.trim(), instructions, deliver }));
              }).then((ok) => {
                if (ok) {
                  setName("");
                  setInstructions("");
                }
              })
            }
          >
            make a webhook
          </button>
        </div>
      </div>
      {hooks.length === 0 ? (
        <p className="text-sm" style={muted}>
          None yet.
        </p>
      ) : (
        <ul className="flex flex-col gap-3 text-sm">
          {hooks.map((hook) => (
            <li key={hook.id}>
              <div>
                <span className="font-mono">{hook.name}</span>{" "}
                <span className="text-xs" style={muted}>
                  POST {hook.path} ·{" "}
                  {hook.last_called_at
                    ? `called ${hook.calls} ${hook.calls === 1 ? "time" : "times"}, last ${new Date(hook.last_called_at).toLocaleString()}`
                    : "not called yet"}
                </span>
              </div>
              {hook.instructions && <div className="text-xs wrap-anywhere">{hook.instructions}</div>}
              <div className="mt-1 flex flex-wrap items-center gap-2">
                <select
                  className="input"
                  style={{ maxWidth: 240 }}
                  value={hook.deliver}
                  disabled={busy}
                  onChange={(e) =>
                    void act(() =>
                      api.webhooks.update(agentId, hook.id, { instructions: hook.instructions, deliver: e.target.value }),
                    )
                  }
                  aria-label="where its results go"
                >
                  {(options.some(([v]) => v === hook.deliver)
                    ? options
                    : [...options, [hook.deliver, hook.deliver] as [string, string]]
                  ).map(([value, label]) => (
                    <option key={value} value={value}>
                      {label}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className="btn-ghost"
                  disabled={busy}
                  onClick={() => {
                    if (window.confirm(`Delete the webhook ${hook.name}? Calls to it are refused from now on.`)) {
                      void act(() => api.webhooks.remove(agentId, hook.id)).then((ok) => {
                        if (ok && made?.id === hook.id) setMade(null);
                      });
                    }
                  }}
                >
                  delete
                </button>
              </div>
            </li>
          ))}
        </ul>
      )}
      {origin && (
        <p className="mt-3 text-xs" style={muted}>
          The path is on the gateway&apos;s API address, not this page&apos;s ({origin}).
        </p>
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
      <h3 id="timezone-title" className="display-h3 mb-2">
        your time zone
      </h3>
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
      <h3 id="web-access-title" className="display-h3 mb-2">
        web access
      </h3>
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
      <h3 className="display-h3 mb-2">MCP servers</h3>
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
                        style={{ fontSize: 12 }}
                      >
                        {cfg.binary}
                      </code>
                    </td>
                    <td
                      className="py-2 pr-2"
                      style={{ fontSize: 12, color: "var(--color-body)" }}
                    >
                      {cfg.args.length > 0 ? cfg.args.join(" ") : "—"}
                    </td>
                    <td
                      className="py-2 pr-2"
                      style={{ fontSize: 12, color: "var(--color-body)" }}
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

/** The shell environment as the editor shows it: one `NAME=value` a line. */
function envText(env: Record<string, string>): string {
  return Object.entries(env)
    .map(([name, value]) => `${name}=${value}`)
    .join("\n");
}

/** `NAME=value` lines as a shell environment; blank lines are skipped. */
function parseEnv(text: string): Record<string, string> | string {
  const env: Record<string, string> = {};
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    const at = line.indexOf("=");
    if (at <= 0) return `"${line}" is not NAME=value`;
    env[line.slice(0, at).trim()] = line.slice(at + 1).trim();
  }
  return env;
}

/**
 * The agent's secrets (issue #233): set and remove them, answer the ones the
 * agent asked for, and give shell commands variables. No value is ever
 * shown again, here or to the agent's model.
 */
function SecretsEditor({ agentId }: { agentId: string }) {
  const [view, setView] = useState<SecretsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // What is typed for each secret, by name; "" for the new one.
  const [values, setValues] = useState<Record<string, string>>({});
  // For a requested secret: give it as the variable the agent asked for.
  const [asVariable, setAsVariable] = useState<Record<string, boolean>>({});
  const [name, setName] = useState("");
  const [variable, setVariable] = useState("");
  const [envDraft, setEnvDraft] = useState("");
  const [savedEnv, flashEnv] = useFlash();
  useEffect(() => {
    let cancelled = false;
    api.secrets.list(agentId).then(
      (v) => {
        if (cancelled) return;
        setView(v);
        setEnvDraft(envText(v.shell_env));
      },
      (e: unknown) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      },
    );
    return () => {
      cancelled = true;
    };
  }, [agentId]);
  async function act(work: () => Promise<SecretsView>, savesEnv = false): Promise<boolean> {
    setBusy(true);
    setError(null);
    try {
      const before = view ? envText(view.shell_env) : "";
      const next = await work();
      setView(next);
      // Edits not saved yet stay; the saved environment replaces a draft
      // that had none.
      setEnvDraft((draft) => (savesEnv || draft === before ? envText(next.shell_env) : draft));
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(false);
    }
  }
  function typed(key: string, value: string) {
    setValues((all) => ({ ...all, [key]: value }));
  }
  async function set(secret: string, key: string, env?: string) {
    const value = values[key] ?? "";
    const ok = await act(() => api.secrets.set(agentId, secret, value, env));
    // The value is not kept in the page once sent.
    typed(key, "");
    return ok;
  }
  if (!view) return error ? <p className="mt-10 text-sm" role="alert">{error}</p> : null;
  const muted = { color: "var(--color-body)" };
  const requested = view.secrets.filter((s) => !s.set);
  const kept = view.secrets.filter((s) => s.set);
  const shellEnv = parseEnv(envDraft);
  const valueInput = (key: string, label: string) => (
    <input
      type="password"
      className="input"
      style={{ maxWidth: 320 }}
      autoComplete="new-password"
      spellCheck={false}
      placeholder="value"
      aria-label={label}
      value={values[key] ?? ""}
      disabled={busy || !view.encrypts}
      onChange={(e) => typed(key, e.target.value)}
    />
  );
  return (
    <>
      <section className="mt-10 card px-4 py-4" aria-labelledby="secrets-title">
        <h3 id="secrets-title" className="display-h3 mb-2">
          secrets for tools
        </h3>
        <p className="mb-2 text-sm" style={muted}>
          Tokens and keys the agent&apos;s tools use. Shell commands get a secret as the variable you give it
          below; an MCP server through <span className="font-mono">secret:&lt;name&gt;</span> in its env, from the
          agent&apos;s next start. Values are encrypted with the gateway&apos;s key and never shown again, here or
          in a chat: in what a tool prints, the agent&apos;s model sees{" "}
          <span className="font-mono">[secret:&lt;name&gt;]</span> in place of a value (values under{" "}
          {view.min_redacted_chars} characters are not replaced; spaces around a value are dropped). The agent can
          still use the value it is given, and send it where it can reach: give a secret only to an agent you trust
          with it.
        </p>
        {!view.encrypts && (
          <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
            Secrets cannot be set: the gateway has no HAVN_AGE_KEY.
          </p>
        )}
        {view.refused_mcp_servers.length > 0 && (
          <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
            Not started:{" "}
            <span className="font-mono">{view.refused_mcp_servers.join(", ")}</span>. Their env refers to secrets,
            and they run with settings your role supplies (which a team admin writes), not this agent&apos;s own, so
            they do not get your secrets. Define the server in this agent&apos;s MCP settings to give it one.
          </p>
        )}
        {error && (
          <p className="mb-2 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
            {error}
          </p>
        )}
        {requested.length > 0 && (
          <div className="mb-4">
            <h4 className="mb-1 text-sm font-semibold">asked for by the agent</h4>
            <p className="mb-2 text-xs" style={muted}>
              Anyone who can chat with the agent can lead it to ask. Enter a value only for a request you expected.
            </p>
            <ul className="flex flex-col gap-3 text-sm">
              {requested.map((secret) => {
                const asked = secret.requested;
                const env = asked?.env;
                const give = env !== undefined && (asVariable[secret.name] ?? false);
                return (
                  <li key={secret.name}>
                    <div>
                      <span className="font-mono">{secret.name}</span>{" "}
                      <span className="text-xs" style={muted}>
                        asked {asked ? new Date(asked.at).toLocaleString() : ""}
                        {asked && asked.again > 0
                          ? ` · asked again ${asked.again} ${asked.again === 1 ? "time" : "times"}`
                          : ""}
                      </span>
                    </div>
                    {asked && (
                      <div
                        className="text-xs"
                        style={asked.by_owner ? muted : { color: "var(--color-ruby)" }}
                      >
                        {asked.by_owner
                          ? `In your own ${asked.asked_in ?? "chat"}.`
                          : asked.asked_by || asked.asked_in
                            ? `While answering someone else: ${asked.asked_by ?? "unknown"}${
                                asked.asked_in ? `, in ${asked.asked_in}` : ""
                              }.`
                            : "In a scheduled task or a heartbeat, not in a chat."}
                      </div>
                    )}
                    {asked && <div className="text-xs wrap-anywhere">The agent&apos;s reason: {asked.reason}</div>}
                    <div className="mt-1 flex flex-wrap items-center gap-2">
                      {valueInput(secret.name, `value of ${secret.name}`)}
                      {env !== undefined && (
                        <label className="inline-flex items-center gap-1 text-xs">
                          <input
                            type="checkbox"
                            checked={give}
                            disabled={busy}
                            onChange={(e) => setAsVariable((all) => ({ ...all, [secret.name]: e.target.checked }))}
                          />
                          shell commands get it as <span className="font-mono">${env}</span>
                        </label>
                      )}
                      <button
                        type="button"
                        className="btn-ghost"
                        disabled={busy || !view.encrypts || !(values[secret.name] ?? "").trim()}
                        onClick={() => void set(secret.name, secret.name, give ? env : undefined)}
                      >
                        set
                      </button>
                      <button
                        type="button"
                        className="btn-ghost"
                        disabled={busy}
                        onClick={() => void act(() => api.secrets.remove(agentId, secret.name))}
                      >
                        dismiss
                      </button>
                    </div>
                  </li>
                );
              })}
            </ul>
          </div>
        )}
        {kept.length === 0 ? (
          <p className="mb-3 text-sm" style={muted}>
            No secrets set.
          </p>
        ) : (
          <ul className="mb-4 flex flex-col gap-3 text-sm">
            {kept.map((secret) => (
              <li key={secret.name}>
                <div>
                  <span className="font-mono">{secret.name}</span>{" "}
                  <span className="text-xs" style={muted}>
                    set {new Date(secret.updated_at).toLocaleString()}
                    {secret.variables.length > 0
                      ? ` · shell commands get it as ${secret.variables.map((v) => `$${v}`).join(", ")}`
                      : " · no shell variable gets it"}
                  </span>
                </div>
                <div className="mt-1 flex flex-wrap items-center gap-2">
                  {valueInput(secret.name, `new value of ${secret.name}`)}
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={busy || !view.encrypts || !(values[secret.name] ?? "").trim()}
                    onClick={() => void set(secret.name, secret.name)}
                  >
                    replace
                  </button>
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={busy}
                    onClick={() => {
                      if (
                        window.confirm(
                          `Remove the secret ${secret.name}? Shell commands lose it at once, MCP servers at the agent's next start. What the agent already did with it cannot be undone.`,
                        )
                      ) {
                        void act(() => api.secrets.remove(agentId, secret.name));
                      }
                    }}
                  >
                    remove
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
        <div className="flex flex-wrap items-center gap-2 text-sm">
          <input
            className="input"
            style={{ maxWidth: 200 }}
            placeholder="name, such as github"
            aria-label="name of a new secret"
            value={name}
            disabled={busy || !view.encrypts}
            onChange={(e) => setName(e.target.value)}
          />
          {valueInput("", "value of the new secret")}
          <input
            className="input font-mono"
            style={{ maxWidth: 260 }}
            placeholder="variable, such as GH_TOKEN"
            aria-label="shell variable for the new secret (optional)"
            value={variable}
            disabled={busy || !view.encrypts}
            onChange={(e) => setVariable(e.target.value)}
          />
          <button
            type="button"
            className="btn-ghost"
            disabled={busy || !view.encrypts || !name.trim() || !(values[""] ?? "").trim()}
            onClick={() =>
              void set(name.trim(), "", variable.trim() || undefined).then((ok) => {
                if (ok) {
                  setName("");
                  setVariable("");
                }
              })
            }
          >
            set
          </button>
        </div>
        <p className="mt-1 text-xs" style={muted}>
          A name has letters, digits, <span className="font-mono">_</span>, <span className="font-mono">-</span> and{" "}
          <span className="font-mono">.</span>. The variable is optional; you can also map one below.
        </p>
      </section>

      <section className="mt-10 card px-4 py-4" aria-labelledby="shell-env-title">
        <h3 id="shell-env-title" className="display-h3 mb-2">
          shell environment
        </h3>
        <p className="mb-3 text-sm" style={muted}>
          Variables every shell command of the agent gets, one <span className="font-mono">NAME=value</span> a line.
          A value <span className="font-mono">secret:&lt;name&gt;</span> is that secret; one not set leaves the
          variable out. Other values are kept as you write them, in the agent&apos;s settings, so put anything
          sensitive in a secret. Applies to the next command, also of a running agent.
        </p>
        <textarea
          className="input w-full font-mono text-xs"
          rows={4}
          value={envDraft}
          disabled={busy}
          spellCheck={false}
          placeholder="GH_TOKEN=secret:github"
          onChange={(e) => setEnvDraft(e.target.value)}
          aria-label="shell environment"
        />
        {typeof shellEnv === "string" && (
          <p className="mt-1 text-xs" role="alert" style={{ color: "var(--color-ruby)" }}>
            {shellEnv}
          </p>
        )}
        <div className="mt-2 flex items-center gap-3">
          <button
            type="button"
            className="btn-ghost"
            disabled={busy || typeof shellEnv === "string" || envDraft === envText(view.shell_env)}
            onClick={() => {
              if (typeof shellEnv === "string") return;
              void act(() => api.secrets.setShellEnv(agentId, shellEnv), true).then((ok) => {
                if (ok) flashEnv();
              });
            }}
          >
            save
          </button>
          <span className="text-xs" style={{ color: "var(--color-success-text)" }} role="status">
            {savedEnv ? "saved" : ""}
          </span>
        </div>
      </section>
    </>
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
      <h3 className="display-h3 mb-2">retrieval layer</h3>
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
  const hintId = useId();
  // The control is described by the hint beside it.
  const control = isValidElement<{ "aria-describedby"?: string }>(children)
    ? cloneElement(children, { "aria-describedby": hintId })
    : children;
  return (
    <div className="grid gap-1.5">
      <label className="grid gap-1.5">
        <span
          className="text-sm"
          style={{ color: "var(--color-label)", fontWeight: 400 }}
        >
          {label}
        </span>
        {control}
      </label>
      <Hint id={hintId} text={hint} />
    </div>
  );
}

/** A setting's description: one line, and all of it on "more" when it is longer. */
function Hint({ id, text }: { id: string; text: string }) {
  const [all, setAll] = useState(false);
  const [long, setLong] = useState(false);
  const line = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    const el = line.current;
    if (!el) return;
    const measure = () => {
      if (!all) setLong(el.scrollHeight > el.clientHeight + 1);
    };
    const watch = new ResizeObserver(measure);
    watch.observe(el);
    return () => watch.disconnect();
  }, [all, text]);
  return (
    <span className="flex items-start gap-1 text-xs" style={{ color: "var(--color-body)" }}>
      <span ref={line} id={id} className={all ? undefined : "line-clamp-1"}>
        {text}
      </span>
      {long && (
        <button
          type="button"
          className="shrink-0 underline decoration-dotted underline-offset-2"
          aria-expanded={all}
          onClick={(e) => {
            e.preventDefault();
            setAll(!all);
          }}
        >
          {all ? "less" : "more"}
        </button>
      )}
    </span>
  );
}
