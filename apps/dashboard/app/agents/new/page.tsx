"use client";

import Link from "next/link";
import { useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import { TriangleAlert } from "lucide-react";
import {
  api,
  modelProvider,
  reachableProviders,
  type ModelsView,
  type TeamView,
} from "@/lib/api";

export default function NewAgentPage() {
  const router = useRouter();
  const [name, setName] = useState("");
  // Empty: the agent follows the gateway's default model.
  const [model, setModel] = useState("");
  const [models, setModels] = useState<ModelsView | null>(null);
  const [teams, setTeams] = useState<TeamView[]>([]);
  const [teamId, setTeamId] = useState("");
  // LLM providers with a key this agent could use; null until known.
  const [providers, setProviders] = useState<Set<string> | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    const wanted = new URLSearchParams(window.location.search).get("team");
    api.models
      .get()
      .then((m) => {
        if (!cancelled) setModels(m);
      })
      .catch(() => {
        // Without the list the agent follows the gateway's default model.
      });
    (async () => {
      try {
        const t = await api.teams.list();
        if (cancelled) return;
        setTeams(t);
        if (wanted && t.some((team) => team.id === wanted && team.can_create_agents))
          setTeamId(wanted);
      } catch {
        // Without the team list the agent is created as a personal agent.
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    reachableProviders(teams, teamId || null)
      .then((p) => {
        if (!cancelled) setProviders(p);
      })
      .catch(() => {
        // Without the credential lists there is no warning to give.
      });
    return () => {
      cancelled = true;
    };
  }, [teams, teamId]);

  const provider = modelProvider(model || models?.effective_default);
  const uncovered = providers !== null && provider !== null && !providers.has(provider);

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const agent = await api.agents.create(
        name.trim(),
        model ? { model } : {},
        teamId || null,
      );
      router.push(`/agents/${agent.id}`);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setSubmitting(false);
    }
  }

  return (
    <div className="mx-auto max-w-xl px-8 py-12">
      <h1 className="display-h2 mb-2">new agent</h1>
      <p style={{ color: "var(--color-body)" }} className="mb-8">
        Pick a name and model. The agent gets its own workspace, three bootstrap
        files (SOUL / USER / HEARTBEAT), and a WebChat channel
        binding automatically.
      </p>

      <form onSubmit={onSubmit} className="card-elevated p-8">
        <div className="mb-5">
          <label htmlFor="name" className="label">
            agent name
          </label>
          <input
            id="name"
            type="text"
            required
            minLength={1}
            maxLength={64}
            placeholder="research-helper"
            value={name}
            onChange={(e) => setName(e.target.value)}
            className="input"
          />
          <p
            className="mt-1.5 text-xs"
            style={{ color: "var(--color-body)" }}
          >
            Must be unique within your account. Lowercase / hyphens recommended.
          </p>
        </div>

        {teams.length > 0 && (
          <div className="mb-5">
            <label htmlFor="team" className="label">
              team
            </label>
            <select
              id="team"
              value={teamId}
              onChange={(e) => setTeamId(e.target.value)}
              className="input"
            >
              <option value="">none (personal agent)</option>
              {teams.map((t) => (
                <option key={t.id} value={t.id} disabled={!t.can_create_agents}>
                  {t.can_create_agents ? t.name : `${t.name} (agent limit reached)`}
                </option>
              ))}
            </select>
            <p
              className="mt-1.5 text-xs"
              style={{ color: "var(--color-body)" }}
            >
              A personal agent can use your keys and those of all your
              teams. A team agent uses your keys and that team&apos;s, runs
              under your role in the team and counts toward its agent limit.
            </p>
          </div>
        )}

        <div className="mb-7">
          <label htmlFor="model" className="label">
            model
          </label>
          <select
            id="model"
            value={model}
            onChange={(e) => setModel(e.target.value)}
            className="input"
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
          </select>
          {models && models.models.length === 0 && (
            <p className="mt-1.5 text-xs" style={{ color: "var(--color-ruby)" }}>
              The gateway lists no models yet, so the agent cannot chat.{" "}
              <Link href="/models" className="underline">
                Add one
              </Link>{" "}
              first.
            </p>
          )}
          {uncovered && (
            <p
              role="status"
              className="mt-2 flex items-start gap-1.5 text-xs"
              style={{ color: "#c0164f" }}
            >
              <TriangleAlert size={14} className="mt-px shrink-0" aria-hidden="true" />
              <span>
                No key for <strong className="font-normal">{provider}</strong> reaches
                this agent, so it cannot chat yet.{" "}
                <Link href="/credentials" className="underline">
                  Add your own
                </Link>
                {teamId
                  ? ", or ask a team admin to add one to the team's credentials."
                  : ", or ask a team admin to add one to a team you are in."}
              </span>
            </p>
          )}
        </div>

        {error && (
          <div
            className="mb-4 rounded px-3 py-2 text-sm"
            style={{
              borderRadius: "var(--radius-tight)",
              backgroundColor: "rgba(234, 34, 97, 0.06)",
              border: "1px solid rgba(234, 34, 97, 0.3)",
              color: "var(--color-ruby)",
            }}
          >
            {error}
          </div>
        )}

        <div className="flex justify-end gap-2">
          <button
            type="button"
            className="btn-ghost"
            disabled={submitting}
            onClick={() => router.push("/")}
          >
            cancel
          </button>
          <button
            type="submit"
            className="btn-primary"
            disabled={submitting || !name.trim()}
          >
            {submitting ? "creating…" : "create agent"}
          </button>
        </div>
      </form>
    </div>
  );
}
