"use client";

import { useRouter } from "next/navigation";
import { useState } from "react";
import { api } from "@/lib/api";

export default function NewAgentPage() {
  const router = useRouter();
  const [name, setName] = useState("");
  const [model, setModel] = useState("claude-opus-4-7");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function onSubmit(e: React.FormEvent) {
    e.preventDefault();
    setSubmitting(true);
    setError(null);
    try {
      const agent = await api.agents.create(name.trim(), { model });
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
        Pick a name and model. The agent gets its own workspace, four bootstrap
        files (SOUL / USER / HEARTBEAT / IDENTITY), and a WebChat channel
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
            <option value="claude-opus-4-7">claude-opus-4-7 — most capable</option>
            <option value="claude-sonnet-4-6">claude-sonnet-4-6 — balanced</option>
            <option value="claude-haiku-4-5">claude-haiku-4-5 — fastest</option>
          </select>
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
