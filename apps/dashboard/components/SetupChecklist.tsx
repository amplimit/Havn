"use client";

import Link from "next/link";
import { useEffect, useState } from "react";
import { Circle, CircleCheck, TriangleAlert } from "lucide-react";
import { api, modelProvider, reachableProviders, type ModelsView } from "@/lib/api";

type Setup = {
  providers: Set<string>;
  models: ModelsView;
};

/**
 * First-run steps on the home page: a key, a model, an agent. Hidden once
 * all three are done and the default model has a key; nothing to dismiss.
 */
export function SetupChecklist({ agentCount }: { agentCount: number | null }) {
  const [setup, setSetup] = useState<Setup | null>(null);

  useEffect(() => {
    let cancelled = false;
    Promise.all([api.teams.list(), api.models.get()])
      .then(async ([teams, models]) => {
        const providers = await reachableProviders(teams, null);
        if (!cancelled) setSetup({ providers, models });
      })
      .catch(() => {
        // Without the data there is nothing reliable to show.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  if (!setup || agentCount === null) return null;
  const { providers, models } = setup;
  const hasKey = providers.size > 0;
  const hasModel = models.models.length > 0;
  const hasAgent = agentCount > 0;
  const defaultProvider = modelProvider(models.effective_default);
  const defaultUncovered = hasModel && defaultProvider !== null && !providers.has(defaultProvider);
  if (hasKey && hasModel && hasAgent && !defaultUncovered) return null;

  return (
    <section aria-labelledby="setup-title" className="card-elevated mb-8 p-6">
      <h2 id="setup-title" className="display-h3 mb-1">
        get started
      </h2>
      <p className="mb-5 text-sm" style={{ color: "var(--color-body)" }}>
        Three steps before the first chat. Done steps are checked.
      </p>
      <ol className="grid gap-4">
        <Step
          done={hasKey}
          title="Add an LLM provider key"
          action={{ href: "/credentials", label: "add a key" }}
        >
          A key you add is yours. To share one, a team admin adds it to the
          team&apos;s credentials: every member&apos;s agents can then use it.
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
          {defaultUncovered && (
            <span
              className="mt-1.5 flex items-start gap-1.5"
              style={{ color: "#c0164f" }}
            >
              <TriangleAlert size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
              <span>
                No key for <strong className="font-normal">{defaultProvider}</strong>, the
                provider of the default model {models.effective_default}, reaches your agents.{" "}
                <Link href="/credentials" className="underline">
                  Add one
                </Link>
                .
              </span>
            </span>
          )}
        </Step>
        <Step
          done={hasAgent}
          title="Create an agent"
          action={{ href: "/agents/new", label: "new agent" }}
        >
          Each agent gets its own workspace and a chat here.
        </Step>
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
