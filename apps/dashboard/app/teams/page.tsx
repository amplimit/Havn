"use client";

/**
 * `/teams` — landing page for the multi-tenant management surface
 * (spec §10.3).
 *
 * Shows every team the calling user belongs to, plus a create-team
 * form. Single-user operators see an empty state with a one-click
 * way to spin up their first team — useful when the operator decides
 * to start sharing agents with a teammate.
 */

import Link from "next/link";
import { useEffect, useState } from "react";
import { api, type TeamView } from "@/lib/api";

export default function TeamsPage() {
  const [teams, setTeams] = useState<TeamView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");

  async function load() {
    try {
      setTeams(await api.teams.list());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    load();
  }, []);

  async function onCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!newName.trim()) return;
    setCreating(true);
    try {
      await api.teams.create(newName.trim());
      setNewName("");
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setCreating(false);
    }
  }

  return (
    <div className="mx-auto max-w-4xl px-8 py-12">
      <header className="mb-8">
        <h1 className="display-h1">teams</h1>
        <p
          className="mt-2 max-w-2xl"
          style={{ color: "var(--color-body)", fontSize: 18, lineHeight: 1.4 }}
        >
          Group users for shared agents, shared credentials, and a single
          audit log. The user who creates a team is auto-promoted to its
          admin role.
        </p>
      </header>

      {error && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{
            color: "var(--color-ruby)",
            borderColor: "rgba(234, 34, 97, 0.3)",
          }}
        >
          {error}
        </div>
      )}

      <form
        onSubmit={onCreate}
        className="card mb-8 flex items-end gap-3 px-4 py-3"
      >
        <div className="flex-1">
          <label
            className="block text-xs uppercase tracking-wider"
            style={{ color: "var(--color-body)" }}
          >
            new team name
          </label>
          <input
            type="text"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder="e.g. engineering"
            className="mt-1 w-full bg-transparent text-base outline-none"
            style={{ borderBottom: "1px solid var(--color-border-default)" }}
          />
        </div>
        <button type="submit" className="btn-primary" disabled={creating}>
          {creating ? "creating…" : "create team"}
        </button>
      </form>

      {teams === null && !error && (
        <div
          className="card px-4 py-6 text-sm"
          style={{ color: "var(--color-body)" }}
        >
          loading…
        </div>
      )}

      {teams !== null && teams.length === 0 && (
        <div className="card-elevated px-8 py-12 text-center">
          <p className="display-h3 mb-2">no teams yet</p>
          <p style={{ color: "var(--color-body)" }}>
            Create one above to start sharing agents and credentials. Or
            keep your agents personal — havn works fine in single-user
            mode forever.
          </p>
        </div>
      )}

      {teams !== null && teams.length > 0 && (
        <ul className="grid gap-3">
          {teams.map((t) => (
            <li key={t.id}>
              <Link
                href={`/teams/${t.id}`}
                className="card flex items-center justify-between px-4 py-3 transition-colors hover:bg-[rgba(83,58,253,0.04)]"
              >
                <div>
                  <p className="display-h3">{t.name}</p>
                  <p
                    className="mt-1 font-mono text-xs"
                    style={{ color: "var(--color-body)" }}
                  >
                    {t.id}
                  </p>
                </div>
                <div className="text-right text-xs">
                  {t.is_admin && (
                    <span
                      className="pill"
                      style={{
                        background: "rgba(83, 58, 253, 0.1)",
                        color: "var(--color-purple)",
                      }}
                    >
                      admin
                    </span>
                  )}
                  <p
                    className="mt-1 font-mono"
                    style={{ color: "var(--color-body)" }}
                  >
                    created {new Date(t.created_at).toISOString().slice(0, 10)}
                  </p>
                </div>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

