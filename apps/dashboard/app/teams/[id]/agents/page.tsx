"use client";

/**
 * `/teams/[id]/agents` — every agent associated with this team
 * (spec §10.3 admin view "Team Agents"). Members can view; admins
 * who want to bulk-stop click into individual agents (the lifecycle
 * endpoints already gate on owner_id, so cross-user start/stop from
 * here is intentionally NOT exposed — admins ask the owner).
 */

import { use, useEffect, useState } from "react";
import Link from "next/link";
import { api, type TeamAgentView } from "@/lib/api";

const STATUS_COLOUR: Record<TeamAgentView["status"], string> = {
  created: "var(--color-body)",
  running: "var(--color-success-text)",
  paused: "var(--color-body)",
  stopped: "var(--color-body)",
  error: "var(--color-ruby)",
};

export default function TeamAgentsPage({
  params,
}: {
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const [agents, setAgents] = useState<TeamAgentView[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const a = await api.teams.listAgents(id);
        if (!cancelled) setAgents(a);
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

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

  if (agents === null) {
    return (
      <p className="card px-4 py-6 text-sm" style={{ color: "var(--color-body)" }}>
        loading…
      </p>
    );
  }

  if (agents.length === 0) {
    return (
      <div className="card-elevated px-8 py-12 text-center">
        <p className="display-h3 mb-2">no team agents</p>
        <p style={{ color: "var(--color-body)" }}>
          Members create agents from the main sidebar; assign one to this
          team via its detail page (coming with the agent-team-binding
          editor).
        </p>
      </div>
    );
  }

  return (
    <ul className="grid gap-2">
      {agents.map((a) => (
        <li key={a.id}>
          <Link
            href={`/agents/${a.id}`}
            className="card flex items-center justify-between px-4 py-3 transition-colors hover:bg-[rgba(83,58,253,0.04)]"
          >
            <div className="min-w-0 flex-1">
              <p className="display-h3">{a.name}</p>
              <p
                className="font-mono text-xs"
                style={{ color: "var(--color-body)" }}
              >
                {a.id}
              </p>
            </div>
            <div className="text-right text-xs">
              <span style={{ color: STATUS_COLOUR[a.status] }}>{a.status}</span>
              <p style={{ color: "var(--color-body)" }}>
                owner: {a.owner_display_name}
              </p>
            </div>
          </Link>
        </li>
      ))}
    </ul>
  );
}
