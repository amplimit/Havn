"use client";

/**
 * Persistent sidebar — agent list (DESIGN.md sidebar pattern §10.2).
 *
 * Polls `/agents` every 5 s so status pills (`running` / `connected`) reflect
 * lifecycle changes without a full reload. WebSocket-based push lands when
 * we ship `/ws/status` (spec §8.3) — for Phase 1 polling is acceptable.
 */

import Link from "next/link";
import { usePathname, useSearchParams } from "next/navigation";
import { useEffect, useRef, useState, useSyncExternalStore } from "react";
import {
  BarChart3,
  Bot,
  Boxes,
  KeyRound,
  Menu,
  PanelLeftClose,
  PanelLeftOpen,
  Plus,
  SlidersHorizontal,
  TriangleAlert,
  Users,
  X,
  type LucideIcon,
} from "lucide-react";
import { ApiError, api, type AgentView, type TeamView } from "@/lib/api";

const POLL_MS = 5000;
const DASHBOARD_VERSION = process.env.NEXT_PUBLIC_DASHBOARD_VERSION ?? "unknown";

const ACTIVE_BG = "rgba(83, 58, 253, 0.07)";
/** Where the browser keeps whether the sidebar is narrowed to its icons. */
const NARROW_KEY = "havn.sidebar.narrow";
// Without storage, the choice lasts while the page is open.
let narrowHere = false;
const narrowListeners = new Set<() => void>();

function narrowStored(): boolean {
  try {
    const value = localStorage.getItem(NARROW_KEY);
    return value === null ? narrowHere : value === "1";
  } catch {
    return narrowHere;
  }
}

function storeNarrow(value: boolean) {
  narrowHere = value;
  try {
    localStorage.setItem(NARROW_KEY, value ? "1" : "0");
  } catch {
    // Kept for this page only.
  }
  for (const listener of narrowListeners) listener();
}

function onNarrow(listener: () => void): () => void {
  narrowListeners.add(listener);
  window.addEventListener("storage", listener);
  return () => {
    narrowListeners.delete(listener);
    window.removeEventListener("storage", listener);
  };
}
const NO_VERSION = "version not reported";

