"use client";

/**
 * Persistent sidebar — agent list (DESIGN.md sidebar pattern §10.2).
 *
 * Polls `/agents` every 5 s so status pills (`running` / `connected`) reflect
 * lifecycle changes without a full reload. WebSocket-based push lands when
 * we ship `/ws/status` (spec §8.3) — for Phase 1 polling is acceptable.
 */

import Link from "next/link";
import { usePathname } from "next/navigation";
import { useEffect, useState } from "react";
import { api, type AgentView, type TeamView } from "@/lib/api";

const POLL_MS = 5000;

export function Sidebar() {
  const pathname = usePathname();
  const [agents, setAgents] = useState<AgentView[] | null>(null);
  const [teams, setTeams] = useState<TeamView[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    async function tick() {
      try {
        const [a, t] = await Promise.all([api.agents.list(), api.teams.list()]);
        if (!cancelled) {
          setAgents(a);
          setTeams(t);
          setError(null);
        }
      } catch (e) {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e));
      }
    }
    tick();
    const id = setInterval(tick, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(id);
    };
  }, []);

  return (
    <aside
      className="flex w-64 shrink-0 flex-col border-r"
      style={{
        borderColor: "var(--color-border-default)",
        backgroundColor: "var(--color-surface)",
      }}
    >
      <Link
        href="/"
        className="px-5 py-5"
        style={{
          color: "var(--color-deep-navy)",
          fontWeight: 300,
          fontSize: 22,
          letterSpacing: "-0.4px",
          lineHeight: 1,
        }}
      >
        havn
      </Link>

      <div className="px-3">
        <div
          className="mb-2 px-2 py-1 text-[11px] uppercase tracking-wider"
          style={{ color: "var(--color-body)" }}
        >
          Agents
        </div>
        <nav className="flex flex-col gap-0.5">
          {agents === null && !error && (
            <div className="px-2 py-2 text-sm" style={{ color: "var(--color-body)" }}>
              loading…
            </div>
          )}
          {error && (
            <div className="px-2 py-2 text-sm" style={{ color: "var(--color-ruby)" }}>
              {error}
            </div>
          )}
          {agents?.length === 0 && (
            <div className="px-2 py-2 text-sm" style={{ color: "var(--color-body)" }}>
              no agents yet
            </div>
          )}
          {agents?.map((a) => {
            const href = `/agents/${a.id}`;
            const active = pathname === href;
            return (
              <Link
                key={a.id}
                href={href}
                className="flex items-center justify-between rounded px-2 py-1.5 text-sm transition-colors"
                style={{
                  backgroundColor: active
                    ? "rgba(83, 58, 253, 0.07)"
                    : "transparent",
                  color: active
                    ? "var(--color-stripe-purple)"
                    : "var(--color-deep-navy)",
                  borderRadius: "var(--radius-tight)",
                }}
              >
                <span className="truncate">{a.name}</span>
                <span className={pillClass(a.status, a.connected)}>
                  {pillLabel(a.status, a.connected)}
                </span>
              </Link>
            );
          })}
        </nav>

        <div className="mt-3 px-1">
          <Link href="/agents/new" className="btn-ghost block w-full text-center text-sm">
            + new agent
          </Link>
        </div>
      </div>

      {teams.length > 0 && (
        <div className="mt-3 px-3">
          <div
            className="mb-2 px-2 py-1 text-[11px] uppercase tracking-wider"
            style={{ color: "var(--color-body)" }}
          >
            Teams
          </div>
          <nav className="flex flex-col gap-0.5">
            {teams.map((t) => {
              const href = `/teams/${t.id}`;
              const active = pathname?.startsWith(href);
              return (
                <Link
                  key={t.id}
                  href={href}
                  className="flex items-center justify-between rounded px-2 py-1.5 text-sm transition-colors"
                  style={{
                    backgroundColor: active
                      ? "rgba(83, 58, 253, 0.07)"
                      : "transparent",
                    color: active
                      ? "var(--color-stripe-purple)"
                      : "var(--color-deep-navy)",
                    borderRadius: "var(--radius-tight)",
                  }}
                >
                  <span className="truncate">{t.name}</span>
                  {t.is_admin && (
                    <span
                      className="pill"
                      title="you're an admin of this team"
                      style={{
                        background: "rgba(83, 58, 253, 0.1)",
                        color: "var(--color-purple)",
                      }}
                    >
                      admin
                    </span>
                  )}
                </Link>
              );
            })}
          </nav>
        </div>
      )}

      <div className="mt-auto flex flex-col gap-0.5 px-3 pb-5">
        <div
          className="mb-1 px-2 py-1 text-[11px] uppercase tracking-wider"
          style={{ color: "var(--color-body)" }}
        >
          Manage
        </div>
        <SidebarLink href="/teams" pathname={pathname} label="👥 teams" />
        <SidebarLink href="/credentials" pathname={pathname} label="🔑 credentials" />
        <SidebarLink href="/" pathname={pathname} label="🤖 agents" />
      </div>
    </aside>
  );
}

function SidebarLink({
  href,
  pathname,
  label,
}: {
  href: string;
  pathname: string | null;
  label: string;
}) {
  const active = pathname === href;
  return (
    <Link
      href={href}
      className="rounded px-2 py-1.5 text-sm transition-colors"
      style={{
        backgroundColor: active ? "rgba(83, 58, 253, 0.07)" : "transparent",
        color: active ? "var(--color-stripe-purple)" : "var(--color-deep-navy)",
        borderRadius: "var(--radius-tight)",
      }}
    >
      {label}
    </Link>
  );
}

function pillLabel(status: AgentView["status"], connected: boolean): string {
  if (status === "running") return connected ? "live" : "starting";
  return status;
}

function pillClass(status: AgentView["status"], connected: boolean): string {
  if (status === "running" && connected) return "pill pill-running";
  if (status === "error") return "pill pill-error";
  return "pill pill-stopped";
}
