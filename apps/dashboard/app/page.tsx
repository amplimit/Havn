"use client";

import { useEffect, useState } from "react";
import Link from "next/link";
import { api, type AgentView } from "@/lib/api";

export default function HomePage() {
  const [agents, setAgents] = useState<AgentView[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  async function reload() {
    try {
      setAgents(await api.agents.list());
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    reload();
  }, []);

  async function deleteAgent(id: string) {
    if (
      !confirm(
        "Delete this agent? Its workspace, conversations, memory, and skills are all removed. Unrecoverable.",
      )
    ) {
      return;
    }
    setBusy(id);
    try {
      await api.agents.delete(id);
      await reload();
    } catch (e) {
      alert(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="mx-auto max-w-5xl px-8 py-12">
      <header className="mb-8 flex items-end justify-between">
        <div>
          <h1 className="display-h1">your agents</h1>
          <p
            className="mt-2 max-w-2xl"
            style={{ color: "var(--color-body)", fontSize: 18, lineHeight: 1.4 }}
          >
            Click an agent to chat. The runtime starts on first use and stays
            warm — you don't manage start/stop yourself.
          </p>
        </div>
        <Link href="/agents/new" className="btn-primary">
          new agent
        </Link>
      </header>

      {error && (
        <div
          className="card mb-6 px-4 py-3 text-sm"
          style={{ color: "var(--color-ruby)", borderColor: "rgba(234, 34, 97, 0.3)" }}
        >
          {error}
        </div>
      )}

      {agents === null && !error && (
        <div className="card px-4 py-6 text-sm" style={{ color: "var(--color-body)" }}>
          loading…
        </div>
      )}

      {agents?.length === 0 && (
        <div className="card-elevated px-8 py-16 text-center">
          <p
            className="display-h3 mb-2"
          >
            no agents yet
          </p>
          <p style={{ color: "var(--color-body)" }} className="mb-6">
            Create your first agent to start chatting.
          </p>
          <Link href="/agents/new" className="btn-primary inline-block">
            create your first agent
          </Link>
        </div>
      )}

      <ul className="grid gap-3">
        {agents?.map((a) => (
          <li key={a.id} className="card px-5 py-4">
            <div className="flex items-center justify-between gap-4">
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-3">
                  <Link
                    href={`/agents/${a.id}`}
                    className="display-h3 truncate hover:underline"
                  >
                    {a.name}
                  </Link>
                  <StatusPill status={a.status} connected={a.connected} />
                </div>
                <p
                  className="mt-1 font-mono text-xs"
                  style={{ color: "var(--color-body)" }}
                >
                  {a.id}
                </p>
              </div>
              <div className="flex shrink-0 gap-2">
                <Link href={`/agents/${a.id}`} className="btn-primary">
                  chat
                </Link>
                <button
                  type="button"
                  className="btn-danger"
                  disabled={busy === a.id}
                  onClick={() => deleteAgent(a.id)}
                >
                  delete
                </button>
              </div>
            </div>
          </li>
        ))}
      </ul>
    </div>
  );
}

function StatusPill({
  status,
  connected,
}: {
  status: AgentView["status"];
  connected: boolean;
}) {
  if (status === "running" && connected) {
    return <span className="pill pill-running">live</span>;
  }
  if (status === "running") {
    return <span className="pill pill-stopped">starting…</span>;
  }
  if (status === "error") {
    return <span className="pill pill-error">error</span>;
  }
  return <span className="pill pill-stopped">{status}</span>;
}
