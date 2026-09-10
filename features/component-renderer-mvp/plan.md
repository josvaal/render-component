# Plan — component-renderer-mvp

Date: 2026-09-09 · Decisions: D1–D4 (see `brief.md`). Verification layers: **Rust unit/integration tests** (backend logic) + **YATT E2E against the rendered page** (user-facing flow). E2E never replaces units: units isolate which layer broke.

Key design facts driving the tasks:

- **Selection is filesystem-IPC**: the binary watches a runtime `selection` file (TUI writes it; headless mode and tests can write it too). This is what makes hot-swap (C07) testable E2E without driving the TUI.
- **CLI surface**: `render-component [path]` opens the TUI over `path` (default cwd). `render-component serve --component <file> [--root <dir>]` runs headless with a preselected component (E2E entrypoint).
- **Host app**: an embedded Angular workspace template (materialized on first run under the OS temp/cache dir), served by the Angular dev-server (`@angular/build`) on the Rust-chosen free port. A Vite-free, standard `ng serve` pipeline keeps Angular semantics real (SCSS, children, modules).
- **Bootstrap contract**: host `main.ts` fetches `/api/current` from the Rust server (proxied) → dynamic-imports the component entry file → mounts it. `/api/events` (SSE) signals re-selects → re-import (hot-swap without restarting anything).
- Fixtures: `tests/fixtures/sample-angular/` — standalone component with SCSS + child component; NgModule-based component + sibling `.module.ts`; broken component; plain `.ts` decoys.
- YATT tests persisted: `crm-e2e-standalone-render`, `crm-e2e-hot-swap`, `crm-e2e-error-state`. Reports stored via `yatt_test_run`; screenshots in `evidence/`.

| # | Task | Cubre | Test |
| --- | --- | --- | --- |
| T1 | Cargo deps + module layout (`cli/args`, `detect`, `explorer`, `serve`, `pipeline`, `host`) + arg parsing/validation: invalid/empty root → early error, non-zero exit | C14 | `src/cli/args.rs` unit tests (`tests/args_test.rs`): empty path, nonexistent path, file-instead-of-dir |
| T2 | Component classifier: path → `ComponentKind {AngularStandalone, AngularModule, React, Svelte, Vue, Other}`; `@Component` detection; sibling `.module.ts` → AngularModule; `.tsx/.svelte/.vue` recognized | C03, C08, C16, C15 (detection) | `src/detect.rs` unit tests over fixture tree: each kind classified, plain `.ts`/`.html` → Other |
| T3 | Explorer TUI: ratatui browser, keyboard nav (up/down/enter/esc-left), component kinds visually highlighted (style map per kind), status bar; selection emits through the selection file | C02, C03 (visual) | `src/explorer/nav.rs` unit tests: open/close folder, move selection across files+folders, boundary conditions; style-mapping unit test per kind |
| T4 | Serve module: free-port acquisition (bind `:0`, report chosen port), axum API (`/api/current`, `/api/events` SSE), selection-file watcher, clean Ctrl-C shutdown releasing port and children | C04, C13, C11 (transport), C07 (mechanism) | Integration `tests/serve_test.rs`: two servers concurrently → distinct ports; spawn → write selection → SSE event observed; kill → port rebindable |
| T5 | Pipeline validation (pre-serve): component parseable (`@Component` present), referenced templateUrl/styleUrls exist; typed early errors surfaced to status | C10, C12 | `src/pipeline/validate.rs` unit tests: broken fixture → typed error; missing templateUrl → early error; valid fixture → ok |
| T6 | Host app template embedded (`include_dir`): Angular workspace + `main.ts` dynamic-import bootstrap; materialization on first run; `npm install` guided; spawn `ng serve --port <free>`; proxy `/api` → Rust | C05, C06 (mechanism), C01 (mechanism) | Unit: materialization writes expected tree; integration: spawn host on fixture (skipped if node absent) then E2E asserts |
| T7 | Hot-swap + race + idempotency in selection state: last-selection-wins token (drop stale), same-component no-op, SSE push on change | C07, C09, C11 | `src/serve/state.rs` unit tests: rapid A→B → B wins; A→A → no event; E2E `crm-e2e-hot-swap` |
| T8 | NgModule mount path: bootstrap component through its sibling module (`createNgModuleRef` + ComponentFactoryResolver in host `main.ts` variant) | C15 | E2E `crm-e2e-ngmodule-render` over NgModule fixture (unit: classifier already routes kind → host strategy) |
| T9 | Unsupported frameworks: selecting `.tsx`/`.svelte`/`.vue` → status message "not supported yet", no server change | C16 | Unit: `pipeline` returns `Unsupported(kind)`; TUI status shows message (style/message unit) |
| T10 | E2E YATT suite + evidence + full verification: fixtures project; `crm-e2e-standalone-render` (component + SCSS computed style + child rendered, ONLY that component); `crm-e2e-hot-swap`; error-state check; screenshots `antes/despues` | C01, C05, C06, C10 (e2e), C04/C07 (e2e confirmation) | YATT tests above + `evidence/*.png` + `verification.md` |

## Coverage closure

Every case C01–C16 has ≥ 1 task and ≥ 1 test:

C01→T6,T10 · C02→T3 · C03→T2,T3,T10 · C04→T4,T10 · C05→T6,T10 · C06→T6,T10 · C07→T4,T7,T10 · C08→T2 · C09→T7 · C10→T5,T10 · C11→T4,T7 · C12→T5 · C13→T4 · C14→T1 · C15→T8(+T2) · C16→T9(+T2)

No case without a task; no task without its named test path.
