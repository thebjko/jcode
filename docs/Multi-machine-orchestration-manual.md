# Manual: Ubuntu server + macOS client

## Build `jcode` from this checkout

This repository is the `jcode` checkout.
Run the build commands from the repo root.

Use the same `jcode` build on both Ubuntu and macOS.
If possible, check out the same branch or commit on both machines.

### Ubuntu build

For Ubuntu or Linux self-dev work, use the repo's preferred build path:

```bash
cd /path/to/jcode
scripts/dev_cargo.sh build --release -p jcode --bin jcode
scripts/dev_cargo.sh --print-setup
scripts/install_release.sh
```

This builds the binary, installs it into jcode's versioned local store, and updates the `jcode` launcher symlink.

### macOS build

On macOS, install from this checkout with the release installer script:

```bash
cd /path/to/jcode
scripts/install_release.sh --fast
```

If you prefer the default optimized install profile instead of the faster one:

```bash
cd /path/to/jcode
scripts/install_release.sh
```

### Verify the install on both machines

```bash
command -v jcode
jcode version
```

If `command -v jcode` prints nothing, add `~/.local/bin` to your `PATH` and open a new shell.

This setup keeps the real `jcode` server on Ubuntu and uses the Mac only as the client UI.

Everything important runs on Ubuntu:
- session state
- model/provider auth
- tool execution
- file reads and writes

The Mac is just the terminal that attaches through the bridge.

## Private-network only

Use this only on a trusted private network such as Tailscale or a LAN you control.
Do not expose `jcode bridge serve` directly to the public Internet.

## What runs where

### Ubuntu
- `jcode serve`
- `jcode bridge serve`

### macOS
- `jcode bridge dial`
- `jcode connect`

## Prerequisites

- Install the same `jcode` build on both Ubuntu and macOS.
- Make sure `jcode` is on `PATH` on both machines.
- Ubuntu must already be able to run `jcode serve` successfully.
- macOS must be able to reach Ubuntu over a private IP.
- Both machines must use the same bridge token.

## 1. Ubuntu: create the bridge token

```bash
mkdir -p "$HOME/.jcode"
openssl rand -hex 32 > "$HOME/.jcode/bridge-token"
chmod 600 "$HOME/.jcode/bridge-token"
```

Copy the exact same token contents to macOS at the same path:

```bash
mkdir -p "$HOME/.jcode"
chmod 600 "$HOME/.jcode/bridge-token"
```

## 2. Ubuntu: start the real jcode server

Use an explicit socket path so the bridge command can target it reliably.

```bash
jcode --socket "/run/user/$UID/jcode.sock" serve
```

If Ubuntu is not authenticated yet, do that there first:

```bash
jcode login
```

You do **not** need to log in on the Mac for the bridge flow.

## 3. Ubuntu: expose that socket over the private network

If you are using Tailscale, get the Ubuntu private IP with:

```bash
tailscale ip -4
```

Then start the bridge:

```bash
jcode bridge serve \
  --listen "100.x.y.z:4242" \
  --local-socket "/run/user/$UID/jcode.sock" \
  --token-file "$HOME/.jcode/bridge-token"
```

Replace `100.x.y.z` with the real Ubuntu private IP.

Keep this process running.

## 4. macOS: create the local dial socket

On the Mac, start the local socket that forwards to Ubuntu:

```bash
mkdir -p "$HOME/.jcode"

jcode bridge dial \
  --remote "100.x.y.z:4242" \
  --bind "$HOME/.jcode/remote-jcode.sock" \
  --token-file "$HOME/.jcode/bridge-token"
```

Keep this process running.

## 5. macOS: attach the TUI to the remote Ubuntu server

Open a second terminal on the Mac and run:

```bash
jcode --socket "$HOME/.jcode/remote-jcode.sock" connect
```

This is the important part:
- use `connect`
- use `--socket "$HOME/.jcode/remote-jcode.sock"`
- do **not** run plain `jcode`, because that may start a local Mac server instead of attaching to Ubuntu

## 6. What the Mac is actually doing

After `connect`, the Mac is only acting as the client.

That means:
- prompts go from the Mac to the Ubuntu server
- model calls happen from Ubuntu
- tools run on Ubuntu
- file edits happen on Ubuntu filesystems
- sessions live on Ubuntu

If you want the agent to work on Ubuntu files, this is exactly what you want.

## Minimal operator flow

### On Ubuntu
Terminal 1:

```bash
jcode --socket "/run/user/$UID/jcode.sock" serve
```

Terminal 2:

```bash
jcode bridge serve \
  --listen "100.x.y.z:4242" \
  --local-socket "/run/user/$UID/jcode.sock" \
  --token-file "$HOME/.jcode/bridge-token"
```

### On macOS
Terminal 1:

```bash
jcode bridge dial \
  --remote "100.x.y.z:4242" \
  --bind "$HOME/.jcode/remote-jcode.sock" \
  --token-file "$HOME/.jcode/bridge-token"
```

Terminal 2:

```bash
jcode --socket "$HOME/.jcode/remote-jcode.sock" connect
```

## How this differs from the Docker smoke test

The Docker test used the same bridge topology:

- server side: `jcode serve`
- server side: `jcode bridge serve`
- client side: `jcode bridge dial`
- client side: attach through the dial socket

So the transport architecture was the same.

What differed was the verification method and the server bootstrap:

- In Docker, I verified the path by sending a raw `ping` JSON message through the bridged Unix socket and checking that it returned `pong`.
- In normal Ubuntu/macOS use, you should attach with `jcode --socket "$HOME/.jcode/remote-jcode.sock" connect` and use the real TUI.
- In Docker, `jcode serve` was started with an offline test-only provider bootstrap so it could run without Internet access.
- In normal Ubuntu/macOS use, Ubuntu should use its real provider/auth setup, usually via `jcode login` or your normal provider configuration.

The important takeaway is that Docker proved the bridge path itself works end to end, while this manual shows the normal operator workflow for using it interactively.

## Troubleshooting

### `bridge dial` says the socket path already exists
A stale local socket is still there. Remove it only after confirming no bridge process is using it.

### Authentication fails
The token file contents must match exactly on Ubuntu and macOS.

### The Mac opens a local session instead of the Ubuntu one
You probably ran plain `jcode` instead of:

```bash
jcode --socket "$HOME/.jcode/remote-jcode.sock" connect
```

### `jcode serve` works on Ubuntu but model calls fail later
That is an Ubuntu-side provider/auth problem. Fix `jcode login` or provider configuration on Ubuntu.

### The bridge starts but the Mac cannot reach Ubuntu
Check the private IP, firewall rules, and that the two machines can reach each other over Tailscale or LAN.

## Recommended practice

For long-running use, keep these three long-lived processes inside `tmux`:
- Ubuntu `jcode serve`
- Ubuntu `jcode bridge serve`
- macOS `jcode bridge dial`
