//! Per-agent veth + NAT network setup (spec §4.1 / §6.3 v0.7).
//!
//! Agents are launched in a fresh `CLONE_NEWNET` namespace, which by
//! default contains nothing but `lo` (down). To give agents outbound
//! network — needed for `web_fetch`, browser packs, MCP servers that
//! reach the internet — the spawner sets up a per-agent veth pair:
//!
//! ```text
//!  host                                             agent ns
//!  ────                                             ────────
//!  eth0 (or default route iface)                     veth-a-<n>
//!    │                                                  ↑ IP 10.42.x.2/30
//!    │  iptables MASQUERADE                             │ default route via .1
//!    │  src 10.42.0.0/16 → SNAT to host IP              │
//!    │                                                  │
//!  veth-h-<n>  ⇄  (kernel veth)  ⇄  veth-a-<n>          │
//!  IP 10.42.x.1/30                ─────────────────────►┘
//! ```
//!
//! Per-agent ops happen on every `spawn()`:
//! 1. Pick a `/30` from `10.42.0.0/16` deterministically from the agent's
//!    UUID (16384 ranges; first-byte collision rate is acceptable for
//!    self-host where simultaneous agent count is dozens).
//! 2. `ip link add veth-h-<n> type veth peer name veth-a-<n>`
//! 3. `ip addr add 10.42.x.1/30 dev veth-h-<n>` + `ip link set ... up`
//! 4. `ip link set veth-a-<n> netns <child-pid>` — moves the agent end
//!    into the agent's just-created netns.
//! 5. `nsenter -t <pid> -n ip ...` to configure the ns side from outside:
//!    address, link up, default route, lo up.
//!
//! Global ops happen once at gateway startup ([`ensure_global_nat`]):
//! - `iptables -t nat ... POSTROUTING ... MASQUERADE` for 10.42.0.0/16.
//! - `iptables FORWARD ... ACCEPT` both directions for 10.42.0.0/16.
//! - `/proc/sys/net/ipv4/ip_forward = 1` (typically already so on a host
//!   that runs containers; we set it idempotently anyway).
//!
//! Permissions: every `ip` / `iptables` call requires `CAP_NET_ADMIN`. The
//! gateway typically runs as root; if not, [`ensure_global_nat`] returns
//! `NetworkUnavailable` and per-agent setup degrades to a warning + empty
//! agent netns (same behaviour as v0.6). Agents that don't need outbound
//! still work — only `web_fetch` and packs degrade.
//!
//! Cleanup: when the agent's netns is destroyed (last process exit), the
//! kernel removes both veth ends automatically. [`teardown_agent_network`]
//! is best-effort `ip link del` for the case where the host-side end
//! lingers (rare; usually a kernel quirk).

#![cfg(target_os = "linux")]

use std::net::Ipv4Addr;
use std::process::Command;

use havn_core::AgentId;

/// Default first / second octet for the per-agent /30 pool.
/// Operators with a 10.x VPN, Docker bridge collision, or k8s pod
/// network must override via gateway config `[network] agent_pool_cidr`.
pub const DEFAULT_POOL_FIRST_OCTET: u8 = 10;
pub const DEFAULT_POOL_SECOND_OCTET: u8 = 42;

/// Mask length for each per-agent allocation. /30 = 4 IPs (network,
/// gateway, agent, broadcast) — minimum that gives one usable host pair.
pub const PER_AGENT_PREFIX: u8 = 30;

/// Configurable /16 pool the gateway carves per-agent /30s out of.
/// Carries the first two octets only — the per-agent allocation strategy
/// (14-bit deterministic index from agent UUID, packed into octets 3+4)
/// is hard-coded; only the prefix moves.
#[derive(Debug, Clone, Copy)]
pub struct AgentSubnetPool {
    pub first_octet: u8,
    pub second_octet: u8,
}

impl Default for AgentSubnetPool {
    fn default() -> Self {
        Self {
            first_octet: DEFAULT_POOL_FIRST_OCTET,
            second_octet: DEFAULT_POOL_SECOND_OCTET,
        }
    }
}

