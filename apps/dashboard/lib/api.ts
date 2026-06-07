/**
 * Thin REST client for the gateway management API (spec §8.3, §10.3).
 *
 * Calls go through Next's `/api/:path*` proxy in dev (see `next.config.mjs`)
 * so the browser stays on a single origin. Production reverse proxies
 * handle the same hop at the edge.
 */

const BASE = "/api";

export type AgentView = {
  id: string;
  name: string;
  status: "created" | "running" | "paused" | "stopped" | "error";
  config: Record<string, unknown>;
  pid: number | null;
  created_at: string;
  updated_at: string;
  /** Set after the runtime completes its Hello/Welcome handshake. */
  connected: boolean;
};

export type MeView = {
  id: string;
  display_name: string;
  created_at: string;
  ws_token: string;
};

export type CredentialView = {
  id: string;
  scope: "user" | "team";
  scope_id: string;
  provider: string;
  priority: number;
  limits: Record<string, unknown>;
  enabled: boolean;
  created_at: string;
};

export type MemoryKind = "identity" | "preference" | "project" | "event";

export type MemoryEntry = {
  key: string;
  value: string;
  kind: MemoryKind;
  source: "user_told" | "agent_inferred";
  ttl_days: number | null;
  created_at: string;
  updated_at: string;
  recall_count: number;
  last_recalled_at: string | null;
  archived_at: string | null;
  supersedes_id: string | null;
};

export type MemoryListResponse = {
  agent_id: string;
  entries: MemoryEntry[];
  /** True when agent.db doesn't exist yet (agent never started). */
  uninitialised: boolean;
};

export type SkillView = {
  name: string;
  description: string;
  source: "bundled" | "workspace";
  pinned: boolean;
  use_count: number;
  last_used_at: string | null;
};

export type SkillListResponse = {
  agent_id: string;
  active: SkillView[];
  archived: SkillView[];
  uninitialised: boolean;
};

export type ConversationTurn = {
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  created_at: string;
};

export type ConversationResponse = {
  agent_id: string;
  channel_id: string;
  turns: ConversationTurn[];
  uninitialised: boolean;
};

export type CuratorReportFile = {
  name: string;
  size_bytes: number;
  modified_at: string | null;
  body: string;
};

export type CuratorReportsResponse = {
  agent_id: string;
  reports: CuratorReportFile[];
  uninitialised: boolean;
};

// ---- Team-management types (Phase 2 multi-tenant surface, spec §10.3) ----

export type TeamView = {
  id: string;
  name: string;
  created_at: string;
  /** True when the calling user holds this team's admin role. */
  is_admin: boolean;
};

export type TeamAgentView = {
  id: string;
  name: string;
  status: AgentView["status"];
  owner_id: string;
  owner_display_name: string;
  created_at: string;
};

export type TeamUsageEntry = {
  user_id: string;
  display_name: string;
  tokens_in: number;
  tokens_out: number;
  call_count: number;
};

export type TeamUsageResponse = {
  team_id: string;
  since: string;
  entries: TeamUsageEntry[];
};

export type MemberView = {
  user_id: string;
  display_name: string;
  role_id: string;
  role_name: string;
  joined_at: string;
};

/** Mirror of `havn_core::Policy`. JSON shape only — the dashboard
 * passes blobs through to the API; rendering details live in the
 * role-editor page. */
export type PolicyView = Record<string, unknown>;

export type RoleView = {
  id: string;
  team_id: string | null;
  name: string;
  policy: PolicyView;
  created_at: string;
};

export type EmbeddingStatus = {
  /** "disabled" | "openai" | "local" | "hrr". */
  provider: string;
  /** True when memory_search runs hybrid (vector + BM25). */
  hybrid_enabled: boolean;
  /** Raw config sub-block — model, dimensions, etc. */
  config: Record<string, unknown>;
};

export type McpServerConfig = {
  binary: string;
  args: string[];
  env: Record<string, string>;
  extra_paths_rw: string[];
  extra_paths_ro: string[];
  timeout_seconds: number;
  enabled: boolean;
};

export type McpView = {
  /** Master gate: when false, the runtime ignores `servers` entirely
   * and the agent never sees any mcp__* tools. */
  can_use_mcp: boolean;
  /** Whitelisted servers, keyed by operator-chosen name (used as
   * the prefix in `mcp__<name>__<tool>` registrations). */
  servers: Record<string, McpServerConfig>;
  /** Binaries currently installed at /usr/share/havn/mcp-servers/.
   * Empty when the dir is missing. */
  available_binaries: string[];
};

export type BootstrapView = {
  /** Persona + identity (tone, values, name, purpose). null = file
   * absent or empty. Edits take effect on the agent's next start
   * (frozen system prompt — spec §9.4). */
  system: string | null;
  /** Durable facts about the user. null = absent. Frozen-prompt rules. */
  user: string | null;
  /** Periodic-self-tick instructions. Re-read fresh on every tick
   * (spec §9.4 / §9.6 — the deliberate exception to the frozen-prompt
   * invariant), so edits take effect at the next heartbeat. */
  heartbeat: string | null;
};

