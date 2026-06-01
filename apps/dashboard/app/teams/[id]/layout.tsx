"use client";

/**
 * Team-scoped layout — shared sub-nav for `/teams/[id]/*` pages
 * (spec §10.3 admin views). Each tab is a Next route under this
 * folder; the layout provides the consistent header + nav strip.
 *
 * The team detail data is fetched once per render of the layout so
 * the header always reflects the latest state. Children call their
 * own data hooks for the tab body.
 */

import Link from "next/link";
import { usePathname } from "next/navigation";
import { use, useEffect, useState } from "react";
import { api, type TeamView } from "@/lib/api";

const TABS: Array<{ slug: string; label: string; adminOnly: boolean }> = [
  { slug: "", label: "overview", adminOnly: false },
  { slug: "agents", label: "agents", adminOnly: false },
  { slug: "members", label: "members", adminOnly: true },
  { slug: "roles", label: "roles", adminOnly: true },
  { slug: "credentials", label: "credentials", adminOnly: false },
  { slug: "usage", label: "usage", adminOnly: false },
  { slug: "audit", label: "audit log", adminOnly: true },
];

export default function TeamLayout({
  children,
  params,
}: {
  children: React.ReactNode;
  params: Promise<{ id: string }>;
}) {
  const { id } = use(params);
  const pathname = usePathname();
  const [team, setTeam] = useState<TeamView | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const t = await api.teams.get(id);
        if (!cancelled) {
          setTeam(t);
          setError(null);
        }
      } catch (e) {
        if (!cancelled)
          setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [id]);

  return (
    <div className="mx-auto max-w-5xl px-8 py-8">
      <header className="mb-6">
        <Link
          href="/teams"
          className="mb-2 inline-block text-sm"
          style={{ color: "var(--color-body)" }}
        >
          ← back to teams
        </Link>
        <div className="flex items-center justify-between">
          <h1 className="display-h1">{team?.name ?? "loading…"}</h1>
          {team?.is_admin && (
            <span
              className="pill"
              style={{
                background: "rgba(83, 58, 253, 0.1)",
                color: "var(--color-purple)",
              }}
            >
              admin
            </span>
          )}
        </div>
        {error && (
          <p className="mt-2 text-sm" style={{ color: "var(--color-ruby)" }}>
            {error}
          </p>
        )}
      </header>

      <nav
        className="mb-6 flex gap-1 overflow-x-auto border-b"
        style={{ borderColor: "var(--color-border-default)" }}
      >
        {TABS.filter((t) => !t.adminOnly || team?.is_admin).map((t) => {
          const href = `/teams/${id}${t.slug ? `/${t.slug}` : ""}`;
          const active = pathname === href;
          return (
            <Link
              key={t.slug}
              href={href}
              className="px-3 py-2 text-sm transition-colors"
              style={{
                borderBottom: active
                  ? "2px solid var(--color-purple)"
                  : "2px solid transparent",
                color: active
                  ? "var(--color-purple)"
                  : "var(--color-body)",
              }}
            >
              {t.label}
            </Link>
          );
        })}
      </nav>

      {children}
    </div>
  );
}
