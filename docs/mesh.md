# PocketMesh

PocketMesh is an optional mesh networking substrate for PocketBoot. When
enabled, multiple PocketBoot devices can cooperate across weird local
links (USB gadget Ethernet first, Wi-Fi P2P later) so a host can reach a
device through cooperating intermediate devices.

The design prefers boring Linux primitives:

```
physical/weird link -> Linux netdev -> IPv6 addressing -> Babel routing
```

PocketBoot remains a normal Linux IPv6 node. Iroh or custom RPC can be
layered on top later once there is an actual routed substrate.

## Status

This is the v0 implementation. It is a trusted local lab mesh only.

- USB gadget Ethernet (NCM/ECM) link backend: implemented.
- Wi-Fi P2P link backend: stubbed, not working yet.
- Routing: shells out to a bundled `babeld` binary.
- Authentication: none. Babel does not authenticate peers.
- No inbound shell/network services are enabled as part of mesh v0.

## Build flags

`Cargo.toml` features:

| feature          | meaning                                                  |
|------------------|----------------------------------------------------------|
| `mesh`           | Enables the runtime mesh substrate (identity, ULA, links).|
| `mesh-usb-net`   | Enables the USB gadget Ethernet link backend. Implies `mesh`.|
| `mesh-wifi-p2p`  | Stubs only for now. Implies `mesh`.                      |

Default builds do not include mesh.

## Babeld packaging

For v0, `babeld` is not built from source. Provide a binary via the
`BABELD` environment variable:

```sh
BABELD=/path/to/babeld cargo xtask cpio --features mesh,mesh-usb-net
```

The binary is copied into the initrd as `/sbin/babeld` (mode 0755). If
`mesh` is enabled and `BABELD` is not set, the build fails with a clear
error.

A later `cargo xtask babeld` task can fetch/build a pinned static
`babeld`; that is separate from the runtime implementation.

## Kernel config

`configs/pocketboot.toml` has an opt-in mesh feature block:

```toml
[features]
mesh = false

[kconfig.mesh]
INET = true
IPV6 = true
NETDEVICES = true
PACKET = true
UNIX = true
USB_CONFIGFS_NCM = true
USB_CONFIGFS_ECM = true
TUN = true
DUMMY = true
```

Enable `mesh` per-device (e.g. in `configs/device/qemu/aarch64-virt.toml`)
once a smoke test topology exists. Adjust symbols if the target kernel
merge complains about indirect selects.

## Runtime behavior

When mesh is compiled in and not disabled on the kernel command line,
PocketBoot:

1. Derives a stable node identity from the detected serial, FDT
   model/compatible strings, and a domain separation string (SHA-256).
   The raw serial is never used as the network-visible identity.
2. Derives a stable IPv6 ULA `/128` and assigns it to `lo`.
3. (with `mesh-usb-net`) Composes an NCM (fallback ECM) USB Ethernet
   function into the existing Fastboot gadget and waits for the netdev.
4. Brings mesh links up and assigns a deterministic link-local IPv6.
5. Generates `/run/pocketboot-babeld.conf` and starts `/sbin/babeld`
   when present. If `babeld` is missing, the subsystem still configures
   the netdev and ULA and continues without dynamic routing.
6. Writes `/run/pocketboot/mesh/status.json`.

Mesh startup is non-fatal. Failures are logged and normal boot
discovery/kexec behavior continues.

### Kernel command line

| param                       | effect                                    |
|-----------------------------|-------------------------------------------|
| `pocketboot.mesh=0`         | Disable mesh even when compiled in.       |
| `pocketboot.mesh.usb=0`     | Disable the USB net backend (optional).   |
| `pocketboot.mesh.ifaces=...`| Extra manual mesh interfaces (optional).  |

### BusyBox

When `mesh` is in the feature set, the BusyBox build enables the `ip`
applet and related features (`FEATURE_IP_ADDRESS`, `FEATURE_IP_LINK`,
`FEATURE_IP_ROUTE`, ...) so the mesh runtime can configure addresses and
links via `/bin/busybox ip`.

## Expected logs

```
mesh config derived node_id=... ula=fdxx:...
mesh identity selected node_id=... ula=fdxx:...
mesh usb net backend enabled
mesh usb net function created function=ncm.pocketmesh ifname=pbmesh0 ...
mesh usb netdev discovered ifname=pbmesh0 kind=usb-gadget
mesh loopback ULA assigned addr=fdxx:.../128
mesh link up ifname=pbmesh0 kind=usb-gadget link_local=fe80::.../64
mesh babel config written path=/run/pocketboot-babeld.conf
mesh babel started pid=...
```

On failure:

```
mesh babel unavailable path=/sbin/babeld
mesh continuing without dynamic routing
```

## Status file

`/run/pocketboot/mesh/status.json`:

```json
{
  "enabled": true,
  "node_id": "hex",
  "ula": "fdxx:...",
  "links": [
    { "name": "pbmesh0", "kind": "usb-gadget", "up": true }
  ],
  "routing": {
    "backend": "babeld",
    "running": true,
    "available": true
  }
}
```

Readable over existing ADB sync/shell paths.

## Demo topology (first device acceptance)

```
host laptop
  USB gadget link to device A

device A
  PocketBoot with mesh enabled
  USB netdev up
  babeld running
```

Minimum acceptance:

1. `BABELD=/path/to/babeld cargo xtask cpio --features mesh,mesh-usb-net`
   succeeds.
2. Booted PocketBoot still exposes fastboot/ADB as before.
3. USB network interface appears on the device as `pbmesh0` (or detected
   by MAC).
4. Host sees a USB network interface.
5. Device logs include mesh identity and ULA.
6. `ip -6 addr show dev lo` shows the ULA `/128`.
7. `ip link show pbmesh0` shows the link up.
8. `babeld` starts if `/sbin/babeld` was included.
9. Boot discovery and kexec behavior are not regressed.

Future multi-hop acceptance:

```
laptop -- USB -- phone A -- (future link) -- phone B -- USB -- host C
ns-host can ping6 ns-b's ULA through ns-a
```

## Known limitations

- No peer authentication. Anyone on the link can inject Babel packets.
- No internet relay, NAT traversal, or global peer discovery.
- Wi-Fi P2P backend is stubbed.
- `babeld` config syntax should be verified against the exact binary
  supplied via `BABELD`. The generated config uses:
  ```
  interface <name> type <wired|wireless|tunnel>
  redistribute local ip <ula>/128 allow
  redistribute local deny
  ```
- The raw device serial is not logged by the mesh subsystem.

## Future work

- Wi-Fi P2P backend using an external `wpa_supplicant`/`wpa_cli`.
- Bluetooth PAN backend if useful.
- Iroh endpoint bound to the mesh ULA.
- Authenticated PocketBoot RPC (logs, boot status, kexec, file
  push/pull, artifact fetch).
- Trust/pairing model (TOFU, QR code on Slint UI, host-signed
  allowlist).
- UI panel showing node ID, ULA, links, routes, and a QR diagnostic
  payload.
- In-process Babel subset only if shelling out to `babeld` becomes
  annoying.
