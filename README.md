# gantry-nautobot

A [gantry](https://github.com/invite-networks/gantry) plugin that keeps
Nautobot in step with what gantry runs, and hands out pod addresses from
Nautobot IPAM.

- **On `gantry up`** it records each pod, its interfaces and IP addresses,
  its containers (image, state, restart policy, command, limits, labels)
  and their volumes, through the
  [invite-nautobot](https://github.com/invite-networks/invite-nautobot) app.
  A service removed from the project is marked `down`, and a removed mount
  is deleted.
- **On `gantry down`** it marks the pods and their containers `down`.
  Nautobot stamps State Changed, so a cleanup job can remove records that
  stayed down long enough.
- **For `address: { from: nautobot }`** it allocates the pod's IPv4 address
  when gantry creates the pod. A recreated pod gets its old address back.

The host is the source of truth. The plugin never changes what gantry runs.

## Requirements

- gantry 0.2.0 or later.
- Nautobot 3.2 or later with invite-nautobot 0.4.0 or later.
- A Device or Virtual Machine in Nautobot for each host, named after the
  host (or set `node` in the config). When it is a member of a Container
  Cluster, pods are recorded in that cluster.
- A Prefix for every address the pods use, and a Container Network that
  carries it, so interfaces get linked to their network.

## Install

```sh
sudo apt install gantry-nautobot
sudo install -m 0600 /usr/share/doc/gantry-nautobot/nautobot.example.toml \
  /etc/gantry/plugins/nautobot.toml
sudoedit /etc/gantry/plugins/nautobot.toml   # url and token
gantry plugin ls                              # shows nautobot and its hooks
```

The token needs to read and write IPAM IP Addresses and the invite-nautobot
containers models, and read Devices, Virtual Machines, VLANs and Prefixes.

Every project on the host is recorded once the plugin is installed. A
project opts out with:

```yaml
plugins:
  nautobot:
    enabled: false
```

## Addresses

```yaml
pods:
  web:
    network:
      - parent: vlan4
        address: { from: nautobot }                       # pool from the parent
      - parent: vlan30
        address: { from: nautobot, prefix: 192.0.2.0/24 } # or a Prefix ID
```

The pool is the Prefix named by `prefix:` (add `namespace:` when the same
CIDR exists in several namespaces). Without one, it is the Prefix that
holds the parent interface's own address, else the Prefix of a Container
Network on the parent's VLAN, preferring the host's cluster. The VLAN ID
comes from the kernel, or from a name such as `vlan4` or `eth0.4`.

The plugin reserves the first free address that is not the gateway by
creating it in Nautobot (status `Active`, or `ip_status` in the config),
with a description naming the project, pod and interface. If the pod's
record already has an address in the pool, that address is returned
instead. gantry's default gateway is the first host of the subnet unless
the compose file sets `gateway:`.

If anything fails, gantry does not create the pod and shows the reason.
Recording on `up` and `down` fails open: problems are warnings, and the
log is `/var/log/gantry/plugins/nautobot.log`.

## Configuration

`/etc/gantry/plugins/nautobot.toml`, root owned, mode 0600:

| Key | Default | Meaning |
|---|---|---|
| `url` | required | Nautobot base URL. |
| `token` | required | API token. |
| `ca_file` | none | Extra PEM CA for a Nautobot behind a private CA. The system trust store is always used. |
| `timeout` | `10` | Seconds per API call. |
| `ip_status` | `Active` | Status of addresses the plugin creates. |
| `node` | host name | The Device or Virtual Machine name of this host. |

## How it maps

| gantry | Nautobot |
|---|---|
| Pod | Pod, found by name, `compose_project` and cluster; `node` and `cluster` from this host. |
| Pod interface | Container Interface on the Pod with its IP Addresses; the Container Network whose Prefix holds the address. |
| Container | Container in the Pod, `type` docker or kiri, `compose_project` and `compose_service` set. |
| Mount | Container Volume, matched by destination. |
| Image | Container Image by repository, tag and digest. |

Environment variables and credentials are never sent to Nautobot.

## Releases

Push a tag `vX.Y.Z` matching the version in `Cargo.toml` and `plugin.toml`.
The release workflow builds the .deb on Ubuntu 22.04, publishes a GitHub
Release with build provenance, and adds the package to the INVITE Networks
apt repository (`https://invite-networks.github.io/apt`, suite `stable`),
the same one gantry is installed from.

## Development

```sh
cargo test
cargo build --release
cargo deb            # needs cargo-deb; Linux
```

The plugin speaks gantry's plugin protocol (api 1) on stdin and stdout, so
it can be driven by hand: send a `hello` line, then `event` or `allocate`
lines, and read one answer per request. See `docs/spec/plugins.md` in the
gantry repository.

## License

MIT. See [LICENSE](LICENSE).
