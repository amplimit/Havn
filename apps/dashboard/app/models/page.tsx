"use client";

import { useEffect, useState } from "react";
import { ApiError, LLM_PROVIDERS as PROVIDERS, api, type ModelEntry, type ModelsView } from "@/lib/api";

export default function ModelsPage() {
  const [view, setView] = useState<ModelsView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const [provider, setProvider] = useState("anthropic");
  const [name, setName] = useState("");
  const [label, setLabel] = useState("");
  const [maxOutput, setMaxOutput] = useState("");
  const [makeDefault, setMakeDefault] = useState(false);

  useEffect(() => {
    api.models
      .get()
      .then(setView)
      .catch((e) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  async function save(defaultModel: string | null, models: ModelEntry[]) {
    setBusy(true);
    setError(null);
    try {
      setView(
        await api.models.put({
          version: view?.version ?? "",
          default_model: defaultModel,
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

  return (
    <div className="mx-auto max-w-4xl px-8 py-12">
      <header className="mb-8">
        <h1 className="display-h1">models</h1>
        <p className="mt-2" style={{ color: "var(--color-body)", fontSize: 18 }}>
          The models agents may use, as provider:model. Agents without a model
          of their own use the default: the one marked below, or the first.
          The list lives in <code className="font-mono">[[llm.models]]</code>{" "}
          of the gateway&apos;s config.toml; changes here are written there.
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
                {PROVIDERS.map((p) => (
                  <option key={p} value={p}>
                    {p}
                  </option>
                ))}
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
                    : "claude-sonnet-4-6"
                }
                value={name}
                onChange={(e) => setName(e.target.value)}
                className="input font-mono"
              />
            </div>
            <div className="md:col-span-2">
              <label className="label" htmlFor="model-label">label (optional)</label>
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
            <div className="flex items-end">
              <label className="flex items-center gap-2 text-sm">
                <input
                  type="checkbox"
                  checked={makeDefault}
                  onChange={(e) => setMakeDefault(e.target.checked)}
                />
                make default
              </label>
            </div>
            <div className="flex justify-end md:col-span-4">
              <button type="submit" className="btn-primary" disabled={busy || !name.trim()}>
                {busy ? "saving…" : "add model"}
              </button>
            </div>
          </form>
        </div>
      )}

      <h2 className="display-h2 mb-4">listed ({view?.models.length ?? "…"})</h2>

      {view === null && !error && (
        <div className="card px-4 py-6 text-sm" style={{ color: "var(--color-body)" }}>
          loading…
        </div>
      )}

      {view?.models.length === 0 && (
        <div className="card px-5 py-10 text-center" style={{ color: "var(--color-body)" }}>
          no models yet — agents cannot chat until one is added.
        </div>
      )}

      <ul className="grid gap-3">
        {view?.models.map((m, i) => (
          <li key={m.id} className="card flex items-center justify-between px-5 py-4">
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-3">
                <span className="display-h3">{m.label ?? m.id}</span>
                {view.effective_default === m.id && (
                  <span className="pill pill-running">default</span>
                )}
                {m.max_output_tokens && (
                  <span className="pill pill-stopped tabular">
                    max {m.max_output_tokens.toLocaleString()} output tokens
                  </span>
                )}
              </div>
              {m.label && (
                <p className="mt-1 font-mono text-xs" style={{ color: "var(--color-body)" }}>
                  {m.id}
                </p>
              )}
            </div>
            {editable && (
              <div className="flex gap-2">
                <button
                  type="button"
                  className="btn-ghost"
                  disabled={busy || i === 0}
                  onClick={() => move(i, -1)}
                >
                  up
                </button>
                <button
                  type="button"
                  className="btn-ghost"
                  disabled={busy || i === view.models.length - 1}
                  onClick={() => move(i, 1)}
                >
                  down
                </button>
                {view.effective_default !== m.id && (
                  <button
                    type="button"
                    className="btn-ghost"
                    disabled={busy}
                    onClick={() => save(m.id, view.models)}
                  >
                    make default
                  </button>
                )}
                <button
                  type="button"
                  className="btn-danger"
                  disabled={busy}
                  onClick={() => remove(m.id)}
                >
                  remove
                </button>
              </div>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}
