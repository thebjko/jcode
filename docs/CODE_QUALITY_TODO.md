# Code Quality Program Todo List

This file tracks the execution backlog for the code-quality uplift program described in `docs/CODE_QUALITY_10_10_PLAN.md`.

Status values:

- `pending`
- `in_progress`
- `blocked`
- `done`

## Phase 0: Prevent Further Decay

- [x] Add CI job for `cargo check --all-targets --all-features`
- [x] Add CI job for `cargo clippy --all-targets --all-features -- -D warnings`
- [x] Keep warning policy on a downward ratchet
- [x] Add documented file-size and function-size targets to contributor guidance

## Phase 1: Warning and Dead-Code Burn-Down

- [ ] Inventory all `#![allow(dead_code)]` locations and justify or remove them
- [ ] Reduce baseline warning count significantly from the current level
- [ ] Remove stale unused functions in `setup_hints.rs`
- [ ] Remove stale unused code in TUI support modules
- [ ] Audit broad suppressions and replace with narrow local allowances

## Phase 2: Decompose the Biggest Files

### Highest priority
- [ ] Split `tests/e2e/main.rs` by feature area
  - Started 2026-03-24: extracted feature modules `session_flow`, `transport`, `provider_behavior`, `binary_integration`, `safety`, and `ambient`
  - Completed 2026-03-24: extracted shared helpers into `tests/e2e/test_support/mod.rs`
- [ ] Continue splitting `src/server.rs` into focused submodules ([#53](https://github.com/1jehuang/jcode/issues/53))
  - Progress 2026-03-24: extracted shared server/swarm state into `src/server/state.rs`
  - Progress 2026-03-24: extracted socket/bootstrap helpers into `src/server/socket.rs`
  - Progress 2026-03-24: extracted reload marker/signal state into `src/server/reload_state.rs`
  - Progress 2026-03-24: extracted path/update/swarm identity utilities into `src/server/util.rs`
- [ ] Split `src/agent.rs` into orchestration, stream, interrupt, and tool-exec modules

### Next wave
- [ ] Split `src/provider/mod.rs` into traits, pricing, routes, and shared HTTP helpers ([#52](https://github.com/1jehuang/jcode/issues/52))
- [ ] Split `src/provider/openai.rs` into request, stream, tool, and response modules ([#52](https://github.com/1jehuang/jcode/issues/52))
- [ ] Split `src/tui/ui.rs` by render responsibility ([#51](https://github.com/1jehuang/jcode/issues/51))
- [ ] Split `src/tui/info_widget.rs` by widget/domain sections ([#51](https://github.com/1jehuang/jcode/issues/51))

## Phase 3: Error Handling Hardening

- [ ] Count production `unwrap` / `expect` separately from test-only usages
- [ ] Replace easy production `unwrap` / `expect` hotspots with explicit errors
- [ ] Add better error context for provider stream parsing failures
- [ ] Add better error context for reload and socket lifecycle failures ([#53](https://github.com/1jehuang/jcode/issues/53))

## Phase 4: Test Strategy Improvements

- [ ] Extract shared e2e test support helpers
- [ ] Add focused tests for reload state transitions
- [ ] Add focused tests for malformed provider stream chunks
- [ ] Add snapshot or golden tests for stable TUI render outputs
- [ ] Add property tests for protocol serialization and tool parsing

## Phase 5: Reliability and Performance Guardrails

- [ ] Add repeated reload reliability test coverage
- [ ] Add repeated attach/detach and reconnect coverage
- [ ] Track memory regression expectations in a documented budget
- [ ] Improve observability around reload, swarm, and tool execution paths
- [ ] Execute the compile-performance roadmap in `docs/COMPILE_PERFORMANCE_PLAN.md`
- [ ] Add repeatable compile timing checkpoints for warm/cold self-dev loops

## Immediate Active Work

- [ ] Land the quality plan document
- [ ] Land this todo list
- [x] Tighten CI guardrails
- [ ] Begin the first high-ROI cleanup or split
  - Follow-up tracking issues: #51, #52, #53, #54
