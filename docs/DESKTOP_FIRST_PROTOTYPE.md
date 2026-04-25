# Desktop First Prototype Target

Status: Proposed
Updated: 2026-04-25

The first implementation step for Jcode Desktop should be a **fake-data spatial workspace prototype**.

Do not start with:

- real server integration
- a full editor
- any browser work
- settings/auth flows
- packaging
- perfect text rendering

Start by proving the thing that makes the desktop app different:

> multiple agent sessions as navigable surfaces in a fast, custom, Niri-like workspace.

## First visual target

```text
┌────────────────────────────────────────────────────────────────────────────────────┐
│ Jcode Workspace: jcode repo      leader: Space     mode: NAV     4 sessions   2 run │
├──────────────┬────────────────────────────┬────────────────────────────┬────────────┤
│ sessions     │ ● fox / coordinator        │   wolf / impl              │ activity   │
│              │                            │                            │            │
│ ● fox        │ user: make desktop app     │ user: inspect tui arch     │ build      │
│ ○ wolf       │                            │                            │ cargo test │
│ ○ owl        │ assistant: plan surfaces   │ assistant: found protocol  │ 42%        │
│ ○ bear       │                            │                            │            │
│              │ tool: read docs            │ tool: grep ServerEvent     │ pending    │
│ files        │ tool: edit architecture    │ tool: summarize tui        │ approval   │
│ diffs        │                            │                            │            │
│ debug        │ composer inactive          │ composer inactive          │            │
├──────────────┴────────────────────────────┴────────────────────────────┴────────────┤
│ Space h/j/k/l focus  ·  Space H/J/K/L move  ·  Space n new session  ·  Space / cmd │
└────────────────────────────────────────────────────────────────────────────────────┘
```

## What this prototype must prove

1. A native window opens on Linux.
2. The app renders custom surfaces, ideally through `wgpu`.
3. There are multiple fake agent session surfaces.
4. One surface is focused.
5. `leader + h/j/k/l` moves focus.
6. `leader + H/J/K/L` moves surfaces.
7. `leader + n` creates a fake session surface.
8. `leader + z` zooms the focused surface.
9. `leader + x` closes the focused surface.
10. Each fake transcript scrolls independently.
11. The app idles at near-zero CPU.
12. Debug HUD shows frame/layout/render stats.

## Why this comes first

This validates the core product bet before heavier features:

- Is the spatial model good?
- Does keyboard navigation feel right?
- Can multiple sessions be visible without chaos?
- Can the custom UI stay fast?
- Does the app feel like Jcode mission control rather than another chat window?

## Initial surface kinds

```rust
enum SurfaceKind {
    AgentSession,
    Activity,
    WorkspaceFiles,
    Diff,
    Debug,
}
```

No browser preplanning. No full editor yet.

## Minimal fake state

```rust
struct WorkspaceLayoutState {
    lanes: Vec<LaneNode>,
    active_surface: SurfaceId,
}

struct LaneNode {
    columns: Vec<ColumnNode>,
}

struct ColumnNode {
    surfaces: Vec<SurfaceId>,
    active_surface_index: usize,
}

struct SurfaceState {
    id: SurfaceId,
    kind: SurfaceKind,
    title: String,
    focused: bool,
}
```

## Suggested first module layout

Build this as a standalone fake-data binary before connecting to the server:

```text
crates/jcode-desktop/
  src/main.rs
  src/app.rs
  src/workspace.rs
  src/input.rs
  src/views/root.rs
  src/views/surface.rs
  src/views/debug_hud.rs
```

## Success bar

The prototype is successful when a user can launch it, see multiple fake sessions, move between them with leader+`h/j/k/l`, create/move/close/zoom surfaces, and confirm the app remains smooth and idle-efficient.
