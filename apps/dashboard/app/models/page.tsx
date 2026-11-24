"use client";

import { useEffect, useRef, useState } from "react";
import { MoreHorizontal } from "lucide-react";
import {
  ApiError,
  LLM_PROVIDERS as PROVIDERS,
  api,
  endpointOf,
  modelName,
  thinkingChoices,
  type ModelCheck,
  type ModelEntry,
  type ModelsView,
  type ThinkingSpec,
} from "@/lib/api";
import { SkeletonList } from "@/components/SkeletonList";

type Price = NonNullable<ModelEntry["price"]>;

// As the usage page shows dollars: with a point and no grouping, whatever
// the locale, so what it shows can be typed back.
function perMillion(n: number): string {
  return `$${n.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 6, useGrouping: false })}`;
}

// A price as typed: decimal digits, with a point or a comma; nothing a
// `Number` would read otherwise (0x10, 0b11). "1,500" could be either a
// thousand and a half or one and a half, so it is refused.
function parsePrice(typed: string): number | null {
  const trimmed = typed.trim();
  if (/^[1-9]\d{0,2},\d{3}$/.test(trimmed)) return null;
  const t = trimmed.replace(/^(\d*),(\d+)$/, "$1.$2");
  if (!/^(\d+\.?\d*|\.\d+)(e[+-]?\d+)?$/i.test(t)) return null;
  const n = Number(t);
  return Number.isFinite(n) ? n : null;
}