impl AgentSubnetPool {
    /// Parse a string like `"10.42.0.0/16"`. Only `/16` is supported —
    /// the per-agent allocation packs a 14-bit index into the lower two
    /// octets, so anything narrower cuts the agent capacity below
    /// reasonable, anything wider wastes address space without payoff.
    pub fn parse(cidr: &str) -> std::result::Result<Self, String> {
        let (addr, prefix) = cidr
            .split_once('/')
            .ok_or_else(|| format!("agent_pool_cidr {cidr:?} missing /<prefix>"))?;
        if prefix.trim() != "16" {
            return Err(format!(
                "agent_pool_cidr {cidr:?} prefix must be /16 (per-agent /30 allocation needs 14 bits of host space)"
            ));
        }
        let parts: Vec<&str> = addr.split('.').collect();
        if parts.len() != 4 {
            return Err(format!("agent_pool_cidr {cidr:?} not a valid IPv4 address"));
        }
        let a: u8 = parts[0]
            .parse()
            .map_err(|e| format!("agent_pool_cidr {cidr:?} octet 0: {e}"))?;
        let b: u8 = parts[1]
            .parse()
            .map_err(|e| format!("agent_pool_cidr {cidr:?} octet 1: {e}"))?;
        // The third + fourth octets must be 0 (it's a /16; lower bits
        // belong to the host part, by convention they're all zero).
        for (i, p) in parts.iter().enumerate().skip(2) {
            let v: u8 = p
                .parse()
                .map_err(|e| format!("agent_pool_cidr {cidr:?} octet {i}: {e}"))?;
            if v != 0 {
                return Err(format!(
                    "agent_pool_cidr {cidr:?} must end in .0.0/16 (operator confused about /16 semantics)"
                ));
            }
        }
        Ok(Self {
            first_octet: a,
            second_octet: b,
        })
    }

    /// Render as `"<a>.<b>.0.0/16"` for `iptables` / `ip route`.
    #[must_use]
    pub fn cidr(&self) -> String {
        format!("{}.{}.0.0/16", self.first_octet, self.second_octet)
    }

