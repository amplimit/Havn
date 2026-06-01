"use client";

/**
 * `/teams/[id]` overview tab — quick summary + admin-only delete
 * (spec §10.3).
 */

import { use, useEffect, useState } from "react";
import { useRouter } from "next/navigation";
import { api, type TeamView } from "@/lib/api";

export default function TeamOverviewPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const router = useRouter();
  const [team, setTeam] = useState<TeamView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const t = await api.teams.get(id);
        if (!cancelled) setTeam(t);
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

  async function onRename(e: React.FormEvent) {
    e.preventDefault();
    if (!renaming || !renaming.trim() || renaming === team?.name) {
      setRenaming(null);
      return;
    }
    try {
      const t = await api.teams.rename(id, renaming.trim());
      setTeam(t);
      setRenaming(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function onDelete() {
    if (
      !confirm(
        `Delete team "${team?.name}"? Members and team-scoped roles cascade away. Agents owned by team members keep their data; their team_id is set to NULL.`,
      )
    ) {
      return;
    }
    try {
      await api.teams.delete(id);
      router.push("/teams");
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  if (error) {
    return (
      <div
        className="card px-4 py-3 text-sm"
        style={{
          color: "var(--color-ruby)",
          borderColor: "rgba(234, 34, 97, 0.3)",
        }}
      >
        {error}
      </div>
    );
  }

  if (!team) {
    return (
      <div
        className="card px-4 py-6 text-sm"
        style={{ color: "var(--color-body)" }}
      >
        loading…
      </div>
    );
  }

  return (
    <div className="grid gap-6">
      <section className="card px-4 py-4">
        <h2 className="display-h3 mb-3">about</h2>
        <dl className="grid grid-cols-[120px_1fr] gap-y-2 text-sm">
          <dt style={{ color: "var(--color-body)" }}>id</dt>
          <dd className="font-mono">{team.id}</dd>
          <dt style={{ color: "var(--color-body)" }}>created</dt>
          <dd>{new Date(team.created_at).toUTCString()}</dd>
          <dt style={{ color: "var(--color-body)" }}>your role</dt>
          <dd>{team.is_admin ? "admin" : "member"}</dd>
        </dl>
      </section>

      {team.is_admin && (
        <>
          <section className="card px-4 py-4">
            <h2 className="display-h3 mb-3">rename</h2>
            {renaming === null ? (
              <button
                type="button"
                onClick={() => setRenaming(team.name)}
                className="btn-ghost"
              >
                rename team
              </button>
            ) : (
              <form onSubmit={onRename} className="flex items-end gap-3">
                <input
                  type="text"
                  value={renaming}
                  onChange={(e) => setRenaming(e.target.value)}
                  className="flex-1 bg-transparent text-base outline-none"
                  style={{
                    borderBottom: "1px solid var(--color-border-default)",
                  }}
                />
                <button type="submit" className="btn-primary">
                  save
                </button>
                <button
                  type="button"
                  className="btn-ghost"
                  onClick={() => setRenaming(null)}
                >
                  cancel
                </button>
              </form>
            )}
          </section>

          <section
            className="card px-4 py-4"
            style={{ borderColor: "rgba(234, 34, 97, 0.3)" }}
          >
            <h2 className="display-h3 mb-3" style={{ color: "var(--color-ruby)" }}>
              danger zone
            </h2>
            <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
              Delete this team. Members, team-scoped roles, and team
              credentials cascade. Agents owned by members keep their
              workspace; only their team_id is cleared.
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
              delete team
            </button>
          </section>
        </>
      )}
    </div>
  );
}