export default function ModelsPage() {
  const [view, setView] = useState<ModelsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [provider, setProvider] = useState("anthropic");
  const [name, setName] = useState("");
  const [label, setLabel] = useState("");
  const [maxOutput, setMaxOutput] = useState("");
  const [makeDefault, setMakeDefault] = useState(false);
  const [vision, setVision] = useState<"" | "yes" | "no">("");

  useEffect(() => {
    api.models
      .get()
      .then(setView)
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  async function save(
    defaultModel: string | null,
    models: ModelEntry[],
    titleModel: string | null = view?.title_model ?? null,
  ) {
    setBusy(true);
    setError(null);
    try {
      setView(
        await api.models.put({
          version: view?.version ?? "",
          default_model: defaultModel,
          // A model no longer listed no longer titles chats.
          title_model: titleModel && models.some((m) => m.id === titleModel) ? titleModel : null,
          models,
        }),
      );
      return true;
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        setError("The list was changed elsewhere; this is the current list. Try again.");
        api.models.get().then(setView).catch(() => {});
      } else {
        setError(e instanceof Error ? e.message : String(e));
      }
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function onAdd(e: React.FormEvent) {
    e.preventDefault();
    if (!view) return;
    const id = `${provider}:${name.trim()}`;
    const entry: ModelEntry = { id };
    if (label.trim()) entry.label = label.trim();
    if (vision) entry.vision = vision === "yes";
    if (maxOutput.trim()) {
      const n = Number(maxOutput);
      if (!Number.isInteger(n) || n <= 0) {
        setError("max output tokens must be a whole number above 0, or blank");
        return;
      }
      entry.max_output_tokens = n;
    }
    const saved = await save(makeDefault ? id : view.default_model, [
      ...view.models,
      entry,
    ]);
    if (saved) {
      setName("");
      setLabel("");
      setMaxOutput("");
      setMakeDefault(false);
      setVision("");
    }
  }

  function remove(id: string) {
    if (!view) return;
    save(
      view.default_model === id ? null : view.default_model,
      view.models.filter((m) => m.id !== id),
    );
  }

  function move(index: number, by: number) {
    if (!view) return;
    const models = [...view.models];
    const [m] = models.splice(index, 1);
    models.splice(index + by, 0, m);
    save(view.default_model, models);
  }

  const editable = view?.editable ?? false;
  const endpoints = view?.endpoints ?? [];

  return (
    <div className="mx-auto max-w-4xl px-4 py-8 sm:px-8 sm:py-12">
      <header className="mb-8">
        <h1 className="display-h1">models</h1>
        <p className="mt-2" style={{ color: "var(--color-body)", fontSize: 18 }}>
          The models agents may use, as provider:model. Agents without a model
          of their own use the default: the one marked below, or the first.
          The list lives in <code className="font-mono">[[llm.models]]</code>{" "}
          of the gateway&apos;s config.toml; changes here are written there.
          A local or other compatible server, set up in{" "}
          <code className="font-mono">[llm.endpoints]</code> there, is a provider
          too, and needs no key unless it asks for one.
          A model&apos;s price, in dollars per million tokens, gives what its
          calls cost on the usage page.
        </p>
        {view && editable && view.default_model && (
          <p className="mt-3 text-sm" style={{ color: "var(--color-body)" }}>
            The default is set to {view.default_model}.{" "}
            <button
              type="button"
              className="underline"
              disabled={busy}
              onClick={() => save(null, view.models)}
            >
              Use the first entry instead
            </button>
          </p>
        )}
        {view && editable && !view.default_model && view.models.length > 1 && (
          <p className="mt-3 text-sm" style={{ color: "var(--color-body)" }}>
            No default is set, so moving another entry to the top changes the
            model of every agent that does not choose its own.
          </p>
        )}
        {view && !editable && (
          <p className="mt-3 text-sm" style={{ color: "var(--color-ruby)" }}>
            With trust_header on, the list can only be changed in config.toml.
          </p>
        )}
        {view && view.models.length > 0 && (
          <label className="mt-3 flex flex-wrap items-center gap-2 text-sm" style={{ color: "var(--color-body)" }}>
            Chat titles are written by
            <select
              className="input"
              value={view.title_model ?? ""}
              disabled={busy || !editable}
              onChange={(e) => void save(view.default_model, view.models, e.target.value || null)}
              aria-label="title model"
            >
              <option value="">each agent&apos;s own model</option>
              {view.models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.label ? `${m.label} (${m.id})` : m.id}
                </option>
              ))}
            </select>
            <span>
              (<code className="font-mono">[llm] title_model</code>; a small, cheap model
              does well. Agents use it from their next start. A team&apos;s policy can
              name another, or turn titles off.)
            </span>
          </label>
        )}
      </header>

      {error && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{ color: "var(--color-ruby)", borderColor: "rgba(234, 34, 97, 0.3)" }}
        >
          {error}
        </div>
      )}

      {editable && (
        <div className="card-elevated mb-8 p-6">
          <h2 className="display-h3 mb-4">add a model</h2>
          <form onSubmit={onAdd} className="grid grid-cols-1 gap-4 md:grid-cols-4">
            <div>
              <label className="label" htmlFor="provider">provider</label>
              <select
                id="provider"
                value={provider}
                onChange={(e) => setProvider(e.target.value)}
                className="input"
              >
                {endpoints.length === 0 ? (
                  PROVIDERS.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))
                ) : (
                  <>
                    <optgroup label="built-in">
                      {PROVIDERS.map((p) => (
                        <option key={p} value={p}>
                          {p}
                        </option>
                      ))}
                    </optgroup>
                    <optgroup label="[llm.endpoints]">
                      {endpoints.map((e) => (
                        <option key={e.name} value={e.name}>
                          {e.name} ({e.api} API)
                        </option>
                      ))}
                    </optgroup>
                  </>
                )}
              </select>
            </div>
            <div className="md:col-span-3">
              <label className="label" htmlFor="model-name">model name</label>
              <input
                id="model-name"
                required
                spellCheck={false}
                placeholder={
                  provider === "openrouter"
                    ? "deepseek/deepseek-v4.1-flash-1017"
                    : endpointOf(view, provider)
                      ? "the name the server gives it, e.g. qwen3:8b"
                      : "claude-sonnet-4-6"
                }
                value={name}
                onChange={(e) => setName(e.target.value)}
                className="input font-mono"
              />
            </div>
            <div className="md:col-span-2">
              <label className="label" htmlFor="model-label">label (optional, shown in place of the id)</label>
              <input
                id="model-label"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                className="input"
              />
            </div>
            <div>
              <label className="label" htmlFor="max-output">max output tokens</label>
              <input
                id="max-output"
                type="number"
                min={1}
                step={1}
                placeholder="model's own"
                value={maxOutput}
                onChange={(e) => setMaxOutput(e.target.value)}
                className="input tabular"
              />
            </div>
            <div title="Whether pictures sent in a chat are shown to this model. Tried, a model that refuses them is sent a note in their place.">
              <label className="label" htmlFor="model-vision">images</label>
              <select
                id="model-vision"
                className="input"
                value={vision}
                onChange={(e) => setVision(e.target.value as "" | "yes" | "no")}
              >
                <option value="">tried</option>
                <option value="yes">shown</option>
                <option value="no">never shown</option>
              </select>
            </div>
            <div className="flex flex-wrap items-center justify-between gap-3 md:col-span-4">
              <label className="flex items-center gap-2 text-sm" style={{ color: "var(--color-label)" }}>
                <input
                  type="checkbox"
                  checked={makeDefault}
                  onChange={(e) => setMakeDefault(e.target.checked)}
                />
                make it the default model
              </label>
              <button type="submit" className="btn-primary" disabled={busy || !name.trim()}>
                {busy ? "saving…" : "add model"}
              </button>
            </div>
          </form>
        </div>
      )}

      <h2 className="display-h2 mb-4">listed ({view?.models.length ?? "…"})</h2>

      {view === null && !error && (
        <SkeletonList />
      )}

      {view?.models.length === 0 && (
        <div className="card px-5 py-10 text-center" style={{ color: "var(--color-body)" }}>
          no models yet — agents cannot chat until one is added.
        </div>
      )}

      <ul className="grid gap-3">
        {view?.models.map((m, i) => (
          <li key={m.id} className="card flex flex-wrap items-center justify-between gap-3 px-5 py-4">
            <div className="min-w-[14rem] flex-1">
              <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                <span className="display-h3 wrap-anywhere">{modelName(m.id, m.label)}</span>
                {view.effective_default === m.id && (
                  <span className="pill pill-running">default</span>
                )}
                {m.max_output_tokens && (
                  <span className="pill pill-stopped tabular">
                    max {m.max_output_tokens.toLocaleString()} output tokens
                  </span>
                )}
                {m.vision === true && <span className="pill pill-stopped">sees images</span>}
                {m.vision === false && <span className="pill pill-stopped">no images</span>}
                {m.thinking && (
                  <span className="pill pill-stopped">
                    thinks: {thinkingChoices(m.thinking).join(" / ")}
                  </span>
                )}
                {m.price && (
                  <span
                    className="pill pill-stopped tabular"
                    title={`dollars per million tokens; cache reads ${perMillion(m.price.cache_read ?? m.price.input)}, cache writes ${perMillion(m.price.cache_write ?? m.price.input)}`}
                  >
                    {perMillion(m.price.input)} in / {perMillion(m.price.output)} out
                  </span>
                )}
              </div>
              <p className="mt-1 font-mono text-xs wrap-anywhere" style={{ color: "var(--color-body)" }}>
                {m.id}
              </p>
              <ThinkingEditor
                entry={m}
                api={endpointOf(view, m.id.split(":")[0])?.api ?? m.id.split(":")[0]}
                editable={editable && !busy}
                onSave={(thinking) =>
                  save(
                    view.default_model,
                    view.models.map((x) => (x.id === m.id ? { ...x, thinking } : x)),
                  )
                }
              />
              <PriceEditor
                entry={m}
                editable={editable}
                busy={busy}
                onSave={(price) =>
                  save(
                    view.default_model,
                    view.models.map((x) => (x.id === m.id ? { ...x, price } : x)),
                  )
                }
              />
            </div>
            {editable && (
              <div className="flex items-center gap-1 self-start">
                {view.effective_default !== m.id && (
                  <button
                    type="button"
                    className="tool-button text-sm"
                    disabled={busy}
                    onClick={() => save(m.id, view.models)}
                  >
                    make default
                  </button>
                )}
                <MoreMenu label={`more for ${m.id}`}>
                  <button
                    type="button"
                    className="menu-item"
                    disabled={busy || i === 0}
                    onClick={() => move(i, -1)}
                  >
                    move up
                  </button>
                  <button
                    type="button"
                    className="menu-item"
                    disabled={busy || i === view.models.length - 1}
                    onClick={() => move(i, 1)}
                  >
                    move down
                  </button>
                  <button
                    type="button"
                    className="menu-item"
                    style={{ color: "var(--color-ruby)" }}
                    disabled={busy}
                    onClick={() => remove(m.id)}
                  >
                    remove
                  </button>
                </MoreMenu>
              </div>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

/**
 * Actions used now and then, behind a button: a menu that closes on a
 * choice, on Escape, or on a click elsewhere.
 */
function MoreMenu({ label, children }: { label: string; children: React.ReactNode }) {
  const menu = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    const close = (e: Event) => {
      const el = menu.current;
      if (!el?.open) return;
      if (e instanceof KeyboardEvent) {
        if (e.key !== "Escape") return;
        el.open = false;
        el.querySelector("summary")?.focus();
      } else if (!el.contains(e.target as Node)) {
        el.open = false;
      }
    };
    document.addEventListener("click", close);
    document.addEventListener("keydown", close);
    return () => {
      document.removeEventListener("click", close);
      document.removeEventListener("keydown", close);
    };
  }, []);
  return (
    <details ref={menu} className="relative">
      <summary className="tool-button list-none [&::-webkit-details-marker]:hidden" aria-label={label} title="more">
        <MoreHorizontal size={18} aria-hidden="true" />
      </summary>
      <div
        className="menu enter absolute right-0 z-20 mt-1 flex min-w-40 flex-col py-1"
        onClick={(e) => {
          if ((e.target as HTMLElement).closest("button")) {
            menu.current?.removeAttribute("open");
            menu.current?.querySelector("summary")?.focus();
          }
        }}
      >
        {children}
      </div>
    </details>
  );
}

/**
 * How a model thinks, as its admin sets it: the style its provider takes
 * (on/off, levels, or token budgets), the choices, the default; and a
 * check that sends it a very short request with it.
 */
function ThinkingEditor({
  entry,
  api: speaks,
  editable,
  onSave,
}: {
  entry: ModelEntry;
  /** The built-in provider whose API the model's provider speaks. */
  api: string;
  editable: boolean;
  onSave: (thinking: ThinkingSpec | undefined) => Promise<boolean>;
}) {
  const spec = entry.thinking;
  const [open, setOpen] = useState(false);
  const [style, setStyle] = useState<"none" | ThinkingSpec["style"]>(spec?.style ?? "none");
  const [list, setList] = useState(
    spec?.style === "budget" ? (spec.budgets ?? []).join(", ") : (spec?.levels ?? []).join(", "),
  );
  const [defaultChoice, setDefaultChoice] = useState(spec?.default ?? "");
  const [mandatory, setMandatory] = useState(spec?.mandatory ?? false);
  const [check, setCheck] = useState<ModelCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [error, setError] = useState<string | null>(null);

  function draft(): ThinkingSpec | undefined {
    if (style === "none") return undefined;
    const items = list.split(/[\s,]+/).filter(Boolean);
    const out: ThinkingSpec = { style };
    if (style === "effort") out.levels = items;
    if (style === "budget") out.budgets = items.map(Number);
    if (defaultChoice) out.default = defaultChoice;
    if (mandatory) out.mandatory = true;
    return out;
  }

  async function runCheck() {
    setChecking(true);
    setError(null);
    setCheck(null);
    try {
      setCheck(await api.models.check({ ...entry, thinking: draft() }));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setChecking(false);
    }
  }

  const muted = { color: "var(--color-body)" };
  if (!open) {
    return (
      <button type="button" className="mt-2 mr-3 text-xs underline" style={muted} onClick={() => setOpen(true)}>
        {spec ? "thinking settings" : "set up thinking"}
      </button>
    );
  }
  const current = draft();
  return (
    <div className="mt-3 grid gap-2 text-sm" style={muted}>
      <label className="flex flex-wrap items-center gap-2">
        thinking
        <select
          className="input"
          value={style}
          disabled={!editable}
          onChange={(e) => setStyle(e.target.value as typeof style)}
          aria-label={`thinking of ${entry.id}`}
        >
          <option value="none">none: not asked to think</option>
          <option value="toggle">on or off</option>
          <option value="effort">by level</option>
          {speaks !== "openai" && <option value="budget">by token budget</option>}
        </select>
      </label>
      {(style === "effort" || style === "budget") && (
        <label className="flex flex-wrap items-center gap-2">
          {style === "effort" ? "levels it takes" : "budgets offered (tokens)"}
          <input
            className="input"
            value={list}
            disabled={!editable}
            placeholder={style === "effort" ? "low, medium, high" : "1024, 4096, 16384"}
            onChange={(e) => setList(e.target.value)}
          />
        </label>
      )}
      {current && (
        <label className="flex flex-wrap items-center gap-2">
          default for a chat
          <select
            className="input"
            value={defaultChoice}
            disabled={!editable}
            onChange={(e) => setDefaultChoice(e.target.value)}
          >
            <option value="">{mandatory ? "the first" : "off"}</option>
            {thinkingChoices(current)
              .filter((c) => c !== "off")
              .map((c) => (
                <option key={c} value={c}>
                  {c}
                </option>
              ))}
          </select>
        </label>
      )}
      {current && (
        <label className="flex items-center gap-2">
          <input type="checkbox" checked={mandatory} disabled={!editable} onChange={(e) => setMandatory(e.target.checked)} />
          it always thinks (no off)
        </label>
      )}
      <div className="flex flex-wrap items-center gap-2">
        <button
          type="button"
          className="btn-ghost"
          disabled={!editable}
          onClick={() => void onSave(current).then((ok) => ok && setOpen(false))}
        >
          save
        </button>
        <button type="button" className="btn-ghost" disabled={checking} onClick={() => void runCheck()}>
          {checking ? "checking…" : "check"}
        </button>
        <button type="button" className="text-xs underline" onClick={() => setOpen(false)}>
          close
        </button>
      </div>
      {error && <p role="alert" style={{ color: "var(--color-ruby)" }}>{error}</p>}
      {check && (
        <p role="status" className="text-xs">
          {check.outcome === "reasoning" &&
            `It answered and showed its reasoning: "${check.reasoning}" Answer: ${check.reply}`}
          {check.outcome === "no_reasoning" &&
            `It answered without showing reasoning: it may not think as set, or keep its reasoning to itself. Answer: ${check.reply}`}
          {check.outcome === "error" && `The provider refused: ${check.error}`}
        </p>
      )}
    </div>
  );
}

/**
 * What a model's tokens cost, in US dollars per million (issue #282): input
 * and output, and its prompt cache's reads and writes, which cost what
 * input does when left blank. All blank, it has no price.
 */
function PriceEditor({
  entry,
  editable,
  busy,
  onSave,
}: {
  entry: ModelEntry;
  /** Whether the list may be changed here (not with trust_header). */
  editable: boolean;
  busy: boolean;
  onSave: (price: Price | undefined) => Promise<boolean>;
}) {
  const price = entry.price;
  const shown = (n: number | undefined) => (n === undefined ? "" : String(n));
  const [open, setOpen] = useState(false);
  const [input, setInput] = useState("");
  const [output, setOutput] = useState("");
  const [cacheRead, setCacheRead] = useState("");
  const [cacheWrite, setCacheWrite] = useState("");
  const [error, setError] = useState<string | null>(null);
  const toggle = useRef<HTMLButtonElement>(null);
  const firstField = useRef<HTMLInputElement>(null);
  // Where focus goes once React has rendered: back to the link on close,
  // back to the first field after a save that failed.
  const focusToggle = useRef(false);
  const focusFirst = useRef(false);

  useEffect(() => {
    if (!open && focusToggle.current) {
      focusToggle.current = false;
      toggle.current?.focus();
    }
  }, [open]);

  useEffect(() => {
    if (open && !busy && focusFirst.current) {
      focusFirst.current = false;
      firstField.current?.focus();
    }
  }, [open, busy]);

  function draft(): Price | undefined | string {
    const fields = [input, output, cacheRead, cacheWrite].map((f) => f.trim());
    if (fields.every((f) => f === "")) return undefined;
    if (fields[0] === "" || fields[1] === "") return "give both the input and the output price, or clear all four";
    const numbers = fields.map((f) => (f === "" ? undefined : parsePrice(f)));
    if (numbers.some((n) => n === null)) {
      return "a price is a number of dollars, 0 or more, such as 0.15 (no thousands separators)";
    }
    const [i, o, r, w] = numbers as Array<number | undefined>;
    const out: Price = { input: i as number, output: o as number };
    if (r !== undefined) out.cache_read = r;
    if (w !== undefined) out.cache_write = w;
    return out;
  }

  function reset() {
    setInput(shown(price?.input));
    setOutput(shown(price?.output));
    setCacheRead(shown(price?.cache_read));
    setCacheWrite(shown(price?.cache_write));
    setError(null);
  }

  function close() {
    focusToggle.current = true;
    setOpen(false);
  }

  async function submit(next: Price | undefined | string) {
    if (typeof next === "string") {
      setError(next);
      return;
    }
    setError(null);
    if (await onSave(next)) {
      close();
    } else {
      setError("Not saved (see the message at the top of the page). Reset shows the price saved now.");
      focusFirst.current = true;
    }
  }

  const muted = { color: "var(--color-body)" };
  // Where only config.toml may change it, a model without one has nothing to show.
  if (!editable && !price) return null;
  if (!open) {
    return (
      <button
        ref={toggle}
        type="button"
        className="mt-2 text-xs underline"
        style={muted}
        aria-label={`${price ? "price" : "set a price"} of ${entry.id}`}
        onClick={() => {
          reset();
          setOpen(true);
        }}
      >
        {price ? "price" : "set a price"}
      </button>
    );
  }
  const field = (label: string, value: string, set: (v: string) => void, placeholder: string, first = false) => (
    <label className="flex flex-wrap items-center gap-2">
      <span className="w-28">{label}</span>
      <input
        ref={first ? firstField : undefined}
        className="input tabular"
        style={{ maxWidth: 140 }}
        inputMode="decimal"
        value={value}
        placeholder={placeholder}
        disabled={!editable || busy}
        autoFocus={first && editable}
        onChange={(e) => set(e.target.value)}
        aria-label={`${label} price of ${entry.id}`}
      />
    </label>
  );
  return (
    <form
      className="mt-3 grid gap-2 text-sm"
      style={muted}
      aria-label={`price of ${entry.id}`}
      onSubmit={(e) => {
        e.preventDefault();
        if (editable && !busy) void submit(draft());
      }}
    >
      <p className="text-xs">US dollars per million tokens, as the provider lists them.</p>
      {field("input", input, setInput, "3", true)}
      {field("output", output, setOutput, "15")}
      {field("cache read", cacheRead, setCacheRead, "as input")}
      {field("cache write", cacheWrite, setCacheWrite, "as input")}
      <div className="flex flex-wrap items-center gap-2">
        {editable && (
          <button type="submit" className="btn-ghost" disabled={busy}>
            save
          </button>
        )}
        {editable && price && (
          <button type="button" className="btn-ghost" disabled={busy} onClick={() => void submit(undefined)}>
            clear
          </button>
        )}
        {editable && (
          <button type="button" className="text-xs underline" onClick={reset}>
            reset
          </button>
        )}
        <button type="button" className="text-xs underline" autoFocus={!editable} onClick={close}>
          close
        </button>
      </div>
      {error && <p role="alert" style={{ color: "var(--color-ruby)" }}>{error}</p>}
    </form>
  );
}