export type AuditEntryView = {
  id: string;
  team_id: string | null;
  user_id: string;
  agent_id: string | null;
  action: string;
  details: Record<string, unknown>;
  created_at: string;
};

export type AuditListResponse = {
  entries: AuditEntryView[];
  next_before: string | null;
};

export class ApiError extends Error {
  constructor(
    public status: number,
    public body: string,
  ) {
    super(`gateway returned ${status}: ${body}`);
  }
}

async function request<T>(
  path: string,
  init: RequestInit = {},
): Promise<T> {
  const res = await fetch(`${BASE}${path}`, {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...(init.headers ?? {}),
    },
    cache: "no-store",
  });
  if (!res.ok) {
    throw new ApiError(res.status, await res.text());
  }
  // 204 No Content has no body.
  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

function qs(params: Record<string, string | number | undefined>): string {
  const entries = Object.entries(params).filter(
    ([, v]) => v !== undefined && v !== "",
  );
  if (entries.length === 0) return "";
  const usp = new URLSearchParams();
  for (const [k, v] of entries) usp.set(k, String(v));
  return `?${usp.toString()}`;
}

export const api = {
  me: () => request<MeView>("/me"),
  /** System-level embedding/hybrid retrieval config (spec §9.4 v0.7). */
  embedding: () => request<EmbeddingStatus>("/embedding"),
  /** Switch the active provider at runtime. The config is swapped
   * in-memory so new agent spawns pick it up at their next Welcome;
   * already-running agents keep their snapshot until restart, and
   * the change is lost when the gateway itself restarts (the
   * `~/.config/havn/config.toml` file is the canonical persistence
   * surface — operators who want the change to stick across reboots
   * edit there too). */
  patchEmbedding: (body: { provider: string; [k: string]: unknown }) =>
    request<EmbeddingStatus>("/embedding", {
      method: "PATCH",
      body: JSON.stringify(body),
    }),
  myAuditLog: (before?: string, limit = 100) =>
    request<AuditListResponse>(`/me/audit-log${qs({ before, limit })}`),

  agents: {
    list: () => request<AgentView[]>("/agents"),
    get: (id: string) => request<AgentView>(`/agents/${id}`),
    create: (name: string, config: Record<string, unknown> = {}) =>
      request<AgentView>("/agents", {
        method: "POST",
        body: JSON.stringify({ name, config }),
      }),
    /** PATCH agent — rename and/or shallow-merge config keys.
     * Pass null in a config value to clear that key. */
    patch: (
      id: string,
      patch: { name?: string; config?: Record<string, unknown | null> },
    ) =>
      request<AgentView>(`/agents/${id}`, {
        method: "PATCH",
        body: JSON.stringify(patch),
      }),
    start: (id: string) =>
      request<AgentView>(`/agents/${id}/start`, { method: "POST" }),
    stop: (id: string) =>
      request<AgentView>(`/agents/${id}/stop`, { method: "POST" }),
    delete: (id: string) =>
      request<void>(`/agents/${id}`, { method: "DELETE" }),
  },

  conversation: {
    /** Recent webchat turns for the calling user's stable channel
     * with `agentId`. Newest at the end. Empty when the agent has
     * never been started (`uninitialised: true`) or when this is the
     * user's first chat with this agent. */
    list: (agentId: string, limit = 100) =>
      request<ConversationResponse>(
        `/agents/${agentId}/conversation?limit=${limit}`,
      ),
  },

  bootstrap: {
    /** Read SOUL.md / USER.md / HEARTBEAT.md from the agent's
     * workspace (spec §5.3, §9.4 layer 2). Each field is `null` when
     * the file doesn't exist or is empty. (The `system` field name is
     * kept for backward compat; it maps to SOUL.md on disk.) */
    get: (agentId: string) =>
      request<BootstrapView>(`/agents/${agentId}/bootstrap`),
    /** Write any non-null field. Empty string is treated as "delete
     * the file" — same semantics as the runtime ("absent or empty =
     * skip this section"). SOUL.md and USER.md changes take effect
     * on the agent's next start (frozen-prompt invariant);
     * HEARTBEAT.md is re-read on every tick. */
    put: (agentId: string, patch: Partial<BootstrapView>) =>
      request<BootstrapView>(`/agents/${agentId}/bootstrap`, {
        method: "PUT",
        body: JSON.stringify(patch),
      }),
  },

  mcp: {
    /** Read the agent's MCP whitelist + `can_use_mcp` flag + the list
     * of binaries currently installed at /usr/share/havn/mcp-servers/
     * (spec §13 Phase 3). */
    get: (agentId: string) => request<McpView>(`/agents/${agentId}/mcp`),
    /** Patch the MCP config. `servers` is a full replacement when
     * present (omit to leave it untouched). Effects land on the
     * agent's next start. */
    patch: (
      agentId: string,
      patch: {
        can_use_mcp?: boolean;
        servers?: Record<string, McpServerConfig>;
      },
    ) =>
      request<McpView>(`/agents/${agentId}/mcp`, {
        method: "PATCH",
        body: JSON.stringify(patch),
      }),
  },

  memory: {
    list: (agentId: string, kind?: MemoryKind) =>
      request<MemoryListResponse>(
        `/agents/${agentId}/memory${kind ? `?kind=${kind}` : ""}`,
      ),
    /**
     * DELETE the memory row for `key`. Routes through the agent socket
     * (spec §5.2 — agent.db is single-writer). The runtime must be
     * running; offline agents return 409.
     */
    forget: (agentId: string, key: string) =>
      request<void>(
        `/agents/${agentId}/memory/${encodeURIComponent(key)}`,
        { method: "DELETE" },
      ),
  },

  skills: {
    list: (agentId: string) =>
      request<SkillListResponse>(`/agents/${agentId}/skills`),
    curatorReports: (agentId: string) =>
      request<CuratorReportsResponse>(`/agents/${agentId}/curator/reports`),
    /** Pin a skill — curator will skip it. */
    pin: (agentId: string, name: string) =>
      request<void>(
        `/agents/${agentId}/skills/${encodeURIComponent(name)}/pin`,
        { method: "POST" },
      ),
    unpin: (agentId: string, name: string) =>
      request<void>(
        `/agents/${agentId}/skills/${encodeURIComponent(name)}/unpin`,
        { method: "POST" },
      ),
  },

  credentials: {
    list: () => request<CredentialView[]>("/credentials"),
    create: (
      provider: string,
      api_key: string,
      priority = 10,
      limits: Record<string, unknown> = {},
    ) =>
      request<CredentialView>("/credentials", {
        method: "POST",
        body: JSON.stringify({ provider, api_key, priority, limits }),
      }),
    delete: (id: string) =>
      request<void>(`/credentials/${id}`, { method: "DELETE" }),
  },

  // ---- Team management (spec §10.3) ----------------------------------

  teams: {
    list: () => request<TeamView[]>("/teams"),
    get: (id: string) => request<TeamView>(`/teams/${id}`),
    create: (name: string) =>
      request<TeamView>("/teams", {
        method: "POST",
        body: JSON.stringify({ name }),
      }),
    rename: (id: string, name: string) =>
      request<TeamView>(`/teams/${id}`, {
        method: "PATCH",
        body: JSON.stringify({ name }),
      }),
    delete: (id: string) =>
      request<void>(`/teams/${id}`, { method: "DELETE" }),
    listAgents: (id: string) =>
      request<TeamAgentView[]>(`/teams/${id}/agents`),
    usage: (id: string, days = 30) =>
      request<TeamUsageResponse>(`/teams/${id}/usage${qs({ days })}`),
    auditLog: (id: string, before?: string, limit = 100) =>
      request<AuditListResponse>(
        `/teams/${id}/audit-log${qs({ before, limit })}`,
      ),

    members: {
      list: (teamId: string) =>
        request<MemberView[]>(`/teams/${teamId}/members`),
      add: (teamId: string, user_id: string, role_id: string) =>
        request<MemberView>(`/teams/${teamId}/members`, {
          method: "POST",
          body: JSON.stringify({ user_id, role_id }),
        }),
      patch: (teamId: string, userId: string, role_id: string) =>
        request<MemberView>(`/teams/${teamId}/members/${userId}`, {
          method: "PATCH",
          body: JSON.stringify({ role_id }),
        }),
      remove: (teamId: string, userId: string) =>
        request<void>(`/teams/${teamId}/members/${userId}`, {
          method: "DELETE",
        }),
    },

    roles: {
      list: (teamId: string) => request<RoleView[]>(`/teams/${teamId}/roles`),
      create: (teamId: string, name: string, policy?: PolicyView) =>
        request<RoleView>(`/teams/${teamId}/roles`, {
          method: "POST",
          body: JSON.stringify({ name, policy }),
        }),
      patch: (teamId: string, roleId: string, policy: PolicyView) =>
        request<RoleView>(`/teams/${teamId}/roles/${roleId}`, {
          method: "PATCH",
          body: JSON.stringify({ policy }),
        }),
      delete: (teamId: string, roleId: string) =>
        request<void>(`/teams/${teamId}/roles/${roleId}`, {
          method: "DELETE",
        }),
    },

    credentials: {
      list: (teamId: string) =>
        request<CredentialView[]>(`/teams/${teamId}/credentials`),
      create: (
        teamId: string,
        provider: string,
        api_key: string,
        priority = 10,
        limits: Record<string, unknown> = {},
      ) =>
        request<CredentialView>(`/teams/${teamId}/credentials`, {
          method: "POST",
          body: JSON.stringify({ provider, api_key, priority, limits }),
        }),
      patch: (
        teamId: string,
        credId: string,
        patch: { priority?: number; limits?: Record<string, unknown>; enabled?: boolean },
      ) =>
        request<CredentialView>(`/teams/${teamId}/credentials/${credId}`, {
          method: "PATCH",
          body: JSON.stringify(patch),
        }),
      delete: (teamId: string, credId: string) =>
        request<void>(`/teams/${teamId}/credentials/${credId}`, {
          method: "DELETE",
        }),
    },
  },
};
