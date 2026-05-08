# Cross-Machine Bridge Plan V1

Status: Draft implementation plan

Scope: Add a thin cross-machine bridge that preserves jcode's existing server-owned session model and newline-delimited request/event protocol, instead of redesigning the application protocol first.

See also:

- [`SERVER_ARCHITECTURE.md`](./SERVER_ARCHITECTURE.md)
- [`SWARM_ARCHITECTURE.md`](./SWARM_ARCHITECTURE.md)
- [`WRAPPERS.md`](./WRAPPERS.md)

## Overview

Today jcode is local-first: one server manages sessions and clients attach over a Unix socket (`docs/SERVER_ARCHITECTURE.md`). The protocol crate explicitly describes the wire format as newline-delimited JSON over a Unix socket (`crates/jcode-protocol/src/lib.rs`). V1 should keep that request/event stream intact and solve only the machine-boundary problem.

The recommended shape is a bridge:

- Machine B keeps running a normal local `jcode serve`
- A new `bridge serve` command listens on a private-network TCP socket
- The bridge authenticates the remote peer
- The bridge opens a local Unix connection to `jcode.sock`
- The bridge relays bytes both directions without translating protocol messages
- A matching `bridge dial` command on machine A exposes a local Unix socket that forwards to the remote bridge

This is intentionally not a full distributed swarm design. V1 should prove that a remote machine can host a normal jcode server while another machine attaches through a narrow, authenticated transport layer.

## Architecture Decisions

- Keep the existing application protocol unchanged. The current protocol is newline-delimited JSON request/event streaming in `crates/jcode-protocol/src/lib.rs`; V1 should relay it instead of redesigning it.
- Keep jcode's local server/session ownership unchanged. The bridge is transport glue around `src/server/socket.rs` and `src/server/client_api.rs`, not a new session runtime.
- Prefer a bridge command over immediate core transport abstraction. `bridge serve` and `bridge dial` are the fastest way to prove the model before changing all client/server connection code.
- Restrict V1 to private-network use. Support Tailscale or trusted LAN first; do not design for public unauthenticated exposure.
- Support one active remote controller path at a time. Avoid multi-writer session ownership and cross-machine swarm semantics in V1.

## Non-Goals

- Full distributed swarm membership
- Cross-machine plan synchronization
- Cross-machine file-touch conflict detection
- Public Internet exposure without a stronger security story
- Replacing Unix sockets for local operation
- Translating protocol messages into a separate bridge-specific schema

## Dependency Graph

```text
CLI command surface
    │
    ├── Bridge config / args
    │       │
    │       ├── TCP listener / dialer implementation
    │       │       │
    │       │       ├── Auth handshake
    │       │       ├── Bidirectional relay
    │       │       └── Local Unix socket exposure
    │       │
    │       └── Lifecycle / logging / errors
    │
    └── Integration tests
            │
            ├── local Unix↔TCP↔Unix relay
            ├── auth failure cases
            └── end-to-end attach to temporary server
```

## Task List

### Phase 1: Foundation

## Task 1: Define the bridge CLI surface

**Description:**
Add explicit CLI commands for the bridge so the transport shape is concrete before implementation. V1 should expose one server-side command that binds a TCP listener and forwards to a local Unix socket, and one client-side command that dials the remote bridge and exposes a local Unix socket for existing client flows.

**Acceptance criteria:**
- [ ] `jcode bridge serve` and `jcode bridge dial` are defined in the CLI argument model.
- [ ] Each command has only the minimum required flags for V1.
- [ ] Help text makes clear that the bridge is for private-network use and forwards to an existing local jcode socket.

**Verification:**
- [ ] Build succeeds: `cargo check`
- [ ] CLI help renders: `jcode bridge --help`
- [ ] CLI help renders: `jcode bridge serve --help`
- [ ] CLI help renders: `jcode bridge dial --help`

**Dependencies:** None

**Files likely touched:**
- `src/cli/args.rs`
- `src/cli/mod.rs`
- `src/cli/dispatch.rs`

**Estimated scope:** Small: 1-3 files

## Task 2: Introduce a bridge module with a minimal TCP↔Unix relay

**Description:**
Create a new bridge module that owns the relay runtime without changing the existing client/server request types. The server-side entrypoint should accept a TCP stream, connect to the configured local Unix socket, and relay bytes in both directions. The dial-side entrypoint should listen on a local Unix socket, accept a local client, connect to the remote TCP bridge, and relay bytes in both directions.

**Acceptance criteria:**
- [ ] A bridge module exists with separate serve and dial entrypoints.
- [ ] The relay is byte-stream based and does not parse or rewrite jcode protocol messages.
- [ ] The implementation supports clean shutdown on disconnect from either side.

