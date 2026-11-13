# Several customers on one server

To serve several customers (tenants) from one server, give each tenant its own
unprivileged [LXD](https://canonical.com/lxd) system container and install a
separate Havn gateway inside it. Each tenant then has its own gateway, agents,
credentials, users and data, and nothing is shared but the kernel.

This setup was tested with Havn v0.1.3 and LXD 6.10 on Ubuntu 24.04: the README
install, the systemd unit, the dashboard and the full acceptance probe suite
work unchanged inside the container, and `havn doctor` reports every check ok.

## Why not something else

- **Several gateways directly on the host** is not supported. All gateways use
  the same cgroup root `/sys/fs/cgroup/havn` and the same `HAVN-AGENT-EGRESS`
  firewall chain, and a starting gateway removes agent cgroups it does not
  track, which kills the other gateway's agents
  ([#80](https://github.com/amplimit/havn/issues/80)).
- **Docker** does not let Havn create its agent sandboxes (user namespaces,
  cgroups, iptables rules) inside an ordinary container. `--privileged` makes
  it work, but then root in the container can read the host disks and load
  kernel modules, so escaping one tenant's agent sandbox gives the whole
  server. Docker also hides securityfs, so `havn doctor` cannot confirm
  Landlock.
- In an **unprivileged LXD container**, root inside the container is an
  ordinary uid on the host, and `security.nesting` lets Havn build its sandboxes
  inside it. An agent that escaped Havn's sandbox would still be confined to
  its tenant's container.

## Prepare the host

Use a server that runs only the tenant containers. Other services on the same
host, Docker in particular, interfere with the container network and widen
what a compromised tenant can reach.

```bash
sudo snap install lxd
sudo lxd init --minimal
```

`lxd init --minimal` creates the bridge `lxdbr0`, which hands out addresses to
the containers and NATs their traffic to the internet. If UFW is active, it
blocks that traffic; allow only DHCP, DNS and outbound forwarding with the
restrictive rules in LXD's
[firewall guide](https://canonical.com/lxd/docs/latest/howto/network_bridge_firewalld/).
Root in a tenant container is not subject to the agent egress rules, so it can
reach anything the host routes to, such as a cloud provider's metadata
address or a private network; block those in the host firewall if tenants
must not reach them.

## Create a tenant

Replace `acme` with the tenant's name:

```bash
lxc init ubuntu:24.04 acme -c security.nesting=true -c security.idmap.isolated=true
lxc config device add acme eth0 nic network=lxdbr0 name=eth0 security.port_isolation=true
lxc start acme
```

- `security.nesting=true` lets Havn create user namespaces, cgroups and
  firewall rules for its agents inside the container.
- `security.idmap.isolated=true` gives the tenant its own range of host uids.
  Without it, all containers map to the same host uids, so a process that
  escaped one container would own the files of the others.
- `security.port_isolation=true` stops this tenant's container from talking to
  other containers on the bridge. It only blocks traffic between two isolated
  ports, so set it on **every** tenant: a tenant without it can still reach,
  and be reached by, all the others.

Then open a shell in the container (`lxc exec acme -- bash`) and follow
[Quick Start](../README.md#quick-start) and
[Run under systemd](../README.md#run-under-systemd) as on a dedicated server,
with a passphrase (`HAVN_AGE_KEY`) and LLM credentials of the tenant's own.
`havn setup` may report that the nested user-namespace check failed; that only
matters for capability packs whose workload nests user namespaces, such as
Chromium-based browser packs.

The gateway and the dashboard listen on 127.0.0.1 inside the container, so
nothing in a tenant is reachable from outside it by default. To give a tenant
access to its dashboard, forward a host port to the container's dashboard with
an LXD [proxy device](https://canonical.com/lxd/docs/latest/reference/devices_proxy/)
and put an authenticating reverse proxy in front of it, as described in the
[dashboard README](../apps/dashboard/README.md).

## Check a tenant

```bash
lxc exec acme -- havn doctor                      # no failures, Landlock enabled
lxc exec acme -- cat /proc/self/uid_map           # a host range no other tenant has
bridge -d link show master lxdbr0 | grep -c 'isolated on'   # one per tenant
```

## Remove a tenant

```bash
lxc delete --force acme
```

This deletes the container with its gateway database, credentials and agent
workspaces.
