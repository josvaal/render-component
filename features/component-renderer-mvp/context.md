# Context — component-renderer-mvp

Date: 2026-09-09

## Repository inventory (verified)

| Path | State |
| --- | --- |
| `Cargo.toml` | Single binary crate `render-component` 0.1.0, `edition = "2024"`, **zero dependencies** |
| `src/main.rs` | Untouched `cargo new` scaffold (`Hello, world!`) |
| `AGENTS.md` | Project conventions (see below) |
| `.gitignore` | `/target` |
| Git | `master`, 0 commits, no remote, no CI |
| Toolchain | rustc/cargo 1.97.1 stable via rustup; no `rust-toolchain.toml` |

## Conventions that apply (from AGENTS.md)

- Base target is **Angular** (`*.component.ts`, `*.module.ts` + templates/SCSS); React/Svelte/Vue planned later — **keep file parsing and serving extensible per framework** (AGENTS.md → "What this is").
- Rendering flow is owner's spec: explorer → free-port server → only that component, hot-swap on reselect (AGENTS.md → intended flow, matches R3–R7).
- Verification is plain `cargo build` / `cargo test` / `cargo run` — no custom harnesses (AGENTS.md → "Verification").
- `edition = "2024"` → rustc/cargo ≥ 1.85 (AGENTS.md → "Toolchain").

## What exists vs. what is missing

Exists: nothing of the feature. The entire flow (R1–R7) is to be built.

Missing (all new):
1. File explorer UI (TUI or web — undecided, GATE 1).
2. Component-file detection per framework extension + highlighting.
3. Angular render pipeline: an Angular component cannot be "served" raw — it needs a compile step (template → render fn, SCSS → CSS). Realistic options: pre-built host Angular app + dev-server, or static CDN page (limited).
4. Free-port server with hot-swap semantics (R7).
5. CLI entrypoint, path handling, keyboard navigation.

## Prior-work search (activo, per workflow)

No existing route, component, endpoint, or equivalent flow in the repo — greenfield confirmed by full tree listing (5 entries + `AGENTS.md` + `features/`).

## Visual exploration (E2E screenshots)

**N/A at this phase**: the repo has no runnable UI yet — there is no screen to screenshot and no frontend URL to open. The E2E baseline (`antes-*.png` equivalents) will be captured against the freshly built tool in FASE 4 / GATE 2. YATT is alive (ping ok, pid 457729); no app login exists, so no credential mini-gate applies.

## Domain notes that change the design

- Angular has two component styles: **standalone** (self-contained, trivial to mount) and **NgModule-based** (needs its module context for providers/declarations). The owner's real Angular workspace (gym) uses NgModule style (`*.module.ts` is explicitly listed in the pedido). Scope decision required (GATE 1).
- "All its imports" can mean: styles (SCSS), templateUrl/styleUrls, child components, services/DI providers, pipes, directives. Depth must be bounded for v1 (GATE 1).
- Hot-swap (R7) implies a persistent host that re-renders on selection change — the dev-server stays up, only the rendered component changes.