export function Sidebar() {
  const pathname = usePathname();
  const current = useSearchParams().get("id");
  const onAgent = pathname === "/agent" || pathname?.startsWith("/agent/");
  const onTeam = pathname === "/team" || pathname?.startsWith("/team/");
  const [agents, setAgents] = useState<AgentView[] | null>(null);
  const [teams, setTeams] = useState<TeamView[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [gatewayVersion, setGatewayVersion] = useState<string | null>(null);
  const [newerRelease, setNewerRelease] = useState<string | null>(null);
  // Below the md breakpoint the sidebar is a drawer behind a menu button.
  const [open, setOpen] = useState(false);
  // From md up it can be narrowed to its icons, as this browser remembers.
  const narrow = useSyncExternalStore(onNarrow, narrowStored, () => false);
  // Set after the first frame: a sidebar loaded narrow does not shrink on screen.
  const [moving, setMoving] = useState(false);
  useEffect(() => {
    const frame = requestAnimationFrame(() => setMoving(true));
    return () => cancelAnimationFrame(frame);
  }, []);
  const toggleNarrow = () => storeNarrow(!narrow);
  // Classes that apply from md up while narrowed.
  const wideOnly = narrow ? "md:sr-only" : "";
  const centred = narrow ? "md:justify-center md:px-0" : "";
  const menuButton = useRef<HTMLButtonElement>(null);
  const closeButton = useRef<HTMLButtonElement>(null);
  const wasOpen = useRef(false);

  useEffect(() => {
    if (open) {
      const behind = [document.getElementById("main"), document.querySelector(".skip-link")];
      for (const el of behind) if (el instanceof HTMLElement) el.inert = true;
      const html = document.documentElement;
      const overflow = html.style.overflow;
      html.style.overflow = "hidden";
      closeButton.current?.focus();
      const onKey = (e: KeyboardEvent) => {
        if (e.key === "Escape") setOpen(false);
      };
      document.addEventListener("keydown", onKey);
      // At md and up the drawer is the regular sidebar; a window widened
      // (or a tablet rotated) while it is open must not stay modal.
      const wide = window.matchMedia("(min-width: 48rem)");
      const onWide = (e: MediaQueryListEvent) => {
        if (e.matches) setOpen(false);
      };
      wide.addEventListener("change", onWide);
      return () => {
        wide.removeEventListener("change", onWide);
        document.removeEventListener("keydown", onKey);
        for (const el of behind) if (el instanceof HTMLElement) el.inert = false;
        html.style.overflow = overflow;
      };
    }
    if (wasOpen.current) menuButton.current?.focus();
  }, [open]);
  useEffect(() => {
    wasOpen.current = open;
  }, [open]);

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
      // Polled with the rest, so a gateway restarted on another release
      // shows up without a reload.
      try {
        const info = await api.authInfo();
        if (!cancelled) {
          setGatewayVersion(info.version ?? NO_VERSION);
          setNewerRelease(info.newer_release ?? null);
        }
      } catch (e) {
        // Gateways before /auth/info answer 404.
        if (!cancelled && e instanceof ApiError && e.status === 404) {
          setGatewayVersion(NO_VERSION);
        }
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
    <>
      <header
        inert={open}
        className="sticky top-0 z-30 flex h-15 items-center justify-between border-b px-4 md:hidden"
        style={{
          borderColor: "var(--color-border-default)",
          backgroundColor: "var(--color-surface)",
        }}
      >
        <Link href="/" style={LOGO_STYLE}>
          havn
        </Link>
        <button
          ref={menuButton}
          type="button"
          onClick={() => setOpen(true)}
          aria-label="Open navigation"
          aria-expanded={open}
          aria-controls="sidebar"
          className="-mr-2 flex h-11 w-11 items-center justify-center rounded"
          style={{ color: "var(--color-deep-navy)" }}
        >
          <Menu size={22} aria-hidden="true" />
        </button>
      </header>
      {open && (
        <div
          className="fixed inset-0 z-40 md:hidden"
          style={{ backgroundColor: "rgba(6, 27, 49, 0.35)" }}
          onClick={() => setOpen(false)}
          aria-hidden="true"
        />
      )}
      <aside
        id="sidebar"
        aria-label="Navigation"
        role={open ? "dialog" : undefined}
        aria-modal={open ? true : undefined}
        className={`fixed inset-y-0 left-0 z-50 flex w-72 max-w-[85vw] flex-col overflow-y-auto overflow-x-hidden overscroll-contain border-r transition-[translate] duration-200 md:sticky md:top-0 md:z-auto md:h-screen md:max-w-none md:shrink-0 md:translate-x-0 ${moving ? "md:transition-[width]" : ""} ${
          narrow ? "md:w-16" : "md:w-64"
        } ${
          open
            ? "translate-x-0"
            : "max-md:invisible max-md:-translate-x-full max-md:transition-[translate,visibility]"
        }`}
        style={{
          borderColor: "var(--color-border-default)",
          backgroundColor: "var(--color-surface)",
        }}
        onClick={(e) => {
          // Following a link closes the drawer.
          if ((e.target as HTMLElement).closest("a")) setOpen(false);
        }}
      >
      <div className={`flex items-center justify-between ${narrow ? "md:justify-center" : ""}`}>
        <Link href="/" className={`px-5 py-5 ${narrow ? "md:hidden" : ""}`} style={LOGO_STYLE}>
          havn
        </Link>
        <button
          type="button"
          onClick={toggleNarrow}
          aria-label={narrow ? "Widen the sidebar" : "Narrow the sidebar to icons"}
          title={narrow ? "Widen the sidebar" : "Narrow the sidebar to icons"}
          aria-expanded={!narrow}
          aria-controls="sidebar"
          className={`tool-button hidden md:inline-flex ${narrow ? "my-4" : "mr-2"}`}
        >
          {narrow ? (
            <PanelLeftOpen size={18} aria-hidden="true" />
          ) : (
            <PanelLeftClose size={18} aria-hidden="true" />
          )}
        </button>
        <button
          ref={closeButton}
          type="button"
          onClick={() => setOpen(false)}
          aria-label="Close navigation"
          className="mr-2 flex h-11 w-11 items-center justify-center rounded md:hidden"
          style={{ color: "var(--color-deep-navy)" }}
        >
          <X size={20} aria-hidden="true" />
        </button>
      </div>

      <div className={narrow ? "px-3 md:px-2" : "px-3"}>
        <SectionTitle className={wideOnly}>Agents</SectionTitle>
        <nav aria-label="Agents" className="flex flex-col gap-0.5">
          {agents === null && !error && (
            <div className="px-2 py-2 text-sm" style={{ color: "var(--color-body)" }}>
              loading…
            </div>
          )}
          {error && (
            <div role="alert" className="px-2 py-2 text-sm" style={{ color: "var(--color-ruby)" }}>
              {error}
            </div>
          )}
          {agents?.length === 0 && (
            <div className="px-2 py-2 text-sm" style={{ color: "var(--color-body)" }}>
              no agents yet
            </div>
          )}
          {agents?.map((a) => {
            const href = `/agent?id=${a.id}`;
            const active = onAgent && current === a.id;
            return (
              <Link
                key={a.id}
                href={href}
                title={narrow ? `${a.name} (${pillLabel(a.status, a.connected)})` : undefined}
                aria-current={active && pathname === "/agent" ? "page" : active ? "true" : undefined}
                className={`flex items-center justify-between gap-2 rounded px-2 py-1.5 text-sm transition-colors hover:bg-[rgba(83,58,253,0.05)] ${centred}`}
                style={{
                  backgroundColor: active ? ACTIVE_BG : undefined,
                  color: active ? "var(--color-stripe-purple)" : "var(--color-deep-navy)",
                  borderRadius: "var(--radius-tight)",
                }}
              >
                {narrow && <Initial name={a.name} live={a.status === "running" && a.connected} />}
                <span className={`truncate ${wideOnly}`}>{a.name}</span>
                <span className={`${pillClass(a.status, a.connected)} ${narrow ? "md:hidden" : ""}`}>
                  {pillLabel(a.status, a.connected)}
                </span>
              </Link>
            );
          })}
        </nav>

        <div className={narrow ? "mt-3 px-1 md:px-0" : "mt-3 px-1"}>
          <Link
            href="/agents/new"
            title={narrow ? "new agent" : undefined}
            className={`btn-ghost flex w-full items-center justify-center gap-1.5 text-sm ${narrow ? "md:px-0" : ""}`}
          >
            <Plus size={16} className="shrink-0" aria-hidden="true" />
            <span className={wideOnly}>new agent</span>
          </Link>
        </div>
      </div>

      {teams.length > 0 && (
        <div className={narrow ? "mt-3 px-3 md:px-2" : "mt-3 px-3"}>
          <SectionTitle className={wideOnly}>Teams</SectionTitle>
          <nav aria-label="Teams" className="flex flex-col gap-0.5">
            {teams.map((t) => {
              const href = `/team?id=${t.id}`;
              const active = onTeam && current === t.id;
              return (
                <Link
                  key={t.id}
                  href={href}
                  title={narrow ? t.name : undefined}
                  aria-current={active && pathname === "/team" ? "page" : active ? "true" : undefined}
                  className={`flex items-center justify-between gap-2 rounded px-2 py-1.5 text-sm transition-colors hover:bg-[rgba(83,58,253,0.05)] ${centred}`}
                  style={{
                    backgroundColor: active ? ACTIVE_BG : undefined,
                    color: active ? "var(--color-stripe-purple)" : "var(--color-deep-navy)",
                    borderRadius: "var(--radius-tight)",
                  }}
                >
                  {narrow && <Initial name={t.name} />}
                  <span className={`truncate ${wideOnly}`}>{t.name}</span>
                  {t.is_admin && (
                    <span
                      className={`pill ${narrow ? "md:hidden" : ""}`}
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

      <div className={`mt-auto flex flex-col gap-0.5 pb-4 ${narrow ? "px-3 md:px-2" : "px-3"}`}>
        <SectionTitle className={wideOnly}>Manage</SectionTitle>
        <nav aria-label="Manage" className="flex flex-col gap-0.5">
          <SidebarLink href="/" pathname={pathname} icon={Bot} label="all agents" narrow={narrow} />
          <SidebarLink href="/models" pathname={pathname} icon={Boxes} label="models" narrow={narrow} />
          <SidebarLink href="/credentials" pathname={pathname} icon={KeyRound} label="credentials" narrow={narrow} />
          <SidebarLink href="/usage" pathname={pathname} icon={BarChart3} label="usage" narrow={narrow} />
          <SidebarLink href="/teams" pathname={pathname} icon={Users} label="teams" narrow={narrow} />
          <SidebarLink href="/settings" pathname={pathname} icon={SlidersHorizontal} label="settings" narrow={narrow} />
        </nav>
        <div className={narrow ? "md:hidden" : ""}>
          <VersionLine gateway={gatewayVersion} newer={newerRelease} />
        </div>
      </div>
      </aside>
    </>
  );
}

/**
 * The page beside the sidebar: shown again with a short fade when the page
 * changes, while the sidebar stays as it is.
 */
export function PageContent({ children }: { children: React.ReactNode }) {
  const pathname = usePathname();
  return (
    <div key={pathname} className="page-enter">
      {children}
    </div>
  );
}

const LOGO_STYLE: React.CSSProperties = {
  color: "var(--color-deep-navy)",
  fontWeight: 600,
  fontSize: 22,
  letterSpacing: "-0.4px",
  lineHeight: 1,
};

/** An agent or team in the narrowed sidebar: its first letter, with a dot while it runs. */
function Initial({ name, live }: { name: string; live?: boolean }) {
  return (
    <span
      aria-hidden="true"
      className="relative hidden h-8 w-8 shrink-0 items-center justify-center rounded text-sm font-semibold md:flex"
      style={{ backgroundColor: "var(--color-surface-muted)", border: "1px solid var(--color-border-default)" }}
    >
      {name.trim().charAt(0).toUpperCase() || "?"}
      {live && (
        <span
          className="absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full"
          style={{ backgroundColor: "var(--color-success)", border: "2px solid var(--color-surface)" }}
        />
      )}
    </span>
  );
}

function SectionTitle({ children, className = "" }: { children: React.ReactNode; className?: string }) {
  return (
    <div
      className={`mb-1 px-2 py-1 text-xs font-medium uppercase tracking-wider ${className}`}
      style={{ color: "var(--color-body)" }}
    >
      {children}
    </div>
  );
}

function SidebarLink({
  href,
  pathname,
  icon: Icon,
  label,
  narrow,
}: {
  href: string;
  pathname: string | null;
  icon: LucideIcon;
  label: string;
  /** Narrowed to its icon from md up. */
  narrow: boolean;
}) {
  const active = pathname === href;
  return (
    <Link
      href={href}
      title={narrow ? label : undefined}
      aria-current={active ? "page" : undefined}
      className={`flex items-center gap-2 rounded px-2 py-1.5 text-sm transition-colors hover:bg-[rgba(83,58,253,0.05)] ${narrow ? "md:justify-center md:py-2" : ""}`}
      style={{
        backgroundColor: active ? ACTIVE_BG : undefined,
        color: active ? "var(--color-stripe-purple)" : "var(--color-deep-navy)",
        borderRadius: "var(--radius-tight)",
      }}
    >
      <Icon size={16} className="shrink-0" aria-hidden="true" />
      <span className={narrow ? "md:sr-only" : ""}>{label}</span>
    </Link>
  );
}

// The gateway serves the dashboard of its own release, so a difference
// means a page loaded before an upgrade (or a dashboard run from source).
function VersionLine({ gateway, newer }: { gateway: string | null; newer: string | null }) {
  const mismatch = gateway !== null && gateway !== DASHBOARD_VERSION;
  return (
    <div className="mt-3 px-2 text-xs tabular" style={{ color: "var(--color-body)" }}>
      <p>
        dashboard {DASHBOARD_VERSION}
        {gateway !== null && <> · gateway {gateway}</>}
      </p>
      <div role="status">
        {mismatch && (
          <p className="mt-1 flex items-start gap-1" style={{ color: "var(--color-warning-text)" }}>
            <TriangleAlert size={12} className="mt-px shrink-0" aria-hidden="true" />
            <span>
              Different releases: reload the page.
            </span>
          </p>
        )}
        {newer && (
          <p className="mt-1">
            {newer} is out: <span className="font-mono">havn update</span> on the server installs it.
          </p>
        )}
      </div>
    </div>
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
