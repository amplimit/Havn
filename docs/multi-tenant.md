# Several customers on one server

To serve several customers (tenants) from one server, give each tenant its own
unprivileged [LXD](https://canonical.com/lxd) system container and install a
separate Havn gateway inside it. Each tenant then has its own gateway, agents,
credentials, users and data. The kernel and the host's CPU, memory, disk and
network are still shared.

What was tested, with Havn v0.1.3 and LXD 6.10 on Ubuntu 24.04: the container
options under [Create a tenant](#create-a-tenant), the README install, the
systemd unit, the dashboard and the full acceptance probe suite inside the
container, isolation between two tenants, and port isolation on NICs attached
to `lxdbr0`. In that test the tenants reached the internet through a macvlan
NIC and the `lxdbr0` NICs had static addresses, so the `lxdbr0` DHCP, NAT and
firewall steps under [Prepare the host](#prepare-the-host) were not exercised,
nor was anything under
[Further hardening](#further-hardening-not-tested-with-havn).

## Why not something else

- **Several gateways directly on the host** is not supported. All gateways use
  the same cgroup root `/sys/fs/cgroup/havn` and the same `HAVN-AGENT-EGRESS`
  firewall chain, so a gateway that isolates agents refuses to start while
  another gateway that isolates agents runs on the host.
- **Docker** does not let Havn create its agent sandboxes inside an ordinary
  container. `--privileged` makes it work, but it hands the container the
  host's devices and all kernel capabilities, so escaping one tenant's agent
  sandbox gives the whole server. Docker also hides securityfs, so
  `havn doctor` cannot confirm Landlock.
- In an **unprivileged LXD container**, root inside the container is an
  ordinary uid on the host, and with `security.nesting` Havn can build its
  sandboxes inside it. An agent that escaped Havn's sandbox would still be
  confined to its tenant's container.

## Prepare the host

Use a server that runs only the tenant containers. Other services on the same
host, Docker in particular, interfere with the container network and widen
what a compromised tenant can reach.

```bash
sudo snap install lxd --channel=6/stable
sudo lxd init --minimal
```

`lxd init --minimal` creates the bridge `lxdbr0`, which hands out addresses to
the containers and NATs their traffic to the internet.

Root in a tenant container is not subject to Havn's agent egress rules. It can
reach the host's own services on the `lxdbr0` address (sshd, LXD's DNS and
DHCP, any proxy listener), a cloud provider's metadata address and any private
network the host routes to. Run a host firewall even though UFW is inactive on
a fresh Ubuntu install, allow guests only DHCP and DNS to the host, as the
restrictive UFW rules in LXD's
[firewall guide](https://canonical.com/lxd/docs/latest/howto/network_bridge_firewalld/)
do, and block whatever else tenants must not reach.

## Create a tenant

Replace `acme` with the tenant's name:

```bash
lxc init ubuntu:24.04 acme -c security.nesting=true -c security.idmap.isolated=true -c security.idmap.size=1000000
lxc config device add acme eth0 nic network=lxdbr0 name=eth0 security.port_isolation=true
lxc start acme
```

- `security.nesting=true` relaxes the container's restrictions on mounting
  `/proc` and `/sys` inside new namespaces, which Havn's agent sandboxes do.
- `security.idmap.isolated=true` gives the tenant its own range of host uids.
  Without it, all containers map to the same host uids, so a process that
  escaped one container would own the files of the others.
- `security.idmap.size=1000000` makes that range a million ids instead of
  65536. Each agent runs as a uid of its own from the upper half of the ids
  above 65535 the container maps; in a container with only 65536 agents do
  not start. LXD's default allocation of a billion host ids holds about a
  thousand such tenants. A tenant created without it gets it with
  `lxc config set acme security.idmap.size=1000000` and a restart, during
  which LXD shifts the container's files to the new range.
  The gateway records the agents' range in the container's `/etc/subuid` and
  `/etc/subgid` as `havn-agents`, so `useradd` never gives a user of the
  container subordinate ids an agent runs as.
- `security.port_isolation=true` stops direct traffic between this container
  and the other isolated containers on the bridge. It only blocks traffic
  between two isolated ports, so set it on **every** tenant: a tenant without
  it can still reach, and be reached by, all the others.

Then open a shell in the container (`lxc exec acme -- bash`) and follow
[Quick Start](../README.md#quick-start) and
[Run under systemd](../README.md#run-under-systemd) as on a dedicated server,
with a passphrase (`HAVN_AGE_KEY`) and LLM credentials of the tenant's own.
`havn setup` may report that the nested user-namespace check failed; that only
matters for capability packs whose workload nests user namespaces, such as
Chromium-based browser packs.

The gateway and the dashboard listen on 127.0.0.1 inside the container, so
neither is reachable from outside the container by default.

## Check a tenant

```bash
lxc exec acme -- havn doctor                      # no failures, Landlock enabled
lxc exec acme -- cat /proc/self/uid_map           # a host range no other tenant has
bridge -d link show master lxdbr0 | grep -c 'isolated on'   # one per running tenant
```

Run from `lxc exec`, `havn doctor` also warns that `HAVN_AGE_KEY` is not set in
that shell; the systemd unit reads it from `/etc/havn/havn.env`.

## Remove a tenant

```bash
lxc delete --force acme
```

This deletes the container with its gateway database, credentials and agent
workspaces.

## Further hardening (not tested with Havn)

- **Resource limits.** Without them one tenant's agents can use up the
  host's memory, CPU or disk for every tenant. Set container limits such as
  `lxc config set acme limits.memory=4GiB limits.cpu=2 limits.processes=2000`,
  sized to the tenant's agents. The `dir` storage pool that
  `lxd init --minimal` creates cannot cap disk usage; use a zfs or btrfs pool
  and set a root disk `size` to do that.
- **Address spoofing.** Port isolation does not stop root in one tenant from
  claiming another tenant's IP or MAC address towards the host. Add
  `security.ipv4_filtering=true security.ipv6_filtering=true` to the NIC,
  which also turns on MAC filtering.
- **Traffic routed through the host.** A tenant can send packets to another
  tenant through the host's `lxdbr0` address, which port isolation does not
  cover. Drop forwarded traffic from `lxdbr0` back to `lxdbr0` in the host
  firewall.
- **Isolation by default.** Setting the options on the default profile
  (`lxc profile set default security.nesting=true security.idmap.isolated=true security.idmap.size=1000000`
  and `lxc profile device set default eth0 security.port_isolation=true`)
  keeps a tenant created without them from being left open. A tenant created
  with the `lxc config device add ... eth0` line above has its own `eth0`,
  which overrides the profile's, so it needs the option on that line.
- **Dashboard access.** The dashboard has no login of its own. To give a
  tenant access to it, add a proxy device that listens on the host's loopback
  only, for example
  `lxc config device add acme dash proxy listen=tcp:127.0.0.1:3101 connect=tcp:127.0.0.1:3000`,
  and point an authenticating reverse proxy on the host at that port (see the
  [dashboard README](../apps/dashboard/README.md) for what the proxy must
  send). The proxy device connects to the dashboard from 127.0.0.1 inside the
  container, so that is the address to set in `HAVN_DASHBOARD_TRUSTED_PROXIES`
  there, not the address of the reverse proxy on the host. For the same
  reason, never listen on `0.0.0.0` or the `lxdbr0` address: anyone who can
  reach that port is treated as the trusted proxy.
