"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { Circle, CircleCheck, TriangleAlert } from "lucide-react";
import {
  api,
  keyScopes,
  modelProvider,
  reachable,
  type ModelsView,
  type SearchStatus,
} from "@/lib/api";

type Setup = {
  providers: Set<string>;
  models: ModelsView;
  /** Null when the gateway predates GET /search. */
  search: SearchStatus | null;
};

// First-run steps; gone once there is a key, a model and an agent.
export function SetupChecklist({
  agentCount,
  onVisibleChange,
}: {
  agentCount: number | null;
  onVisibleChange?: (visible: boolean) => void;
}) {
  const [setup, setSetup] = useState<Setup | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let cancelled = false;
    Promise.all([api.teams.list(), api.models.get(), api.search.get().catch(() => null)])
      .then(async ([teams, models, search]) => {
        const scopes = await keyScopes(teams);
        if (!cancelled) setSetup({ providers: reachable(scopes, null), models, search });
      })
      .catch(() => {
        // Without the data there is nothing reliable to show.
        if (!cancelled) setFailed(true);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const providers = setup?.providers;
  const models = setup?.models;
  const anyKey = (providers?.size ?? 0) > 0;
  const hasModel = (models?.models.length ?? 0) > 0;
  const hasAgent = (agentCount ?? 0) > 0;
  const defaultProvider = modelProvider(models?.effective_default);
  const keyDone = hasModel && defaultProvider ? providers?.has(defaultProvider) === true : anyKey;
  const decided = agentCount !== null && (setup !== null || failed);
  const visible = setup !== null && agentCount !== null && !(anyKey && hasModel && hasAgent);

  useEffect(() => {
    if (decided) onVisibleChange?.(visible);
  }, [decided, visible, onVisibleChange]);

  if (!visible || !models) return null;

  return (
    <section aria-labelledby="setup-title" className="card-elevated mb-8 p-6">
      <h2 id="setup-title" className="display-h3 mb-1">
        get started
      </h2>
      <p className="mb-5 text-sm" style={{ color: "var(--color-body)" }}>
        Three steps before the first chat, and one optional. Done steps are
        checked.
      </p>
      <ol role="list" className="grid gap-4">
        <Step
          done={keyDone}
          title="Add an LLM provider key"
          action={{ href: "/credentials", label: "add a key" }}
        >
          A key you add is yours. To share one, a team admin adds it to the
          team&apos;s credentials; members&apos; personal agents and the
          team&apos;s agents can then use it.
          {hasModel && anyKey && !keyDone && (
            <span
              className="mt-1.5 flex items-start gap-1.5"
              style={{ color: "var(--color-warning-text)" }}
            >
              <TriangleAlert size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
              <span>
                No key your agents can use is for{" "}
                <strong className="font-normal">{defaultProvider}</strong>, the provider of the
                default model {models.effective_default}.
              </span>
            </span>
          )}
        </Step>
        <Step
          done={hasModel}
          title="List a model"
          action={models.editable ? { href: "/models", label: "add a model" } : undefined}
        >
          {models.editable ? (
            <>
              Agents use the models listed as <code className="font-mono">provider:model</code>,
              for example <code className="font-mono">openrouter:deepseek/deepseek-v4.1-flash-1017</code>.
            </>
          ) : (
            <>The operator lists models in the gateway&apos;s config.toml.</>
          )}
        </Step>
        <Step
          done={hasAgent}
          title="Create an agent"
          action={{ href: "/agents/new", label: "new agent" }}
        >
          Each agent gets its own workspace and a chat here.
        </Step>
        {setup?.search && setup.search.provider !== "off" && (
          <Step
            done={setup.search.backend !== null}
            title="Optional: let agents search the web"
            action={{ href: "/credentials", label: "add a search key" }}
          >
            {setup.search.backend ? (
              <>Agents search with {setup.search.backend}.</>
            ) : (
              <>
                Add a Brave, Tavily or Exa key as a web search key; with only an
                OpenRouter key and an <code className="font-mono">openrouter:</code>{" "}
                model, agents search through OpenRouter.
              </>
            )}
          </Step>
        )}
      </ol>
    </section>
  );
}

function Step({
  done,
  title,
  action,
  children,
}: {
  done: boolean;
  title: string;
  action?: { href: string; label: string };
  children: React.ReactNode;
}) {
  const Icon = done ? CircleCheck : Circle;
  return (
    <li className="flex items-start gap-3">
      <Icon
        size={20}
        className="mt-0.5 shrink-0"
        style={{ color: done ? "var(--color-success-text)" : "var(--color-border-purple)" }}
        aria-hidden="true"
      />
      <div className="min-w-0 flex-1">
        <p style={{ color: "var(--color-heading)" }}>
          {title}
          <span className="sr-only">{done ? " (done)" : " (to do)"}</span>
        </p>
        <div className="mt-0.5 text-sm" style={{ color: "var(--color-body)" }}>
          {children}
        </div>
      </div>
      {action && !done && (
        <Link href={action.href} className="btn-ghost shrink-0 text-sm">
          {action.label}
        </Link>
      )}
    </li>
  );
}