**Verification:**
- [ ] Build succeeds: `cargo check`
- [ ] Unit or integration test proves a simple payload can cross Unix→TCP→Unix unchanged
- [ ] Manual smoke test can establish a bridge process without panicking

**Dependencies:** Task 1

**Files likely touched:**
- `src/bridge.rs` or `src/bridge/mod.rs`
- `src/cli/dispatch.rs`
- `src/transport/mod.rs`
- `src/transport/unix.rs`

**Estimated scope:** Medium: 3-5 files

### Checkpoint: Foundation

- [ ] `cargo check` passes
- [ ] CLI surface is stable enough to use in tests
- [ ] Relay exists without changing request/event schema

### Phase 2: Security and Local Lifecycle

## Task 3: Add a minimal authentication handshake

**Description:**
Before relaying arbitrary traffic, require a minimal authentication handshake on the TCP bridge. Keep this simple for V1: a pre-shared token passed by flag or environment variable is enough. The bridge should fail closed before exposing the local Unix socket to unauthenticated peers.

**Acceptance criteria:**
- [ ] `bridge serve` requires an auth token source.
- [ ] `bridge dial` presents the token before relay begins.
- [ ] Unauthenticated or incorrect tokens are rejected without connecting to the local jcode socket.

**Verification:**
- [ ] Build succeeds: `cargo check`
- [ ] Test: valid token establishes relay
- [ ] Test: invalid token is rejected
- [ ] Manual check: wrong token prints a clear error and exits non-zero

**Dependencies:** Task 2

**Files likely touched:**
- `src/bridge.rs` or `src/bridge/auth.rs`
- `src/cli/args.rs`
- `src/cli/dispatch.rs`

**Estimated scope:** Small: 2-4 files

## Task 4: Add local socket lifecycle and safety rules for dial mode

**Description:**
Dial mode should expose a local Unix socket on machine A so current local client flows can attach without large refactors. Reuse the existing socket hygiene patterns from `src/server/socket.rs`: avoid silent cleanup of unknown live sockets, clean up only bridge-owned sockets, and produce actionable errors on refused or stale paths.

**Acceptance criteria:**
- [ ] Dial mode can bind a configured local Unix socket path.
- [ ] The bridge only removes sockets it owns or created for the current run.
- [ ] Failure messages match existing socket error style where practical.

**Verification:**
- [ ] Build succeeds: `cargo check`
- [ ] Test: bridge-created local socket is cleaned up on normal shutdown
- [ ] Test: existing foreign socket path is not destructively removed
- [ ] Manual smoke test: local client can connect to the dial socket

**Dependencies:** Task 2

**Files likely touched:**
- `src/bridge.rs` or `src/bridge/dial.rs`
- `src/server/socket.rs`
- `src/server/socket_tests.rs`

**Estimated scope:** Medium: 3-5 files

### Checkpoint: Security and Lifecycle

- [ ] Auth failure cases are covered
- [ ] Local socket lifecycle is safe and predictable
- [ ] The bridge is still protocol-transparent

### Phase 3: End-to-End Integration

## Task 5: Wire bridge commands into existing client flows

**Description:**
Connect the CLI flow so `jcode bridge dial` can act as a local attachment target for existing clients. The plan is not to replace `Client::connect_with_path` in `src/server/client_api.rs`; instead, V1 should make the bridge look like a normal local socket endpoint so current client semantics continue to work.

**Acceptance criteria:**
- [ ] Existing client API can connect through the dial socket without code changes to request/event shapes.
- [ ] Ping and subscribe flows still work through the bridge.
- [ ] Error paths clearly distinguish local Unix failures from remote TCP/auth failures.

**Verification:**
- [ ] Build succeeds: `cargo check`
- [ ] Integration test: `Request::Ping` receives `ServerEvent::Pong` through the bridge
- [ ] Integration test: subscribe/ack path works through the bridge
- [ ] Manual smoke test: attach through the dial socket to a temporary server

**Dependencies:** Tasks 3-4

**Files likely touched:**
- `src/server/client_api.rs`
- `src/bridge.rs` or `src/bridge/tests.rs`
- `crates/jcode-protocol/src/lib.rs` (tests only if needed, no schema change intended)

**Estimated scope:** Medium: 3-5 files

## Task 6: Add end-to-end tests using a temporary local jcode server

**Description:**
Add a real integration-style test that starts a temporary jcode server on a Unix socket, runs the bridge in front of it, then verifies that a client can connect through the bridge and complete a minimal protocol exchange. Reuse the existing temporary server smoke-test patterns in `src/build.rs` and existing socket tests where possible.

