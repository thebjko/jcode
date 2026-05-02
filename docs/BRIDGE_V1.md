# Bridge V1

Bridge V1 lets one machine expose a normal local `jcode` server to another machine without changing the existing client/server protocol.

## What it does

The bridge keeps jcode's current local socket protocol intact:

- machine B runs a normal local `jcode serve`
- machine B runs `jcode bridge serve` on a private-network TCP address
- machine A runs `jcode bridge dial` and exposes a local Unix socket
- existing local client flows connect to that dial socket as if it were a normal local server

The bridge is transport glue only. It relays bytes between TCP and Unix sockets and does not translate protocol messages.

## Commands

Server-side machine:

```bash
jcode bridge serve \
  --listen 100.64.0.10:4242 \
  --local-socket /run/user/$UID/jcode.sock \
  --token-file ~/.jcode/bridge-token
```

Coordinator-side machine:

```bash
jcode bridge dial \
  --remote 100.64.0.10:4242 \
  --bind /tmp/jcode-remote.sock \
  --token-file ~/.jcode/bridge-token
```

Then point normal local flows at the dial socket:

```bash
JCODE_SOCKET=/tmp/jcode-remote.sock jcode connect
```

## How it works

The data path is:

```text
local jcode client
  -> local Unix socket
  -> bridge dial
  -> TCP over private network
  -> bridge serve
  -> remote Unix socket
  -> remote jcode server
```

This keeps the seam narrow:

- local jcode still speaks newline-delimited JSON over a local Unix socket
- the remote machine still owns the real jcode server session state
- the bridge only handles transport and token authentication

## Network and security assumptions

Bridge V1 is for private-network use:

- Tailscale
- trusted LAN
- other operator-controlled private networks

Each bridge connection uses a pre-shared token read from `--token-file`. The server rejects incorrect tokens before relaying traffic to the local jcode socket.

Do not expose Bridge V1 directly to the public Internet.

## V1 limits

Bridge V1 is intentionally narrow. It is not:

- full distributed swarm membership
- cross-machine plan or memory synchronization
- multi-controller session ownership
- a public Internet remote-access feature
- a replacement for local Unix sockets during normal single-machine use

## Verification

This bridge path has end-to-end coverage in `tests/e2e/transport.rs` and was also verified in two offline Docker containers by running a real `ping` through the bridged Unix socket.
