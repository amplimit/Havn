"use client";

/**
 * `/teams/[id]/members` — admin-only roster (spec §10.3).
 *
 * Add/remove members + change their role. Uses the team's roles API
 * to populate the role dropdown so admins always see the actual set
 * of roles available in this team. Spec §10.3 calls out: "no invite
 * link / email flow — that's the upstream auth proxy's job"; the
 * admin types a known X-User-ID (CLI: `havn user list`).
 */

import { use, useEffect, useState } from "react";
import {
  ApiError,
  api,
  type MemberView,
  type RoleView,
} from "@/lib/api";

export default function MembersPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const [members, setMembers] = useState<MemberView[] | null>(null);
  const [roles, setRoles] = useState<RoleView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [newUserId, setNewUserId] = useState("");
  const [newRoleId, setNewRoleId] = useState("");

  async function load() {
    try {
      const [m, r] = await Promise.all([
        api.teams.members.list(id),
        api.teams.roles.list(id),
      ]);
      setMembers(m);
      setRoles(r);
      // Default to the seeded `member` role on the add form.
      if (!newRoleId) {
        const seed = r.find((x) => x.name === "member") ?? r[0];
        if (seed) setNewRoleId(seed.id);
      }
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  async function onAdd(e: React.FormEvent) {
    e.preventDefault();
    if (!newUserId.trim() || !newRoleId) return;
    setAdding(true);
    try {
      await api.teams.members.add(id, newUserId.trim(), newRoleId);
      setNewUserId("");
      await load();
    } catch (err) {
      if (err instanceof ApiError && err.status === 400) {
        setError(
          "User not found in users table — provision via `havn user add <id> --display-name \"...\"` first.",
        );
      } else {
        setError(err instanceof Error ? err.message : String(err));
      }
    } finally {
      setAdding(false);
    }
  }

  async function onRoleChange(userId: string, roleId: string) {
    try {
      await api.teams.members.patch(id, userId, roleId);
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function onRemove(userId: string, displayName: string) {
    if (!confirm(`Remove ${displayName} from this team?`)) return;
    try {
      await api.teams.members.remove(id, userId);
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

      <section className="card px-4 py-4">
        <h2 className="display-h3 mb-3">add member</h2>
        <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
          The user must already exist in the users table. Provision new
          users from the host with{" "}
          <code className="font-mono">
            havn user add &lt;uuid&gt; --display-name "..."
          </code>
          .
        </p>
        <form onSubmit={onAdd} className="grid gap-3 sm:grid-cols-[1fr_auto_auto]">
          <input
            type="text"
            value={newUserId}
            onChange={(e) => setNewUserId(e.target.value)}
            placeholder="user X-User-ID (UUID v7)"
            className="bg-transparent text-base outline-none"
            style={{ borderBottom: "1px solid var(--color-border-default)" }}
          />
          <select
            value={newRoleId}
            onChange={(e) => setNewRoleId(e.target.value)}
            className="bg-transparent text-sm outline-none"
            style={{ borderBottom: "1px solid var(--color-border-default)" }}
          >
            {roles.map((r) => (
              <option key={r.id} value={r.id}>
                {r.name}
              </option>
            ))}
          </select>
          <button type="submit" className="btn-primary" disabled={adding}>
            {adding ? "adding…" : "add"}
          </button>
        </form>
      </section>

      <section>
        <h2 className="display-h3 mb-3">members</h2>
        {members === null ? (
          <p style={{ color: "var(--color-body)" }}>loading…</p>
        ) : members.length === 0 ? (
          <p style={{ color: "var(--color-body)" }}>(none yet)</p>
        ) : (
          <ul className="grid gap-2">
            {members.map((m) => (
              <li
                key={m.user_id}
                className="card flex items-center justify-between px-4 py-3"
              >
                <div className="min-w-0 flex-1">
                  <p>{m.display_name}</p>
                  <p
                    className="font-mono text-xs"
                    style={{ color: "var(--color-body)" }}
                  >
                    {m.user_id}
                  </p>
                </div>
                <div className="flex items-center gap-3">
                  <select
                    value={m.role_id}
                    onChange={(e) => onRoleChange(m.user_id, e.target.value)}
                    className="bg-transparent text-sm outline-none"
                    style={{
                      borderBottom: "1px solid var(--color-border-default)",
                    }}
                  >
                    {roles.map((r) => (
                      <option key={r.id} value={r.id}>
                        {r.name}
                      </option>
                    ))}
                  </select>
                  <button
                    type="button"
                    onClick={() => onRemove(m.user_id, m.display_name)}
                    className="text-xs underline"
                    style={{ color: "var(--color-ruby)" }}
                  >
                    remove
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