**Acceptance criteria:**
- [ ] A test starts a temporary server bound to a non-default Unix socket.
- [ ] The test starts bridge serve and bridge dial around that socket.
- [ ] The test verifies ping and subscribe behavior over the bridged path.

**Verification:**
- [ ] Targeted tests pass: `cargo test bridge -- --nocapture`
- [ ] Relevant socket/server tests still pass
- [ ] No existing local socket tests regress

**Dependencies:** Task 5

**Files likely touched:**
- `src/bridge/tests.rs` or `src/cli/dispatch_tests.rs`
- `src/build.rs`
- `src/server/socket_tests.rs`
- `src/cli/tui_launch/tests.rs` or `src/cli/dispatch_tests.rs`

**Estimated scope:** Medium: 3-5 files

### Checkpoint: Core End-to-End Flow

- [ ] Bridged ping works end-to-end
- [ ] Bridged subscribe works end-to-end
- [ ] Existing local-only connection behavior is unchanged

### Phase 4: Documentation and V1 Constraints

## Task 7: Document bridge usage and V1 limits

**Description:**
Document how to run the bridge on two machines, what network assumptions V1 makes, and what is explicitly unsupported. Keep the scope narrow so users do not mistake this for a full distributed swarm feature.

**Acceptance criteria:**
- [ ] Documentation explains `bridge serve` and `bridge dial` with example commands.
- [ ] Documentation explicitly says V1 is for private-network use.
- [ ] Documentation lists unsupported cases: full distributed swarm, public Internet exposure, multi-controller session ownership.

**Verification:**
- [ ] Docs are readable from the existing docs index or README links if added
- [ ] Example commands match the implemented CLI flags
- [ ] Another engineer could follow the steps without guessing hidden setup

**Dependencies:** Tasks 1-6

**Files likely touched:**
- `README.md`
- `docs/SERVER_ARCHITECTURE.md`
- `docs/plan_v1.md`
- optionally a new `docs/BRIDGE_V1.md`

**Estimated scope:** Small: 1-3 files

### Checkpoint: Complete

- [ ] All bridge tests pass
- [ ] `cargo check` passes
- [ ] End-to-end private-network bridge flow works
- [ ] V1 limitations are explicit in documentation
- [ ] Ready for implementation review

## Parallelization Opportunities

Safe to parallelize after Task 1:

- Bridge relay implementation (Task 2)
- CLI help/docs drafting (Task 7 draft only)
- Test harness scaffolding for temporary sockets/server startup (part of Task 6)

Must remain sequential:

- Final CLI surface before docs/examples are finalized
- Auth handshake before end-to-end verification is considered complete
- End-to-end integration after relay behavior is stable

## Risks and Mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| Bridge scope expands into a full distributed protocol rewrite | High | Keep relay protocol-transparent; no schema redesign in V1 |
| Local socket cleanup becomes destructive | High | Reuse `src/server/socket.rs` safety patterns and add ownership-focused tests |
| Auth is too weak or too broad | High | Restrict V1 to private networks with explicit token auth and fail-closed handshake |
| Client/server code starts depending on TCP details | Medium | Keep bridge logic isolated from `Client` request/event semantics |
| V1 is mistaken for full swarm support | Medium | Document unsupported cases explicitly and keep command names narrow |
| Test setup becomes flaky due to multiple temporary sockets | Medium | Reuse temporary server and socket test patterns already present in `src/build.rs` and `src/server/socket_tests.rs` |

## Open Questions

- Should `bridge dial` create the local Unix socket automatically, or should the path be mandatory?
- Should the auth token come only from flags/env vars in V1, or also from config?
- Should dial mode serve exactly one client connection at a time in V1, or multiple local clients to one remote bridge?
- Should the bridge emit a small version preflight before authentication so mismatched protocol builds fail clearly?
- Should the bridge live in `src/bridge.rs` first, or in a new internal module tree such as `src/bridge/{mod.rs,auth.rs,tests.rs}`?

## Recommended Initial Command Shape

Server-side machine:

```bash
jcode bridge serve \
  --listen 100.64.0.10:4242 \
  --socket /run/user/$UID/jcode.sock \
  --token-file ~/.jcode/bridge-token
```

Coordinator-side machine:

```bash
jcode bridge dial \
  --remote 100.64.0.10:4242 \
  --bind /tmp/jcode-remote.sock \
  --token-file ~/.jcode/bridge-token
```

Then existing local flows can target the dial socket:

```bash
JCODE_SOCKET=/tmp/jcode-remote.sock jcode connect
```

This keeps the seam narrow: local jcode still speaks its normal protocol over a local Unix socket, and the bridge handles only cross-machine transport plus authentication.
