import type { Attachment } from "./protocol";
/**
 * Thin REST client for the gateway management API (spec §8.3, §10.3).
 *
 * Calls go to `/api/*` on the dashboard's own origin: the gateway serves
 * both, and in development `server.mjs` proxies them to it.
 */

const BASE = "/api";

/** A chat command, as `GET /commands` lists it. */
export type ChatCommand = {
  name: string;
  aliases: string[];
  /** How its arguments are written; empty when it takes none. */
  args: string;
  about: string;
  /** Only for the agent's owner. */
  admin: boolean;
};

/** What a search of the caller's chats found. */
export type ChatSearch = {
  /** Chats whose title has every word, with the turn each starts at. */
  chats: { id: string; title: string; updated_at: string; first_turn: string | null }[];
  /** Messages that have every word, with an excerpt around the first. */
  turns: {
    id: string;
    chat: string;
    title: string;
    role: "user" | "assistant";
    excerpt: string;
    created_at: string;
    /** On its chat's current branch; off it, opening it moves the chat. */
    on_branch: boolean;
  }[];
};

export type AgentView = {
  id: string;
  name: string;
  status: "created" | "running" | "paused" | "stopped" | "error";
  /** Team the agent was created in; null for a personal agent. */
  team_id: string | null;
  config: Record<string, unknown>;
  pid: number | null;
  created_at: string;
  updated_at: string;
  /** Set after the runtime completes its Hello/Welcome handshake. */
  connected: boolean;
  /** Why an agent that should be up is not: the last lines its runtime
   * wrote to stderr before it exited. */
  failure: string | null;
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

/** `GET /search`: the backend the caller's agents search with. */
export type SearchStatus = {
  /** `[search] provider` in the gateway config: auto, off, or one backend. */
  provider: string;
  backend: string | null;
};

/** `/agents/{id}/web`: one agent's web access, from its next start. */
export type AgentWebView = {
  can_search_web: boolean;
  /** web_fetch: the gateway reads pages for the agent. */
  can_fetch_web: boolean;
  /** Network egress from the agent's sandbox. */
  egress_allowed: boolean;
  /** That egress goes through the gateway's proxy, which asks about new domains. */
  egress_proxy: boolean;
  /** What the owner's policy permits; false only in multi-user mode. */
  search_permitted: boolean;
  fetch_permitted: boolean;
  egress_permitted: boolean;
  /** The backend this agent would search with, or null when none is set up. */
  search_backend: string | null;
  /** The agent's own domain lists. */
  allowed_domains: string[];
  blocked_domains: string[];
  /** Added from the owner: their role's and their own lists, the operator's blocks. */
  inherited_allowed_domains: string[];
  inherited_blocked_domains: string[];
};

/** `/me/domains`: the caller's own domain lists, for all their agents. */
export type MyDomainsView = {
  allowed_domains: string[];
  blocked_domains: string[];
  /** Set by the caller's roles and the operator; not removable. */
  admin_allowed_domains: string[];
  admin_blocked_domains: string[];
};

export type AgentWebPatch = {
  can_search_web?: boolean;
  can_fetch_web?: boolean;
  egress_allowed?: boolean;
  egress_proxy?: boolean;
  allowed_domains?: string[];
  blocked_domains?: string[];
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

/** A file the agent sent, kept by the gateway. */
export type SentFile = {
  id: string;
  name: string;
  media_type: string;
  size: number;
  channel_id: string;
  conversation?: string;
  caption?: string;
  created_at: string;
};

/** How long a memory the user saves is kept once unused. */
export type MemoryExpiry = "kind_default" | "never" | { days: number };

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

/** One part of what a reply's turn did on the way. */
export type TraceItem =
  | { kind: "thinking"; text: string }
  | {
      kind: "step";
      tool: string;
      input: string;
      result: string;
      error: boolean;
      /** A subagent's own steps, for its call (issue #222). */
      steps?: TraceItem[];
      left_out?: number;
    };

/** What a reply's turn did on the way: the model's reasoning and its tool calls. */
export type TurnTrace = {
  /** How long the turn took. */
  ms: number;
  items: TraceItem[];
  /** Parts not kept. */
  left_out: number;
};

/** What a reply's folded line says of its turn. */
export type TraceSummary = { ms: number; thought: boolean; steps: number };

export type ConversationTurn = {
  id: string;
  role: "user" | "assistant" | "system" | "tool";
  content: string;
  created_at: string;
  /** Its versions: the turns in its place on other branches, itself
   * included, oldest first. */
  versions: string[];
  /** For a reply: what its turn did on the way, in short. */
  trace?: TraceSummary;
};

export type ConversationResponse = {
  agent_id: string;
  channel_id: string;
  /** The chat the turns are of; `null` when there is no active one yet. */
  conversation: string | null;
  turns: ConversationTurn[];
  /** Messages of the chat not answered yet, the one being answered included. */
  waiting?: number;
  uninitialised: boolean;
  /** The chat's own model and thinking choice, when it has them. */
  model?: string;
  thinking?: string;
};

/** One of the caller's chats with an agent. */
export type ChatSummary = {
  id: string;
  title: string;
  created_at: string;
  updated_at: string;
  /** New messages without a chosen chat continue this one. */
  active: boolean;
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
  /** True when the caller's role in the team allows another agent there. */
  can_create_agents: boolean;
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

/** How a model thinks, as its admin set it. */
export type ThinkingSpec = {
  style: "toggle" | "effort" | "budget";
  /** For `effort`: the levels it takes. */
  levels?: string[];
  /** For `budget`: the token budgets offered. */
  budgets?: number[];
  /** A chat's choice when it made none. */
  default?: string;
  /** It always thinks: off is not offered. */
  mandatory?: boolean;
};

/** What a chat may choose for a model with `spec`, off first unless it must think. */
export function thinkingChoices(spec: ThinkingSpec): string[] {
  const out = spec.mandatory ? [] : ["off"];
  if (spec.style === "toggle") out.push("on");
  if (spec.style === "effort") out.push(...(spec.levels ?? []));
  if (spec.style === "budget") out.push(...(spec.budgets ?? []).map(String));
  return out;
}

export type ModelEntry = {
  id: string;
  label?: string;
  max_output_tokens?: number;
  /** Shown images: always (true), never (false), or tried when unset. */
  vision?: boolean;
  /** How it thinks; absent, it is not asked to. */
  thinking?: ThinkingSpec;
  /** What its tokens cost, in US dollars per million (issue #235). */
  price?: { input: number; output: number; cache_read?: number; cache_write?: number };
};

/** One line of a usage table (issue #235): a day, an agent, a model or a user. */
export type UsageBucket = {
  key: string;
  label: string;
  calls: number;
  tokens: number;
  /** US dollars of its priced calls, at the prices set now. */
  cost: number;
  /** Some of its calls are of a model without a price. */
  unpriced: boolean;
};

export type UsageView = {
  since: string;
  days: number;
  currency: string;
  /** Whose: a user's id, or `all`. */
  user: string;
  /** Whether the caller may see others' (the gateway's owner). */
  everyone: boolean;
  users: { user_id: string; display_name: string }[];
  calls: number;
  tokens: number;
  cost: number;
  unpriced_tokens: number;
  /** Calls still running or unreported, counted at their reserved most. */
  unsettled_calls: number;
  searches: number;
  by_day: UsageBucket[];
  by_agent: UsageBucket[];
  by_model: UsageBucket[];
  by_user: UsageBucket[];
};

/** What a model check found. */
export type ModelCheck = {
  outcome: "reasoning" | "no_reasoning" | "error";
  reasoning?: string;
  reply?: string;
  error?: string;
};

/** The models an agent may use, for a chat's model menu. */
export type AgentModels = {
  /** The agent's own model; null for the gateway's default. */
  agent_model: string | null;
  default_model: string | null;
  models: ModelEntry[];
};

export type ModelsView = {
  /** Pass back on put: the gateway refuses it if the list changed since. */
  version: string;
  /** Default set in config.toml; null when the first entry is the default. */
  default_model: string | null;
  effective_default: string | null;
  /** The model that writes chats' titles; null for each agent's own. */
  title_model: string | null;
  models: ModelEntry[];
  /** False with trust_header on: the list is then edited in config.toml only. */
  editable: boolean;
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

// The gateway answers errors with `{"error": "<kind>: <message>"}`; the
// kind repeats the status, so only the message is shown.
const ERROR_KINDS = /^(bad request|forbidden|conflict): /i;
const MAX_ERROR_LENGTH = 300;

function statusMessage(status: number): string {
  if (status === 401) return "you are not signed in; reload the page to sign in again";
  if (status === 403) return "you are not allowed to do this";
  if (status === 502 || status === 503 || status === 504) return "the gateway is not reachable";
  return `request failed (${status})`;
}

// The gateway's `{"error": ...}` message, or a short plain-text body (the
// dashboard server's own errors, request extractor errors). An HTML page
// from a reverse proxy or SSO login falls back to the status.
export function readableError(status: number, body: string, contentType = ""): string {
  const text = body.trim();
  let message = "";
  if (contentType.includes("json") || text.startsWith("{")) {
    try {
      const parsed: unknown = JSON.parse(text);
      if (parsed && typeof parsed === "object" && "error" in parsed) {
        const { error } = parsed as { error: unknown };
        if (typeof error === "string") message = error.replace(ERROR_KINDS, "");
      }
    } catch {
      // Not the gateway's JSON; fall through to the status.
    }
  } else if (!contentType.includes("html") && !text.startsWith("<")) {
    message = text;
  }
  if (!message) return statusMessage(status);
  return message.length > MAX_ERROR_LENGTH
    ? `${message.slice(0, MAX_ERROR_LENGTH)}…`
    : message;
}

export class ApiError extends Error {
  constructor(
    public status: number,
    public body: string,
    contentType = "",
    message?: string,
  ) {
    super(message ?? readableError(status, body, contentType));
  }
}

export type AuthInfo = {
  identity_header: string;
  trust_header: boolean;
  /** Absent on gateways that do not report their version. */
  version?: string;
  /** A newer release the gateway found, as its tag; null when none. */
  newer_release?: string | null;
};

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
    throw new ApiError(res.status, await res.text(), res.headers.get("content-type") ?? "");
  }
  // 204 No Content has no body.
  if (res.status === 204) return undefined as T;
  // A sign-in page from an SSO proxy whose session expired arrives as 200.
  const type = res.headers.get("content-type") ?? "";
  if (!type.includes("json")) {
    throw new ApiError(
      res.status,
      await res.text(),
      type,
      "the response was not from the gateway; your sign-in may have expired, reload the page",
    );
  }
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

/** `/me/timezone`: the zone the caller's agents tell the time in. */
export type TimezoneView = {
  timezone: string;
  /** The caller chose it; otherwise it is the gateway host's. */
  chosen: boolean;
  host: string;
};

/** A webhook of an agent: an address outside systems call (issue #236). */
export type WebhookView = {
  id: string;
  name: string;
  /** Where it is called, under the gateway's `listen` address. */
  path: string;
  instructions: string;
  /** `web`, `last`, or a channel's name. */
  deliver: string;
  created_at: string;
  last_called_at: string | null;
  calls: number;
  /** Only when it was just made: not shown again. */
  secret?: string;
};

/** A scheduled task (cron job) of an agent. */
export type CronJobView = {
  id: string;
  name: string;
  prompt: string;
  summary: string;
  next_run_at: string;
  deliver: string;
  enabled: boolean;
  last_run?: { started_at: string; error: string | null };
};

/** `/agents/{id}/approvals`: tools whose every call waits for approval. */
export type ApprovalsView = { tools: string[]; inherited: string[] };

/** Whether a model titles the agent's new chats. */
export type TitlesView = {
  enabled: boolean;
  /** Off by the owner's role: cannot be turned on here. */
  locked: boolean;
  /** The model that writes them; empty for the agent's own. */
  model: string;
};

/**
 * What the owner always allows in a conversation not theirs (issue #255): a
 * tool's results (`input` empty) or one exact call.
 */
export type Access = {
  scope: string;
  tool: string;
  input: string;
  channel?: string;
  account?: string;
  sender?: string;
  /** What not to say there; empty for nothing. */
  rule: string;
  allowed_at: string;
};

/** One capability that costs model calls; locked when the owner's role has it off. */
export type Capability = { enabled: boolean; locked: boolean };

/** What an agent may do beyond its own turn (issue #222). */
export type CapabilitiesView = { subagents: Capability; agent_queries: Capability };

/** A one-time code that links a chat-app account to the caller (issue #182). */
export type PairingCode = {
  /** The invite's id, for an invite code made on the access page. */
  id?: string;
  channel: string;
  account_id: string;
  code: string;
  /** Opens the bot with the code, when the bot's username is known. */
  link?: string;
  expires_at: string;
};

/** Who can reach one of an agent's bots (issue #231). */
export type AccountAccess = {
  channel: string;
  account_id: string;
  dm_policy: "allowlist" | "open";
  /** Listed in config.toml, removed only there. */
  allow_from: string[];
  /** `yours`: connected to the owner's Havn account, which removing does not shut out. */
  allowed: { sender_id: string; since: string; by_code: boolean; connected: boolean; yours: boolean }[];
  invites: { id: string; status: "active" | "used" | "expired" | "revoked"; created_at: string; expires_at: string; used_by?: string }[];
  /** `group`: a group turned away (issue #263), with what it is called and the server it is part of (issue #287). */
  refused: {
    sender_id: string;
    group: boolean;
    first_at: string;
    last_at: string;
    count: number;
    said: string;
    title?: string;
    space?: { id: string; title: string };
  }[];
  group_policy: "off" | "allowlist" | "open";
  /** Groups in config.toml, removed only there. */
  allow_groups: string[];
  /** Groups let in by the owner (`owner`), or used while the account was open (`open`). */
  groups: {
    group_id: string;
    title: string;
    source: "owner" | "open";
    since: string;
    /** Whether the bot sees every message there; null when its adapter has not said since the gateway started. */
    sees_all: boolean | null;
    /** A server let in as a whole, every channel in it (issue #287). */
    space: boolean;
    /** The server a channel let in is part of. */
    in_space?: { id: string; title: string };
  }[];
};

export type ChannelAccessView = {
  accounts: AccountAccess[];
  notify_unpaired: boolean;
  /** Whether the bots' policies can be set here: not with trust_header, where config.toml sets them. */
  editable: boolean;
};

/** A change of one bot's policies; what is left out stays as it is. */
export type PolicyChange = {
  dm_policy?: AccountAccess["dm_policy"];
  group_policy?: AccountAccess["group_policy"];
};

/** One sender of one of an agent's bots. */
export type SenderRef = { channel: string; account_id: string; sender_id: string; group?: boolean };

/** One of the caller's chats with an agent in a chat app they linked (issue #184). */
export type ChannelChat = ChatSummary & {
  channel: string;
  account_id: string;
  sender_id: string;
  /** Pass back to read it, switch its versions or move it to the web. */
  scope: string;
};

/** A chat-app account linked to the caller. */
export type ChannelLinkView = {
  channel: string;
  sender_id: string;
  linked_at: string;
};

/** Whether an agent's workspace can be browsed (issue #177). */
export type WorkspaceAccess = {
  /** `[workspace] browse` in the gateway's config.toml. */
  gateway: boolean;
  /** The owner's switch for this agent. */
  agent: boolean;
  /** Both: the workspace page is there. */
  enabled: boolean;
};

export type WorkspaceEntry = {
  name: string;
  kind: "dir" | "file" | "link" | "other";
  size?: number;
  /** Unix seconds. */
  modified?: number;
  /** Where a link points, as written; never followed. */
  target?: string;
};

export type WorkspaceListing = {
  path: string;
  entries: WorkspaceEntry[];
  /** More entries than were listed. */
  truncated: boolean;
};

/** The gateway's workspace browsing switch. */
export type BrowseView = {
  browse: boolean;
  /** The dashboard may change it (not with trust_header). */
  editable: boolean;
};

/** Where a file of agent `agentId`'s workspace is fetched or downloaded. */
export function workspaceFileUrl(agentId: string, path: string): string {
  return `${BASE}/agents/${agentId}/workspace/file?path=${encodeURIComponent(path)}`;
}

export const api = {
  /** What model calls used (issue #235): the caller's, or `user`'s (`all`) for the gateway's owner. */
  usage: (days = 30, user?: string) => request<UsageView>(`/usage${qs({ days, user })}`),
  me: () => request<MeView>("/me"),
  timezone: () => request<TimezoneView>("/me/timezone"),
  channels: {
    pair: (agentId: string) =>
      request<PairingCode[]>(`/agents/${agentId}/pairing`, { method: "POST" }),
    links: () => request<ChannelLinkView[]>("/me/channel-links"),
    /** The caller's chats with `agentId` in the chat apps they linked. */
    chats: (agentId: string) => request<ChannelChat[]>(`/agents/${agentId}/channel-chats`),
    /** Continue channel chat `chat` on the web; there is no way back. */
    move: (agentId: string, chat: string, scope: string) =>
      request<void>(`/agents/${agentId}/chats/${encodeURIComponent(chat)}/move`, {
        method: "POST",
        body: JSON.stringify({ scope }),
      }),
    unlink: (channel: string, sender: string) =>
      request<void>(
        `/me/channel-links?channel=${encodeURIComponent(channel)}&sender=${encodeURIComponent(sender)}`,
        { method: "DELETE" },
      ),
    /** Who can reach the agent's bots (issue #231). */
    access: (agentId: string) => request<ChannelAccessView>(`/agents/${agentId}/channel-access`),
    invite: (agentId: string, channel: string, account_id: string, hours: number) =>
      request<PairingCode>(`/agents/${agentId}/channel-access/invites`, {
        method: "POST",
        body: JSON.stringify({ channel, account_id, hours }),
      }),
    revokeInvite: (agentId: string, invite: string) =>
      request<ChannelAccessView>(`/agents/${agentId}/channel-access/invites/${encodeURIComponent(invite)}`, {
        method: "DELETE",
      }),
    allow: (agentId: string, who: SenderRef) =>
      request<ChannelAccessView>(`/agents/${agentId}/channel-access/senders`, {
        method: "POST",
        body: JSON.stringify(who),
      }),
    remove: (agentId: string, who: SenderRef) =>
      request<ChannelAccessView>(`/agents/${agentId}/channel-access/senders`, {
        method: "DELETE",
        body: JSON.stringify(who),
      }),
    forget: (agentId: string, who: SenderRef) =>
      request<ChannelAccessView>(`/agents/${agentId}/channel-access/refused`, {
        method: "DELETE",
        body: JSON.stringify(who),
      }),
    policy: (agentId: string, account: Pick<AccountAccess, "channel" | "account_id">, change: PolicyChange) =>
      request<ChannelAccessView>(`/agents/${agentId}/channel-access/policy`, {
        method: "PUT",
        body: JSON.stringify({ channel: account.channel, account_id: account.account_id, ...change }),
      }),
  },
  workspace: {
    access: (agentId: string) => request<WorkspaceAccess>(`/agents/${agentId}/workspace/access`),
    setAccess: (agentId: string, enabled: boolean) =>
      request<WorkspaceAccess>(`/agents/${agentId}/workspace/access`, {
        method: "PUT",
        body: JSON.stringify({ enabled }),
      }),
    list: (agentId: string, path: string) =>
      request<WorkspaceListing>(`/agents/${agentId}/workspace?path=${encodeURIComponent(path)}`),
    browse: () => request<BrowseView>("/settings/workspace"),
    setBrowse: (browse: boolean) =>
      request<BrowseView>("/settings/workspace", { method: "PUT", body: JSON.stringify({ browse }) }),
  },
  titles: {
    get: (agentId: string) => request<TitlesView>(`/agents/${agentId}/titles`),
    put: (agentId: string, enabled: boolean) =>
      request<TitlesView>(`/agents/${agentId}/titles`, {
        method: "PUT",
        body: JSON.stringify({ enabled }),
      }),
  },
  access: {
    get: (agentId: string) => request<{ allowed: Access[] }>(`/agents/${agentId}/access`),
    put: (agentId: string, allowed: Pick<Access, "scope" | "tool" | "input">, rule: string | null) =>
      request<{ allowed: Access[] }>(`/agents/${agentId}/access`, {
        method: "PUT",
        body: JSON.stringify({ scope: allowed.scope, tool: allowed.tool, input: allowed.input, rule }),
      }),
  },
  capabilities: {
    get: (agentId: string) => request<CapabilitiesView>(`/agents/${agentId}/capabilities`),
    put: (agentId: string, change: Partial<Record<keyof CapabilitiesView, boolean>>) =>
      request<CapabilitiesView>(`/agents/${agentId}/capabilities`, {
        method: "PUT",
        body: JSON.stringify(change),
      }),
  },
  approvals: {
    get: (agentId: string) => request<ApprovalsView>(`/agents/${agentId}/approvals`),
    put: (agentId: string, tools: string[]) =>
      request<ApprovalsView>(`/agents/${agentId}/approvals`, {
        method: "PUT",
        body: JSON.stringify({ tools }),
      }),
  },
  cron: {
    list: (agentId: string) => request<CronJobView[]>(`/agents/${agentId}/cron`),
    setEnabled: (agentId: string, jobId: string, enabled: boolean) =>
      request<CronJobView>(`/agents/${agentId}/cron/${jobId}`, {
        method: "PATCH",
        body: JSON.stringify({ enabled }),
      }),
    remove: (agentId: string, jobId: string) =>
      request<void>(`/agents/${agentId}/cron/${jobId}`, { method: "DELETE" }),
    runNow: (agentId: string, jobId: string) =>
      request<{ run_id: string }>(`/agents/${agentId}/cron/${jobId}/run`, { method: "POST" }),
  },
  webhooks: {
    list: (agentId: string) => request<WebhookView[]>(`/agents/${agentId}/webhooks`),
    create: (agentId: string, made: { name: string; instructions: string; deliver: string }) =>
      request<WebhookView>(`/agents/${agentId}/webhooks`, {
        method: "POST",
        body: JSON.stringify(made),
      }),
    update: (agentId: string, id: string, change: { instructions: string; deliver: string }) =>
      request<WebhookView>(`/agents/${agentId}/webhooks/${id}`, {
        method: "PATCH",
        body: JSON.stringify(change),
      }),
    remove: (agentId: string, id: string) =>
      request<void>(`/agents/${agentId}/webhooks/${id}`, { method: "DELETE" }),
  },
  setTimezone: (timezone: string | null) =>
    request<TimezoneView>("/me/timezone", {
      method: "PUT",
      body: JSON.stringify({ timezone }),
    }),
  authInfo: () => request<AuthInfo>("/auth/info"),
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
  search: {
    get: () => request<SearchStatus>("/search"),
    agent: (agentId: string) => request<AgentWebView>(`/agents/${agentId}/web`),
    patchAgent: (agentId: string, patch: AgentWebPatch) =>
      request<AgentWebView>(`/agents/${agentId}/web`, {
        method: "PATCH",
        body: JSON.stringify(patch),
      }),
    myDomains: () => request<MyDomainsView>("/me/domains"),
    putMyDomains: (lists: { allowed_domains: string[]; blocked_domains: string[] }) =>
      request<MyDomainsView>("/me/domains", {
        method: "PUT",
        body: JSON.stringify(lists),
      }),
  },

  models: {
    get: () => request<ModelsView>("/llm/models"),
    /** A very short request to `entry`, saved or not, with its thinking at its default. */
    check: (entry: ModelEntry) =>
      request<ModelCheck>("/llm/models/check", { method: "POST", body: JSON.stringify({ entry }) }),
    /** The models agent `agentId` may use. */
    forAgent: (agentId: string) => request<AgentModels>(`/agents/${agentId}/models`),
    /** Replaces the whole list; the gateway writes it to config.toml. */
    put: (body: {
      version: string;
      default_model: string | null;
      title_model: string | null;
      models: ModelEntry[];
    }) =>
      request<ModelsView>("/llm/models", {
        method: "PUT",
        body: JSON.stringify(body),
      }),
  },
  myAuditLog: (before?: string, limit = 100) =>
    request<AuditListResponse>(`/me/audit-log${qs({ before, limit })}`),

  agents: {
    list: () => request<AgentView[]>("/agents"),
    get: (id: string) => request<AgentView>(`/agents/${id}`),
    create: (
      name: string,
      config: Record<string, unknown> = {},
      teamId: string | null = null,
    ) =>
      request<AgentView>("/agents", {
        method: "POST",
        body: JSON.stringify({ name, config, team_id: teamId }),
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
    list: (agentId: string, limit = 100, conversation?: string, scope?: string) =>
      request<ConversationResponse>(
        `/agents/${agentId}/conversation?limit=${limit}` +
          (conversation ? `&conversation=${encodeURIComponent(conversation)}` : "") +
          (scope ? `&scope=${encodeURIComponent(scope)}` : ""),
      ),
  },

  /** The chat commands the gateway answers. */
  commands: () => request<ChatCommand[]>("/commands"),
  chats: {
    /** The caller's chats with `agentId`, most recently used first. */
    list: (agentId: string) => request<ChatSummary[]>(`/agents/${agentId}/chats`),
    /** Titles and messages of the caller's chats with `agentId` that have every word of `q`. */
    search: (agentId: string, q: string) =>
      request<ChatSearch>(`/agents/${agentId}/search?q=${encodeURIComponent(q)}`),
    rename: (agentId: string, chat: string, title: string) =>
      request<void>(`/agents/${agentId}/chats/${encodeURIComponent(chat)}`, {
        method: "PATCH",
        body: JSON.stringify({ title }),
      }),
    delete: (agentId: string, chat: string) =>
      request<void>(`/agents/${agentId}/chats/${encodeURIComponent(chat)}`, {
        method: "DELETE",
      }),
    /** A chat's own model and thinking choice; `null` goes back to the default. */
    settings: (agentId: string, chat: string, settings: { model?: string | null; thinking?: string | null }) =>
      request<void>(`/agents/${agentId}/chats/${encodeURIComponent(chat)}`, {
        method: "PATCH",
        body: JSON.stringify(settings),
      }),
    /** What reply `turn` of `chat` did on the way. */
    trace: (agentId: string, chat: string, turn: string, scope?: string) =>
      request<TurnTrace>(
        `/agents/${agentId}/chats/${encodeURIComponent(chat)}/turns/${encodeURIComponent(turn)}/trace` +
          (scope ? `?scope=${encodeURIComponent(scope)}` : ""),
      ),
    /** Continue `chat` from the branch of `turn`, at its newest turn. */
    branch: (agentId: string, chat: string, turn: string, scope?: string) =>
      request<void>(`/agents/${agentId}/chats/${encodeURIComponent(chat)}/branch`, {
        method: "POST",
        body: JSON.stringify(scope ? { turn, scope } : { turn }),
      }),
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

  outbox: {
    /** The files the agent sent in chat `conversation`, oldest first. */
    list: (agentId: string, conversation?: string) =>
      request<SentFile[]>(
        `/agents/${agentId}/outbox${conversation ? `?conversation=${encodeURIComponent(conversation)}` : ""}`,
      ),
  },

  files: {
    /** Upload `file` to the agent's inbox for a chat message. */
    upload: async (agentId: string, file: File): Promise<Attachment> => {
      const res = await fetch(
        `${BASE}/agents/${agentId}/files?name=${encodeURIComponent(file.name)}`,
        {
          method: "POST",
          body: file,
          // A page on another site cannot set this header, so it cannot
          // post files in the owner's name.
          headers: { "Content-Type": file.type || "application/octet-stream", "X-Havn-Upload": "1" },
          cache: "no-store",
        },
      );
      if (!res.ok) {
        throw new ApiError(res.status, await res.text(), res.headers.get("content-type") ?? "");
      }
      return (await res.json()) as Attachment;
    },
  },

  memory: {
    /** Active entries, at most `limit` (the gateway's default 200, up to 1000). */
    list: (agentId: string, kind?: MemoryKind, limit?: number) => {
      const params = new URLSearchParams();
      if (kind) params.set("kind", kind);
      if (limit) params.set("limit", String(limit));
      const query = params.toString();
      return request<MemoryListResponse>(`/agents/${agentId}/memory${query ? `?${query}` : ""}`);
    },
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
    /** Undo a change a chat showed: take entry `after` out and bring
     * entry `before` back. 409 when the memory changed since. */
    undo: (agentId: string, key: string, before?: string, after?: string) =>
      request<void>(`/agents/${agentId}/memory/${encodeURIComponent(key)}/undo`, {
        method: "POST",
        body: JSON.stringify({ before: before ?? null, after: after ?? null }),
      }),
    /** Add or edit the memory under `key`, as something the user said. */
    save: (
      agentId: string,
      key: string,
      entry: { value: string; kind: MemoryKind; expiry: MemoryExpiry },
    ) =>
      request<MemoryEntry>(`/agents/${agentId}/memory/${encodeURIComponent(key)}`, {
        method: "PUT",
        body: JSON.stringify(entry),
      }),
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

export const LLM_PROVIDERS: readonly string[] = ["anthropic", "openai", "openrouter", "gemini"];

/** Web search backends whose keys are stored as `search:<name>`. */
export const SEARCH_PROVIDERS: readonly string[] = ["brave", "tavily", "exa"];

export function isSearchCredential(credentialProvider: string): boolean {
  return credentialProvider.startsWith("search:");
}

// Credentials are stored as `llm:<provider>` or a bare name; channel and
// SaaS tokens are not LLM keys.
export function llmProvider(credentialProvider: string): string | null {
  const name = credentialProvider.startsWith("llm:")
    ? credentialProvider.slice(4)
    : credentialProvider;
  return LLM_PROVIDERS.includes(name) ? name : null;
}

export function modelProvider(id: string | null | undefined): string | null {
  const provider = id?.split(":")[0];
  return provider ? provider : null;
}

export type KeyScopes = { own: Set<string>; teams: Map<string, Set<string>> };

function enabledProviders(creds: CredentialView[]): Set<string> {
  const out = new Set<string>();
  for (const c of creds) {
    const provider = llmProvider(c.provider);
    if (c.enabled && provider) out.add(provider);
  }
  return out;
}

// Rejects when a list cannot be read, so callers show no coverage rather
// than a false gap; only a team the user has just left (403) counts as empty.
export async function keyScopes(teams: TeamView[]): Promise<KeyScopes> {
  const [own, ...shared] = await Promise.all([
    api.credentials.list(),
    ...teams.map((t) =>
      api.teams.credentials.list(t.id).catch((e: unknown) => {
        if (e instanceof ApiError && e.status === 403) return [] as CredentialView[];
        throw e;
      }),
    ),
  ]);
  return {
    own: enabledProviders(own),
    teams: new Map(teams.map((t, i) => [t.id, enabledProviders(shared[i])])),
  };
}

// As the gateway resolves keys: the owner's own, plus the agent's team, or
// every team the owner is in for an agent without a team.
export function reachable(scopes: KeyScopes, teamId: string | null): Set<string> {
  const out = new Set(scopes.own);
  const teams = teamId ? [scopes.teams.get(teamId)] : [...scopes.teams.values()];
  for (const set of teams) for (const p of set ?? []) out.add(p);
  return out;
}