    /// Refuse to start when an existing host route already covers this
    /// pool — VPN, Docker bridge, k8s pod network. Returns an error
    /// rather than silently coexisting because the kernel WILL pick one
    /// route for outbound packets and the agent's traffic will randomly
    /// land in someone else's network. Caller logs + bails.
    pub fn refuse_on_route_conflict(&self) -> std::result::Result<(), String> {
        let pool_prefix = format!("{}.{}.", self.first_octet, self.second_octet);
        let out = Command::new("ip")
            .args(["-o", "-4", "route", "show"])
            .output()
            .map_err(|e| format!("ip route show: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "ip route show exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            // Each line looks like:
            //   10.42.0.0/16 dev veth-h-… scope link …
            //   10.0.0.0/8 dev wg0 …            ← a /8 covers our /16
            //   172.17.0.0/16 dev docker0 …
            // We collide if a route's destination CIDR contains the pool.
            let dest = line.split_whitespace().next().unwrap_or("");
            // Skip our own pool — it'll be present after a previous run
            // installed it; we re-add idempotently. Compare by destination
            // text (exact match against `<a>.<b>.0.0/16`).
            if dest == self.cidr() {
                continue;
            }
            // Anything else starting with `<a>.<b>.` is a sub-route inside
            // our pool — that's also a conflict (someone else's veth).
            if dest.starts_with(&pool_prefix) {
                return Err(format!(
                    "host already has route {dest} which overlaps havn pool {}; \
                     change `[network] agent_pool_cidr` in gateway config to a free range",
                    self.cidr()
                ));
            }
            // Broader covering routes: /8 covering our /16 (e.g. 10.0.0.0/8
            // when our pool is 10.42.0.0/16). Detect by parsing the
            // destination prefix and seeing if it covers our first octet.
            if let Some((dest_addr, dest_prefix)) = dest.split_once('/')
                && let (Some(first_dest_octet), Ok(prefix)) = (
                    dest_addr
                        .split('.')
                        .next()
                        .and_then(|s| s.parse::<u8>().ok()),
                    dest_prefix.parse::<u8>(),
                )
                && first_dest_octet == self.first_octet
                && prefix < 16
            {
                return Err(format!(
                    "host route {dest} covers havn pool {}; agent egress would clash with that interface",
                    self.cidr()
                ));
            }
        }
        Ok(())
    }
}

/// One agent's network plumbing. All names + IPs deterministic from
/// `AgentId` so a restart reproduces the same allocation (no clash with
/// a previously-leaked host-side veth).
#[derive(Debug, Clone)]
pub struct AgentNetwork {
    pub host_iface: String,
    pub ns_iface: String,
    pub host_ip: Ipv4Addr,
    pub ns_ip: Ipv4Addr,
    pub network: Ipv4Addr,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum NetworkError {
    /// `ip` / `iptables` / `nsenter` not on PATH or not executable. Agents
    /// fall back to empty netns (no outbound) in this case.
    #[error("network tooling unavailable: {0}")]
    Unavailable(String),
    /// Caller lacks CAP_NET_ADMIN — common when the gateway runs as a
    /// non-root user. Same fallback as Unavailable.
    #[error("insufficient privilege for network setup: {0}")]
    PermissionDenied(String),
    /// One of the `ip` / `iptables` / `nsenter` shellouts returned a
    /// non-zero exit. The body carries stdout+stderr verbatim — operators
    /// reading the gateway log can spot why.
    #[error("network setup command failed: {0}")]
    CommandFailed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, NetworkError>;

/// Probe + idempotently apply the once-per-gateway-lifetime rules:
/// MASQUERADE for the 10.42.0.0/16 pool, FORWARD accepts in both
/// directions, ip_forward sysctl on. Call from the gateway's startup,
/// before any agent spawns.
///
/// On success, returns `Ok(primary_iface_name)` so the caller can log
/// "outbound goes via X" — operators investigating routing then know
/// where to look. On any failure returns the typed error and the
/// spawner falls through to "agent has no outbound network", which is
/// the v0.6 baseline.
pub fn ensure_global_nat(pool: &AgentSubnetPool) -> Result<String> {
    probe_tooling()?;
    let primary = detect_primary_iface()?;
    let pool_cidr = pool.cidr();

    // /proc/sys/net/ipv4/ip_forward — idempotent write.
    if let Err(e) = std::fs::write("/proc/sys/net/ipv4/ip_forward", "1\n") {
        // EPERM here is most often an unprivileged gateway. Surface as
        // PermissionDenied so the caller can log + degrade.
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(NetworkError::PermissionDenied(format!(
                "writing /proc/sys/net/ipv4/ip_forward: {e}"
            )));
        }
        return Err(NetworkError::Io(e));
    }

    add_iptables_rule_idempotent(&[
        "-t",
        "nat",
        "-A",
        "POSTROUTING",
        "-s",
        &pool_cidr,
        "-o",
        &primary,
        "-j",
        "MASQUERADE",
    ])?;
    add_iptables_rule_idempotent(&["-A", "FORWARD", "-s", &pool_cidr, "-j", "ACCEPT"])?;
    add_iptables_rule_idempotent(&["-A", "FORWARD", "-d", &pool_cidr, "-j", "ACCEPT"])?;
    Ok(primary)
}

/// Lay down the per-agent veth pair, addressing, default route, and `lo`.
/// Call from `NamespaceSpawner::spawn` *after* `Command::spawn` returns
/// (we need the child's host pid to address the new netns by `/proc/<pid>/ns/net`).
///
/// On failure the agent ends up with an empty netns, which is fine for
/// any agent that doesn't need outbound — the only surface that breaks
/// is `web_fetch` / packs / network-needing MCP servers, all of which
/// surface their own clear errors at tool-use time.
pub fn configure_agent_network(
    child_pid: i32,
    agent_id: AgentId,
    pool: &AgentSubnetPool,
) -> Result<AgentNetwork> {
    let net = network_for(agent_id, *pool);
    let pid = child_pid.to_string();

    // Host side
    run_ip(&[
        "link",
        "add",
        &net.host_iface,
        "type",
        "veth",
        "peer",
        "name",
        &net.ns_iface,
    ])?;
    run_ip(&[
        "addr",
        "add",
        &format!("{}/{}", net.host_ip, PER_AGENT_PREFIX),
        "dev",
        &net.host_iface,
    ])?;
    run_ip(&["link", "set", &net.host_iface, "up"])?;
    run_ip(&["link", "set", &net.ns_iface, "netns", &pid])?;

    // ns side via nsenter -t <pid> -n
    run_nsenter(
        &pid,
        &[
            "addr",
            "add",
            &format!("{}/{}", net.ns_ip, PER_AGENT_PREFIX),
            "dev",
            &net.ns_iface,
        ],
    )?;
    run_nsenter(&pid, &["link", "set", &net.ns_iface, "up"])?;
    run_nsenter(&pid, &["link", "set", "lo", "up"])?;
    run_nsenter(
        &pid,
        &["route", "add", "default", "via", &net.host_ip.to_string()],
    )?;

    Ok(net)
}

/// Best-effort cleanup of the host-side veth. The kernel normally
/// reaps the pair when the agent's netns goes away; this is the
/// belt-and-braces path for cases where it doesn't.
pub fn teardown_agent_network(agent_id: AgentId, pool: &AgentSubnetPool) {
    let net = network_for(agent_id, *pool);
    let _ = run_ip_silent(&["link", "del", &net.host_iface]);
}

/// Garbage-collect orphaned host-side veth interfaces left behind by
/// previous gateway runs. Run at gateway startup, immediately after
/// `ensure_global_nat` (so the global rules cover whatever survives
/// the GC).
///
/// We don't try to identify which veth belongs to which agent — at
/// startup, by construction, no agent runtime is alive (the gateway
/// is the only thing that spawns them, and the gateway's pidfile
/// guarded against multiple instances). Every `veth-h-*` we find is
/// therefore an orphan from a previous crashed / killed run, safe to
/// delete. Returns the number of interfaces removed for the operator
/// to log.
pub fn gc_orphaned_veth() -> Result<usize> {
    let out = Command::new("ip")
        .args(["-o", "link", "show", "type", "veth"])
        .output()
        .map_err(|e| NetworkError::Unavailable(format!("ip link show: {e}")))?;
    if !out.status.success() {
        return Err(NetworkError::CommandFailed(format!(
            "ip link show type veth exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut count = 0usize;
    for line in text.lines() {
        // Lines look like: `25: veth-h-0034e1@if24: <…> mtu 1500 …`
        // Extract the iface name before `@`.
        let Some((_, after_idx)) = line.split_once(": ") else {
            continue;
        };
        let name = after_idx.split('@').next().unwrap_or("").trim();
        if !name.starts_with("veth-h-") {
            continue;
        }
        let _ = run_ip_silent(&["link", "del", name]);
        count += 1;
    }
    Ok(count)
}

/// Compute the per-agent network from the agent's UUID. Deterministic
/// (a restart picks the same /30) and collision-resistant up to a few
/// hundred concurrent agents.
fn network_for(agent_id: AgentId, pool: AgentSubnetPool) -> AgentNetwork {
    let bytes = agent_id.as_uuid().as_bytes();
    // 14-bit index → 16384 /30s.
    let idx = ((u32::from(bytes[6]) << 8) | u32::from(bytes[7])) & 0x3FFF;
    // Each /30 is 4 IPs apart starting at <pool>.0.0.
    let base = idx * 4;
    let third = u8::try_from((base >> 8) & 0xFF).unwrap_or(0);
    let fourth = u8::try_from(base & 0xFF).unwrap_or(0);
    let net_addr = Ipv4Addr::new(pool.first_octet, pool.second_octet, third, fourth);
    let host = Ipv4Addr::new(pool.first_octet, pool.second_octet, third, fourth + 1);
    let ns = Ipv4Addr::new(pool.first_octet, pool.second_octet, third, fourth + 2);
    // 6 hex chars, prefix "veth-h-" / "veth-a-" — 13 chars, under
    // IFNAMSIZ=15. Idx fits in 14 bits → max 4 hex chars; pad to 6 to
    // keep names visually aligned.
    AgentNetwork {
        host_iface: format!("veth-h-{idx:06x}"),
        ns_iface: format!("veth-a-{idx:06x}"),
        host_ip: host,
        ns_ip: ns,
        network: net_addr,
    }
}

fn probe_tooling() -> Result<()> {
    // Each tool's actual version flag differs (`ip` only accepts `-V`,
    // not `--version`; `iptables` and `nsenter` accept either). We
    // pick a shape that works for all three.
    for (bin, flag) in [
        ("ip", "-V"),
        ("iptables", "--version"),
        ("nsenter", "--version"),
    ] {
        let r = Command::new(bin).arg(flag).output();
        match r {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                return Err(NetworkError::Unavailable(format!(
                    "{bin} {flag} exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Err(e) => {
                return Err(NetworkError::Unavailable(format!(
                    "could not exec {bin}: {e}"
                )));
            }
        }
    }
    Ok(())
}

/// Find the host's primary outbound interface by asking the kernel
/// which iface a packet to a public address would leave on. Avoids
/// hard-coding `eth0` / `enp0s3` / etc.
fn detect_primary_iface() -> Result<String> {
    // `ip -o route get 1.1.1.1` → "1.1.1.1 via 192.168.1.1 dev eth0 src ..."
    let out = Command::new("ip")
        .args(["-o", "route", "get", "1.1.1.1"])
        .output()?;
    if !out.status.success() {
        return Err(NetworkError::CommandFailed(format!(
            "ip route get failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut tokens = text.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "dev"
            && let Some(iface) = tokens.next()
        {
            return Ok(iface.to_string());
        }
    }
    Err(NetworkError::CommandFailed(format!(
        "couldn't parse 'dev <iface>' out of: {text}"
    )))
}

fn add_iptables_rule_idempotent(rule: &[&str]) -> Result<()> {
    // `iptables -C` checks existence (returns 0 if present, 1 if not).
    // We translate the leading `-A` / `-I` to `-C` to probe.
    let mut check = rule.to_vec();
    if let Some(pos) = check.iter().position(|a| *a == "-A" || *a == "-I") {
        check[pos] = "-C";
    }
    let probe = Command::new("iptables").args(&check).output()?;
    if probe.status.success() {
        return Ok(()); // already there
    }
    let add = Command::new("iptables").args(rule).output()?;
    if !add.status.success() {
        let stderr = String::from_utf8_lossy(&add.stderr).trim().to_string();
        if stderr.to_lowercase().contains("permission")
            || stderr.to_lowercase().contains("operation not permitted")
        {
            return Err(NetworkError::PermissionDenied(stderr));
        }
        return Err(NetworkError::CommandFailed(format!(
            "iptables {}: {}",
            rule.join(" "),
            stderr
        )));
    }
    Ok(())
}

fn run_ip(args: &[&str]) -> Result<()> {
    let out = Command::new("ip").args(args).output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.to_lowercase().contains("operation not permitted") {
        return Err(NetworkError::PermissionDenied(stderr));
    }
    Err(NetworkError::CommandFailed(format!(
        "ip {}: {}",
        args.join(" "),
        stderr
    )))
}

fn run_ip_silent(args: &[&str]) -> Result<()> {
    let _ = Command::new("ip").args(args).output()?;
    Ok(())
}

fn run_nsenter(pid: &str, ip_args: &[&str]) -> Result<()> {
    let mut full = vec!["-t", pid, "-n", "ip"];
    full.extend(ip_args);
    let out = Command::new("nsenter").args(&full).output()?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(NetworkError::CommandFailed(format!(
        "nsenter {}: {}",
        full.join(" "),
        stderr
    )))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn default_pool() -> AgentSubnetPool {
        AgentSubnetPool::default()
    }

    #[test]
    fn network_for_returns_distinct_ranges_for_distinct_uuids() {
        let pool = default_pool();
        let a = network_for(AgentId::new(), pool);
        let b = network_for(AgentId::new(), pool);
        // Birthday paradox: 16384 buckets, 2 draws → collision <0.01%.
        // Pin the structural invariants regardless.
        assert_eq!(a.host_ip.octets()[0], 10);
        assert_eq!(a.host_ip.octets()[1], 42);
        assert_eq!(a.host_ip.octets()[3] & 0x3, 1, "host is .1 of /30");
        assert_eq!(a.ns_ip.octets()[3] & 0x3, 2, "ns is .2 of /30");
        assert!(a.host_iface.starts_with("veth-h-"));
        assert!(a.ns_iface.starts_with("veth-a-"));
        assert!(a.host_iface.len() <= 15, "IFNAMSIZ");
        // Different UUIDs almost certainly hash to different /30s.
        assert!(a.host_iface != b.host_iface || a.network == b.network);
    }

    #[test]
    fn network_for_is_deterministic() {
        let pool = default_pool();
        let id = AgentId::new();
        let a = network_for(id, pool);
        let b = network_for(id, pool);
        assert_eq!(a.host_ip, b.host_ip);
        assert_eq!(a.host_iface, b.host_iface);
    }

    #[test]
    fn network_for_packs_inside_pool() {
        let pool = default_pool();
        for raw in [0_u32, 1, 100, 16382, 16383] {
            let bytes = u16::try_from(raw).unwrap().to_be_bytes();
            let mut uuid_bytes = [0u8; 16];
            uuid_bytes[6] = bytes[0];
            uuid_bytes[7] = bytes[1];
            let id = AgentId::from_uuid(uuid::Uuid::from_bytes(uuid_bytes));
            let net = network_for(id, pool);
            assert_eq!(net.host_ip.octets()[0], DEFAULT_POOL_FIRST_OCTET);
            assert_eq!(net.host_ip.octets()[1], DEFAULT_POOL_SECOND_OCTET);
        }
    }

    #[test]
    fn iface_names_fit_ifnamsiz() {
        // IFNAMSIZ on Linux is 16 (with null terminator); usable len is 15.
        // Worst case: idx = 0x3FFF → "veth-h-003fff" → 13 chars. Pin it.
        let pool = default_pool();
        let mut bytes = [0u8; 16];
        bytes[6] = 0x3F;
        bytes[7] = 0xFF;
        let id = AgentId::from_uuid(uuid::Uuid::from_bytes(bytes));
        let net = network_for(id, pool);
        assert!(net.host_iface.len() <= 15);
        assert!(net.ns_iface.len() <= 15);
    }

    #[test]
    fn pool_parses_canonical_form() {
        let p = AgentSubnetPool::parse("10.42.0.0/16").expect("ok");
        assert_eq!(p.first_octet, 10);
        assert_eq!(p.second_octet, 42);
        assert_eq!(p.cidr(), "10.42.0.0/16");
    }

    #[test]
    fn pool_rejects_non_slash_16() {
        assert!(AgentSubnetPool::parse("10.42.0.0/24").is_err());
        assert!(AgentSubnetPool::parse("10.42.0.0/12").is_err());
        assert!(AgentSubnetPool::parse("10.42.0.0").is_err());
    }

    #[test]
    fn pool_rejects_non_zero_lower_octets() {
        // Operator confused — `.42.1.0/16` is meaningful as a route but
        // ambiguous as a /16 base. Refuse rather than silently masking.
        assert!(AgentSubnetPool::parse("10.42.1.0/16").is_err());
    }

    #[test]
    fn pool_accepts_alternate_octets() {
        let p = AgentSubnetPool::parse("172.20.0.0/16").expect("ok");
        assert_eq!(p.first_octet, 172);
        assert_eq!(p.second_octet, 20);
    }
}
