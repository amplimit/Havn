"use client";

import { useEffect, useState } from "react";
import { api, type BrowseView } from "@/lib/api";

/** `/settings` — the gateway's own settings, written to its config.toml. */
export default function SettingsPage() {
  const [browse, setBrowse] = useState<BrowseView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    api.workspace.browse().then(setBrowse, (e: unknown) =>
      setError(e instanceof Error ? e.message : String(e)),
    );
  }, []);
  async function toggle(on: boolean) {
    setSaving(true);
    setError(null);
    try {
      setBrowse(await api.workspace.setBrowse(on));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }
  const muted = { color: "var(--color-body)" };
  return (
    <div className="mx-auto max-w-3xl px-6 py-8">
      <h1 className="display-h2 mb-2">gateway settings</h1>
      <p className="mb-8 text-sm" style={muted}>
        Settings of the whole gateway, kept in its config.toml.
      </p>
      {error && (
        <p className="mb-4 text-sm" role="alert" style={{ color: "var(--color-ruby)" }}>
          {error}
        </p>
      )}
      {browse && (
        <section className="card px-4 py-4" aria-labelledby="browse-title">
          <h2 id="browse-title" className="display-h3 mb-2">
            workspace browsing
          </h2>
          <p className="mb-3 text-sm" style={muted}>
            On, each agent&apos;s owner can browse its workspace from the dashboard, read-only,
            unless they turn it off for that agent. Off, no agent&apos;s workspace can be
            browsed. Written to <span className="font-mono">[workspace] browse</span>.
          </p>
          <label className="inline-flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              checked={browse.browse}
              disabled={saving || !browse.editable}
              onChange={(e) => void toggle(e.target.checked)}
            />
            let owners browse their agents&apos; workspaces
          </label>
          {!browse.editable && (
            <p className="mt-1 text-xs" style={muted}>
              With trust_header on, this is set in config.toml.
            </p>
          )}
        </section>
      )}
    </div>
  );
}
