"use client";

/**
 * `/teams/[id]/roles` — admin-only role editor (spec §6.4, §10.3).
 *
 * Two-pane: list of roles on top, JSON editor inline. Spec §10.3
 * calls for "form-based policy editor (not raw YAML)" — JSON in a
 * textarea is a deliberate compromise. The full Policy schema is too
 * wide for a generated form (boolean toggles for permissions plus
 * nested objects for resource_limits / context_toolsets / network_policy).
 * Until we ship per-field controls, we surface the shape with hint
 * text and validate by deserialising before send.
 *
 * Built-in `admin` and `member` roles can be edited in place but not
 * deleted (the seed roles are load-bearing for the dashboard's role
 * picker).
 */

import { use, useEffect, useState } from "react";
import { ApiError, api, type RoleView } from "@/lib/api";

const POLICY_HINT = `Edit the JSON below. Required top-level keys (see havn-core::Policy):
  max_agents, allowed_models, resource_limits, budget, permissions,
  network_policy, context_toolsets, admin_visibility.
Save validates by deserialising — broken JSON yields a 400 with the parse error.`;

export default function RolesPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const [roles, setRoles] = useState<RoleView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [newName, setNewName] = useState("");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState<string>("");

  async function load() {
    try {
      setRoles(await api.teams.roles.list(id));
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id]);

  async function onCreate(e: React.FormEvent) {
    e.preventDefault();
    if (!newName.trim()) return;
    setCreating(true);
    try {
      await api.teams.roles.create(id, newName.trim());
      setNewName("");
      await load();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setCreating(false);
    }
  }

  function onEdit(r: RoleView) {
    setEditingId(r.id);
    setDraft(JSON.stringify(r.policy, null, 2));
  }

  async function onSave(roleId: string) {
    let policy: Record<string, unknown>;
    try {
      policy = JSON.parse(draft);
    } catch (e) {
      setError(`policy is not valid JSON: ${e instanceof Error ? e.message : e}`);
      return;
    }
    try {
      await api.teams.roles.patch(id, roleId, policy);
      setEditingId(null);
      await load();
    } catch (err) {
      if (err instanceof ApiError) {
        setError(`${err.status}: ${err.body}`);
      } else {
        setError(err instanceof Error ? err.message : String(err));
      }
    }
  }

  async function onDelete(role: RoleView) {
    if (role.name === "admin" || role.name === "member") {
      setError(`the seeded ${role.name} role can be edited but not deleted`);
      return;
    }
    if (!confirm(`Delete role "${role.name}"?`)) return;
    try {
      await api.teams.roles.delete(id, role.id);
      await load();
    } catch (err) {
      if (err instanceof ApiError && err.status === 409) {
        setError(
          "Role still has members assigned. Re-assign them on the members page first.",
        );
      } else {
        setError(err instanceof Error ? err.message : String(err));
      }
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
        <h2 className="display-h3 mb-3">create role</h2>
        <p className="mb-3 text-sm" style={{ color: "var(--color-body)" }}>
          New roles get a narrow default policy. Click "edit policy" on
          the role below to broaden permissions.
        </p>
        <form onSubmit={onCreate} className="flex items-end gap-3">
          <input
            type="text"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            placeholder="e.g. auditor, ops-readonly"
            className="flex-1 bg-transparent text-base outline-none"
            style={{ borderBottom: "1px solid var(--color-border-default)" }}
          />
          <button type="submit" className="btn-primary" disabled={creating}>
            {creating ? "creating…" : "create"}
          </button>
        </form>
      </section>

      <section>
        <h2 className="display-h3 mb-3">roles</h2>
        {roles === null ? (
          <p style={{ color: "var(--color-body)" }}>loading…</p>
        ) : (
          <ul className="grid gap-3">
            {roles.map((r) => (
              <li key={r.id} className="card px-4 py-3">
                <div className="flex items-start justify-between gap-3">
                  <div>
                    <p className="display-h3">{r.name}</p>
                    <p
                      className="font-mono text-xs"
                      style={{ color: "var(--color-body)" }}
                    >
                      {r.id}
                    </p>
                  </div>
                  <div className="flex gap-2">
                    {editingId !== r.id && (
                      <button
                        type="button"
                        onClick={() => onEdit(r)}
                        className="btn-ghost text-sm"
                      >
                        edit policy
                      </button>
                    )}
                    {r.name !== "admin" && r.name !== "member" && (
                      <button
                        type="button"
                        onClick={() => onDelete(r)}
                        className="text-sm underline"
                        style={{ color: "var(--color-ruby)" }}
                      >
                        delete
                      </button>
                    )}
                  </div>
                </div>

                {editingId === r.id ? (
                  <div className="mt-3">
                    <p
                      className="mb-2 text-xs"
                      style={{ color: "var(--color-body)" }}
                    >
                      {POLICY_HINT}
                    </p>
                    <textarea
                      value={draft}
                      onChange={(e) => setDraft(e.target.value)}
                      rows={20}
                      spellCheck={false}
                      className="w-full font-mono text-xs"
                      style={{
                        background: "rgba(100, 116, 141, 0.05)",
                        padding: "12px",
                        border: "1px solid var(--color-border-default)",
                        borderRadius: "4px",
                      }}
                    />
                    <div className="mt-3 flex gap-2">
                      <button
                        type="button"
                        onClick={() => onSave(r.id)}
                        className="btn-primary"
                      >
                        save
                      </button>
                      <button
                        type="button"
                        onClick={() => setEditingId(null)}
                        className="btn-ghost"
                      >
                        cancel
                      </button>
                    </div>
                  </div>
                ) : (
                  <details className="mt-2">
                    <summary
                      className="cursor-pointer text-xs"
                      style={{ color: "var(--color-body)" }}
                    >
                      show policy JSON
                    </summary>
                    <pre
                      className="mt-2 overflow-x-auto text-xs leading-relaxed"
                      style={{
                        background: "rgba(100, 116, 141, 0.05)",
                        padding: "12px",
                        borderRadius: "4px",
                      }}
                    >
                      {JSON.stringify(r.policy, null, 2)}
                    </pre>
                  </details>
                )}
              </li>
            ))}
          </ul>
        )}
      </section>
    </div>
  );
}
