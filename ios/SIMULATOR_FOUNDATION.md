# jcode Mobile App Simulator Foundation

This document describes the first simulation slice now checked into the repo.

For the full target architecture and milestone plan, see
[`docs/MOBILE_AGENT_SIMULATOR.md`](../docs/MOBILE_AGENT_SIMULATOR.md).

## Product direction

The simulator is intended to be a **Linux-native simulator for the jcode mobile
application itself**. It is not Apple iOS Simulator, not an iPhone mirror, and
not a substitute for final on-device validation. Its purpose is to let humans
and AI agents build, run, inspect, test, and iterate on the mobile app without a
MacBook, Xcode, or a live iPhone.

The mobile app should be **Rust-first**. Shared behavior should live in Rust and
be exercised by both the Linux simulator and the eventual iOS host. The iOS app
should become a thin platform shell for OS-specific capabilities such as
window/view hosting, secure storage, push notifications, camera/photo picker,
microphone, and haptics.

## What exists now

The simulator foundation is currently **headless-first** and focused on
automation, logging, and deterministic state transitions. This is the seed of
the larger app simulator. It should evolve from a mocked flow into the real
shared mobile application core plus an agent-native automation surface.

### Workspace crates

- `crates/jcode-mobile-core`
  - shared simulator state
  - typed actions
  - reducer/store
  - semantic UI tree generation
  - transition/effect logging
  - baseline scenarios
- `crates/jcode-mobile-sim`
  - headless simulator daemon
  - Unix socket automation protocol
  - CLI for starting, inspecting, and driving the simulator

## Current scope

This first slice intentionally does **not** include a GUI renderer yet.

Instead, it gives us a solid automation and state foundation so agents can:

- start the simulator
- query state snapshots
- query the semantic UI tree
- dispatch typed actions
- tap semantic node IDs
- load scenarios
- inspect transition/effect logs
- reset and shut down the simulator

The long-term simulator must also support human-like interaction and visual
inspection:

- deterministic layout export
- hit testing by coordinates
- screenshots
- image/layout diffs
- replay bundles
- high-level assertions
- fake backend scenarios
- integration with jcode debug/tester tooling

The goal is for an agent to test autonomously in every way a human would, while
also having richer semantic APIs than a human has.

## Default transport

The simulator listens on a **Unix socket** by default.

Default path:

- `$JCODE_RUNTIME_DIR/jcode-mobile-sim.sock` if `JCODE_RUNTIME_DIR` is set
- otherwise `$XDG_RUNTIME_DIR/jcode-mobile-sim.sock`
- otherwise a private temp dir fallback

You can always override the path with `--socket`.

## Scenarios

Supported baseline scenarios:

- `onboarding`
- `pairing_ready`
- `connected_chat`

## CLI usage

### Start a simulator in the background

```bash
cargo run -p jcode-mobile-sim -- start --scenario onboarding
```

This prints the socket path when the simulator is ready.

### Serve in the foreground

```bash
cargo run -p jcode-mobile-sim -- serve --scenario pairing_ready
```

### Query status

```bash
cargo run -p jcode-mobile-sim -- status
```

### Dump full state

```bash
cargo run -p jcode-mobile-sim -- state
```

### Dump semantic UI tree

```bash
cargo run -p jcode-mobile-sim -- tree
```

### Dump transition/effect logs

```bash
cargo run -p jcode-mobile-sim -- log
cargo run -p jcode-mobile-sim -- log --limit 10
```

### Set fields

```bash
cargo run -p jcode-mobile-sim -- set-field host devbox.tailnet.ts.net
cargo run -p jcode-mobile-sim -- set-field pair_code 123456
cargo run -p jcode-mobile-sim -- set-field draft "hello simulator"
```

Supported fields right now:

- `host`
- `port`
- `pair_code`
- `device_name`
- `draft`

### Tap semantic nodes

```bash
cargo run -p jcode-mobile-sim -- tap pair.submit
cargo run -p jcode-mobile-sim -- tap chat.send
cargo run -p jcode-mobile-sim -- tap chat.interrupt
```

### Load a scenario

```bash
cargo run -p jcode-mobile-sim -- load-scenario connected_chat
```

### Reset to default onboarding state

```bash
cargo run -p jcode-mobile-sim -- reset
```

### Dispatch an action directly as JSON

```bash
cargo run -p jcode-mobile-sim -- dispatch-json '{"type":"set_host","value":"devbox.tailnet.ts.net"}'
```

### Shut down the simulator

```bash
cargo run -p jcode-mobile-sim -- shutdown
```

## Semantic node IDs

Examples exposed by the current semantic tree:

### Pairing/onboarding

- `pair.host`
- `pair.port`
- `pair.code`
- `pair.device_name`
- `pair.submit`

### Chat

- `chat.messages`
- `chat.draft`
- `chat.send`
- `chat.interrupt`

## Logging model

Every dispatched action produces a transition record containing:

- sequence number
- timestamp
- action
- state before
- state after
- emitted effects

Effects are also recorded separately.

This is the foundation for future:

- replay bundles
- simulator-driven regression tests
- renderer debugging
- fidelity comparisons against the eventual iPhone app

## Current limitations

This is an initial foundation only.

Not included yet:

- visible desktop renderer
- layout geometry export
- screenshot export
- richer fixtures/replay DSL
- live render inspector
- iOS host integration
- shared custom renderer backend
- fake jcode backend that exercises real pairing/WebSocket/protocol flows
- complete semantic automation operations such as assert/wait/scroll/type/gesture
- Rust-owned mobile protocol adapters equivalent to the current Swift SDK

## Recommended first workflow

A good current loop is:

1. start the simulator
2. inspect `state`
3. inspect `tree`
4. drive it with `set-field` and `tap`
5. inspect `log`
6. iterate on the shared simulator core

Example:

```bash
cargo run -p jcode-mobile-sim -- start --scenario pairing_ready
cargo run -p jcode-mobile-sim -- state
cargo run -p jcode-mobile-sim -- tap pair.submit
cargo run -p jcode-mobile-sim -- tree
cargo run -p jcode-mobile-sim -- set-field draft "hello simulator"
cargo run -p jcode-mobile-sim -- tap chat.send
cargo run -p jcode-mobile-sim -- log --limit 10
cargo run -p jcode-mobile-sim -- shutdown
```
