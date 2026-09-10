# Verification — component-renderer-mvp

Date: 2026-09-09 · Decisions D1–D4 · Plan T1–T10 complete.

## Final suite status

| Layer | Result |
| --- | --- |
| `cargo build` | ✅ clean (no errors, no warnings) |
| `cargo test` (full suite) | ✅ **35 passed / 0 failed** |
| YATT E2E (4 tests, 24 steps) | ✅ **24/24 ok, 100%** |

## E2E YATT evidence (all green)

| Test | Result | Report | Closes |
| --- | --- | --- | --- |
| `crm-e2e-standalone-render` | 5/5 ok | `crm-e2e-standalone-render-YATT-20260909-120539.json` | C01, C05 (+C06 via DOM asserts) |
| `crm-e2e-hot-swap` | 8/8 ok | `crm-e2e-hot-swap-YATT-20260909-120552.json` | C07 (+C15 module mount via swap) |
| `crm-e2e-ngmodule-render` | 4/4 ok | `crm-e2e-ngmodule-render-YATT-20260909-120619.json` | C15 |
| `crm-e2e-error-state` | 7/7 ok | `crm-e2e-error-state-YATT-20260909-120648.json` | C10 (rejected + previous view intact) |

Reproduce: start the server in gallery state (command below), then `yatt_test_run <name>` for each test in order (standalone → hot-swap requires gallery start; ngmodule requires badge start; error-state requires gallery start).

```bash
target/debug/render-component serve \
  --component "$(pwd)/tests/fixtures/sample-angular/gallery/gallery.component.ts" \
  --root "$(pwd)/tests/fixtures/sample-angular" \
  --port 17600 --selection-file /tmp/opencode/rc-e2e/selection.json --no-open
```

## Case-by-case status (Estado)

| ID | Estado | Evidencia |
| --- | --- | --- |
| C01 happy path | ✅ | `crm-e2e-standalone-render` green; screenshot in report + evidence below |
| C02 explorer navigation | ✅ | `explorer::nav` unit tests (open/move/edges/up); TUI key wiring ⚠️ manual (see deferred) |
| C03 highlighting | ✅ | classifier unit tests + TestBackend style test (cyan vs plain, C03 assertion) |
| C04 free port | ✅ | `free_ports_are_distinct_while_bound`; E2E used fixed control port + runtime vite port |
| C05 only that component | ✅ | `assert_hidden .badge` in standalone test |
| C06 all imports (SCSS/children) | ✅ | child text asserted; computed styles asserted via browser eval: `.gallery` bg `rgb(240,244,248)`, `h1` `rgb(15,23,42)` (transcript in session) |
| C07 hot-swap | ✅ | `crm-e2e-hot-swap` 8/8; live screenshot: badge replaced gallery, same server |
| C08 non-component inert | ✅ | classifier unit tests (`Other` not renderable/highlighted) |
| C09 same reselect no-op | ✅ | `selecting_same_component_is_unchanged` + `entry_is_stable_for_same_input` |
| C10 broken component | ✅ | `broken_decorator_is_typed_parse_error` + `crm-e2e-error-state` (rejected, view intact) |
| C11 race last-wins | ✅ | `rapid_selection_race_last_wins` (sequential select; latest revision wins) |
| C12 missing source datum | ✅ | `missing_template_file_is_early_error`, `missing_style_file_is_early_error` |
| C13 clean shutdown | ✅ | `server_starts_reports_and_frees_port_on_shutdown` (graceful shutdown → port rebindable) |
| C14 invalid root | ✅ | args unit tests (missing root/file-as-root/missing component) |
| C15 NgModule scope (D3) | ✅ | `crm-e2e-ngmodule-render` 4/4 (initial mount through `BadgeModule`) + hot-swap into badge |
| C16 future frameworks (D4) | ✅ | unit: kinds highlighted, `unsupported_message` present, not renderable; TUI status display ⚠️ manual |
| C17 tree explorer (owner follow-up, 2026-09-09) | ✅ | 10 new unit tests on the tree model (`explorer::nav`): in-place expand/collapse, lazy children, DFS order (VS Code-style), depth rows, ← collapse-or-parent, → expand, file selection inert, cursor clamping; TUI renders indent + ▾/▸ indicator (TestBackend test) |

**No ❌ cases.**

## Follow-up verification (C17, tree explorer)

- Suite after the tree rewrite: `cargo test` → **41 passed / 0 failed**.
- Key bindings updated: `Enter`/`→` expand-or-collapse a directory in place (files select as before), `←` collapses or jumps to the parent row, `q` quits.
- The headless `serve` path is untouched by this change; the YATT E2E results above remain valid.

## Follow-up verification (C18 logs panel + C19 material icons)

- Suite: `cargo test` → **47 passed / 0 failed**, zero warnings.
- C18: `LogSink` unit tests (order, 1000-line cap, timestamps); TestBackend test asserts the sink tail renders in the panel; error lines render red. `npm install` runs with `--no-progress --no-audit --no-fund` and BOTH streams are piped into the sink live (`[npm]`/`[npm!]`); `ng serve` stdout+stderr likewise (`[ng]`/`[ng!]`). Headless mode mirrors every line to stderr — verified in smoke run (log sample shows build output per line). The silent `ng-serve.log` file was removed in favor of live streaming.
- C19 (rev 2): crate swap `material-icons` → `devicons` (rust-devicons) per owner. `icon_for_file(path, &Some(Theme::Dark))` returns glyph + brand color per file type; the icon span is colored with `parse_hex(color)`, directories use nf-fa-folder(-open) glyphs. Tests: `devicons_render_next_to_names` (folder-open + TS glyph assertions), `hex_parsing_handles_material_colors`. Suite after swap: **48 passed / 0 failed**, zero warnings. NOTE: devicons glyphs are Nerd Font codepoints — the terminal must use a Nerd Font (GitHub README states the same requirement).
- E2E re-run after both changes: `crm-e2e-standalone-render` → 5/5 ok.
- Notes: TUI redraws every 500ms so logs stream without user input; `→` now expands directories (no longer duplicates Enter).

## C21: scrollable logs panel + log dump (owner request, 2026-09-09)

- Logs panel grew to 30% of the terminal height and is now **scrollable**: `PgUp`/`PgDn` page through the full history, `Home` jumps to the top, `End` (or scrolling back down) returns to live-follow mode — the block title shows the distance ("logs — N lines up"). Scroll offset counts lines up from the bottom, so new logs keep arriving while following.
- `l` **dumps the complete log** to `/tmp/render-component-logs.log` (status bar shows the path + line count) so the owner can share it verbatim.
- Sink buffer raised 1000 → 5000 lines; new accessors `len()` / `range()` (clamped) / `dump_to_file()`.
- Suite: **53 passed / 0 failed**, zero warnings. New tests: panel scroll assertions (bottom=latest visible, top=oldest visible), `range` clamping, `dump_to_file` round-trip, buffer bound at 5000.

## Bugfix: "Unknown reference" on real Angular workspaces (owner report, 2026-09-09)

- **Symptom**: rendering a gym component failed at build: `CircleLoaderComponent ... Unknown reference` in `ui-area-search.component.ts:66`. Caught instantly by the logs panel (working as designed).
- **Root cause**: the workspace uses tsconfig `paths` aliases; the host workspace had no knowledge of them, so aliased imports inside the user's component subtree did not resolve.
- **Fix**: `bridge_tsconfig_paths` — nearest tsconfig.json walking up from the selected component (nested workspaces supported), follows `extends` chains (incl. package-style), parses JSONC (comments + trailing commas), rewrites path targets under `external/` (the root symlink) and filters framework keys (`@angular`, `rxjs`, `tslib`, `zone.js`). Written into the host tsconfig BEFORE the dev server spawns.
- **Verification**: suite **50 passed / 0 failed**, zero warnings. New tests: `strip_jsonc_...` (comments/trailing commas/URL preservation), `bridge_maps_user_paths_and_filters_framework_keys` (nested workspace, extends chain, filtering, `external/` prefixing).
- **Rev 2 (owner report: `TS2571 Object is of type 'unknown'`)**: paths bridging alone was not enough — the host's `strict: true` collided with the workspace's non-strict settings. The bridge now inherits the FULL `compilerOptions` per TS `extends` semantics (base fields win unless the child redefines; per-key merge for compilerOptions/angularCompilerOptions) and copies a curated set of type-checking flags (strict family, decorators, target, lib, skipLibCheck, etc.) into the host tsconfig. Build/module plumbing stays ours. Suite: **50/50**, zero warnings.
- **Rev 3 (owner dump `/tmp/render-component-logs.log`: "no tsconfig.json found near the component")**: the walk-up had an arbitrary 6-level cap while the real workspace tsconfig sits ~13 levels deep → the bridge never ran and every alias failed again. Now `find_workspace_config` walks UNCAPPED to the explored root and prefers the dir containing `angular.json` (canonical workspace marker), falling back to the nearest tsconfig. ANSI escape codes are stripped from streamed compiler output so dumps are readable. Suite: **56/56**, zero warnings.
- **Rev 4 (owner dump: `TS2339 Property 'zfill' does not exist on type 'string'`)**: two gaps. (1) The bridge early-returned when the workspace tsconfig declared no `paths`, skipping the later stages — now paths, compiler options and ambient declarations are independent stages. (2) `paths` targets resolve against baseUrl relative to the **config file's directory**, not the explored root — nested workspaces were re-anchored wrong (`external/projects/...` instead of `external/gym/projects/...`), so aliases still failed. Now re-anchored via `config_dir.strip_prefix(root)`. Plus: ambient declarations (`*.d.ts`) from the config chain's `files`/`include` are bridged into the host `files` (they are never imported, so types like monkey-patched `String.prototype.zfill` need explicit inclusion), with a bounded-glob fallback when the tsconfig declares none. ANSI-free logs.
- **Rev 5 (owner dump: vite `Failed to resolve import "@angular/forms"` / `ngx-sonner`)**: four fixes. (1) `detect_angular_range` used a JSON pointer containing the key `@angular/core` — pointers treat `/` as a separator, so it NEVER matched and always fell back to ^20; switched to direct `Value::get`. (2) Third-party deps: the project's runtime packages are symlinked from the workspace `node_modules` into the host's (host-installed packages are never clobbered) — Vite resolves `ngx-sonner` & friends at the project's own versions. (3) TS 6 deprecates `baseUrl` — host tsconfig no longer writes it; mapped targets are explicit `./external/...` relative paths. (4) ng22 removed `createNgModuleRef` → renamed to `createNgModule` in the host bootstrap; template drop of the removed `ComponentFactoryResolver` finalized.
- **End-to-end synthetic gym scenario** (nested workspace + `angular.json` + non-strict + aliases + `zfill` prototype patch declared in `typings.d.ts`): build goes from `TS2339/TS2307` failures to `Application bundle generation complete`. Suite: **58/58**, zero warnings.
- To pick the fixes up, rebuild (`cargo build --release`) and restart the tool; the bridged count appears in the logs (`[host] bridged N tsconfig path mapping(s) ...`, `[host] bridged compiler options: ...`, `[host] included N ambient declaration file(s)`).

## C24: Angular version matching + prototype-patch globals (owner dump, 2026-09-09)

- **Discovery**: gym workspace runs **Angular ^22.0.5** while the host pinned ^20 → `NG5002 Parser Error` on template arrow functions (v22 feature). Also: gym has ZERO `.d.ts` files — `zfill` is declared+implemented in `projects/modules-shared/prototypes.ts` (a plain side-effect .ts imported by their app bootstrap), so a `.d.ts`-only bridge can never see it.
- **Fix 1 — version matching**: `detect_angular_range` reads `@angular/core` from the nearest package.json (walking up from the component); the host cache dir is keyed per major (`host-ng22`, `host-ng20`) and the template's Angular packages are patched to the target's range before `npm install`.
- **Fix 2 — globals**: `prototypes.ts` files are (a) added to tsconfig `files` (type side) and (b) wired into a generated `src/selected/globals.ts` which `main.ts` imports statically (runtime side) — prototype patches land before the component mounts. Host template dropped the deprecated `ComponentFactoryResolver` usage (removed in newer Angular) in favor of `ViewContainerRef.createComponent({ ngModuleRef })`.
- **Verification**: suite **58/58**, zero warnings; synthetic gym scenario (nested workspace + angular.json + non-strict + `@shared` alias + `zfill` patch in prototypes.ts) now renders in-browser: `Calendar Zfill Component 05x` — the patch executes at runtime and the alias resolves.

## C26: project runtime deps installed into the host (owner console, 2026-09-09)

- **Symptom**: page broken with vite `[plugin:vite:import-analysis] Failed to resolve import "socket.io-parser" from .../vite/deps/socket__io-client.js` + NS_ERROR_CORRUPTED_CONTENT cascade for the prebundled deps (`@angular/forms`, `rxjs`, `pdfjs-dist`, `@ng-bootstrap`, `zod`).
- **Root cause**: symlink-based dependency linking breaks Vite's deps pre-bundling — `socket.io-parser` is a TRANSITIVE dep of `socket.io-client` (pnpm-nested in the user's workspace), unreachable from the host `node_modules` chain.
- **Fix**: symlink linking REMOVED. `install_project_dependencies` now runs `npm install <dep>@<range> ...` for every project `dependencies` entry (batch first; on failure, one-by-one so private/unavailable packages are skipped with a log line instead of sinking the set). npm resolves transitives properly into the host tree.
- **Verification**: suite **59/59**, zero warnings. Smoke with `socket.io-client@^4.8.1` in the project deps: install batch OK, `socket.io-parser` present in the host tree, bundle complete, page renders (`Calendar Zfill Component 05x`), no error overlay.
- **Rev 2 (owner dump: `ERESOLVE monaco-editor peer conflict` → then `Cannot find module '@angular/compiler-cli'`)**: real workspaces carry peer conflicts their own install tolerates (gym: monaco 0.55 + ngx-monaco-editor-v2 wanting 0.56) — all npm installs now run with `--legacy-peer-deps` mirroring the workspace's effective mode. Side-effect handled: legacy mode stops npm's auto-install of peer deps, so the host template now declares `@angular/compiler-cli` explicitly (version-patched to the target's range by `patch_angular_versions` like the rest of the Angular set).
- **Known limitation surfaced** (D3, accepted for v1): the component's runtime calls to the project backend fire for real and get `401 Unauthorized` (no auth/backend in the host). Visible in console; does not block rendering.

## C28: required signal inputs at mount (owner console: NG0950, 2026-09-09)

- **Symptom**: rendering `ui-document-management-dashboard-state` threw `NG0950: Input "state" is required but no value is available yet` — the component uses `input.required<T>()` and in the real app a parent always provides it; standalone mounting left it unset at the first CD pass.
- **Fix**: the pipeline extracts required signal inputs from the source (`name = input.required<...>()` scan), `/api/current` publishes them as `requiredInputs`, and the host page calls `componentRef.setInput(name, {})` for each right after creation (same tick, before CD) — the template renders with empty bindings instead of throwing. Legacy `@Input({required:true})` not covered (v22 codebases use signal inputs).
- **Verification**: suite **60/60**, zero warnings. New pipeline test (`required_signal_inputs_are_extracted`) + E2E with a new `dashboard-state` fixture (required `{id}` input): `Application bundle generation complete` → component renders its template with the input placeholder, no NG0950, no error overlay.

## C29: global workspace styles + Sass includePaths (owner report: "no carga todos los scss involucrados, ej. gym/assets/scss-v2/main.scss", 2026-09-09)

- **Root cause**: components depend on the design-system SCSS the app loads globally via `angular.json` (`styles` array + `stylePreprocessorOptions.includePaths` for `@use`/`@import` of shared partials). The host bridged none of that.
- **Fix**: `bridge_global_styles` — finds the nearest `angular.json` walking up from the component, selects the project whose root is the longest prefix of the component path, maps its `styles` (string + `{input}` entries, `inject:false` skipped) and `stylePreprocessorOptions.includePaths` to `./external/...` host paths, and appends them to the host `angular.json`.
- **Verification**: suite **61/61**, zero warnings. New unit test (`bridge_global_styles_maps_nearest_project`: longest-prefix selection, string + object entries, includePaths). E2E synthetic gym scenario with `angular.json` + `assets/scss-v2/main.scss` (`@use 'vars'` inside) + component SCSS using the same includePath: browser shows the global rule in `document.styleSheets` AND the component computed padding from the shared Sass variable (8px), build complete, no error overlay.

## C30: SCSS themes (owner: "_theme.scss / _theme_dark.scss", 2026-09-09)

- **Verified against the REAL workspace**: `main.scss` already `@use`s `themes/theme` + `themes/theme_dark`; through the bridged styles the host `styles.css` (415 KB) contains BOTH themes (`body { --bg... }` light + `body.dark` dark overrides). Light applies automatically (body-scoped).
- **Dark toggle added**: the design system scopes dark to `body.dark`/`body.fdark` (the app's ThemeService applies it) — the host page mirrors it: `d` keybind toggles the class, persisted in `localStorage` (`rc-theme-dark`) so it survives hot-swap reloads. Verified live on the real gym styles: light bg `rgb(244,244,249)` → dark bg `rgb(20,20,20)`.
- **Boundary observed** (next milestone, needs decisions): components whose DI graph needs the app's providers (HttpClient, AuthService, ...) fail to mount on the empty host injector (body renders empty, no crash overlay). That is the D3 provider-bridging scope, not a theme issue.

## C31: hot-swap reload coordination + dev-server death detection (owner: "cae el servidor o no carga en el navegador", 2026-09-09)

- **Diagnosis from the 575-line dump**: the server did NOT crash (9 hot-swaps, 8/8 builds complete, 0 failed in one session). The real flakiness: three competing reload sources per swap (vite re-optimization full-reload + dev-server "Page reload" + our SSE reload) racing a 0.3-2.2s rebuild — a reload landing mid-bundle-write breaks the page ("no carga"). Also observed (pre-existing in their workspace, warning only): `core-col.component.ts` styles with Sass interpolation compiled as CSS (`component:css` loader) — cosmetic, present in their own build too.
- **Fixes**: (1) `/api/ready` publishes `pending` (selection applied but build not settled) and `ngExited`; `build_pending` is set on selection and cleared by the ng stdout reader when a build settles; `ng_exited` flips when the dev-server pipes close. (2) Host page: on a revision bump it polls `/api/ready` (≤10s) and reloads only when settled; if the dev-server died it shows a clear error panel instead of hanging. (3) Watcher debounce (400ms): rapid successive selections collapse into ONE apply/rebuild.
- **Verification**: suite **61/61**, zero warnings. E2E: mount OK → selection-file hot-swap → single reload → new component renders. (TUI interactive flow remains under the ⚠️ manual item.)

## C32: fictional typed data for component inputs (owner: "auto-rellenalos con datos ficticios según su tipo", 2026-09-09)

- **What**: inputs declared in the component (`input.required<T>()`, `input<T>()`, `model<T>()`) are auto-filled with fictional data BY TYPE before mounting — required inputs stop throwing, optional inputs render too, `model()` two-way inputs included. Inputs with author defaults keep their defaults.
- **How**: `pipeline_types` parses the declarations (balanced generic scan), resolves referenced types against the project sources (nearest-first directory walk from the component: `interface`/`type`/`enum`/`class` bodies, JSONC-free bounded reads) and generates deterministic JSON: strings by field-name semantics (id/name/email/url/date/color...), numbers (1 / 10 for totals), booleans, arrays with one element, enums by first member, interfaces recursively (depth-capped, cycles safe). Values travel via `/api/current` (`inputValues`) and the host page applies them with `ComponentRef.setInput` in the creation tick (required-input `{}` fallback stays for anything unresolved).
- **Verification**: suite **62/62**, zero warnings. New tests: `input_values_generated_from_workspace_interfaces` (interface fields: id/name/email/active; model-with-default not overridden; unknown type → empty array) + parse test. E2E browser: gym-like component with `user = input.required<CalendarUser>()` renders `Calendar Sample name 05x <user@example.com>` + includePath SCSS padding — all fictional data live.

## Bugfix: infinite page-refresh loop (owner report, 2026-09-09)

- **Symptom**: the rendered page reloaded endlessly after opening; Firefox console showed `Loading failed for the module .../chunk-M56VZHOY.js` (the abort of an in-flight chunk fetch, a symptom — not the cause).
- **Root cause**: the tokio `watch`-backed SSE stream **echoes the current revision to every new subscriber**. The host page created the EventSource BEFORE mounting and compared with `!==` while `lastRevision` was still `-1` → the subscribe echo always triggered `location.reload()` → reconnect → echo again → infinite loop.
- **Fix (host `main.ts`)**: (1) pin `lastRevision` from `/api/current` BEFORE subscribing; (2) reload only when the received revision is STRICTLY GREATER (revisions are monotonic, so the subscribe echo is a guaranteed no-op); (3) on a transient mount failure (e.g. entry chunk fetched mid-rebuild), retry exactly once via a full reload guarded by a `sessionStorage` flag — never loops.
- **Verification**: E2E `crm-e2e-standalone-render` 5/5 and `crm-e2e-hot-swap` 8/8 after the fix; a marker-based stability probe (set `window` flag, wait 4s, assert it survives → no reload occurred) passed on a live page; the genuine revision bump still reloads exactly once and mounts the new component.

## Bugfix: TUI froze on "starting Angular host..." (owner report, 2026-09-09)

- **Root cause**: `activate()` awaited `serve::start()` inline, blocking the TUI event loop for the whole startup (npm install + ng serve ≈ 10s+). Consequence: no tick redraws (log panel frozen → looked hung), no key handling (q unresponsive).
- **Fix**: `serve::start` now runs in `tokio::spawn`; the result arrives via an unbounded mpsc channel handled as a `select!` branch in the event loop. While starting: logs stream live into the panel and keys keep working (re-enter shows "already starting"). On quit with a startup in flight, the spawned task is aborted (`kill_on_drop` stops the ng child).
- **Verification**: suite **48/48**, zero warnings; headless serve path re-verified. Interactive behavior of the TUI remains part of the ⚠️ manual-verification item (this harness cannot drive a TTY).

## ⚠️ Deferred (needs explicit user acceptance)

1. **TUI interactive flow (keyboard/click event loop, status-bar messages)** — YATT drives browsers, not terminal TTYs (pseudo-TTY smoke was polluted by the user's shell greeting and could not capture the alternate screen). Covered indirectly: nav model, styles and all serve logic are unit-tested; headless mode exercises the identical orchestration. Manual check: run `target/debug/render-component tests/fixtures/sample-angular`, navigate with ↑/↓/Enter/Esc, select `gallery.component.ts` (browser opens), re-select `badge.component.ts` (hot-swap), select `Button.tsx` (unsupported message), select `helper.ts` (inert), `q` quits and frees the port.

## Evidence files

- `features/component-renderer-mvp/evidence/` — screenshots captured in-session: standalone gallery render (SCSS visible), post-hot-swap badge render (module SCSS visible). YATT step screenshots also embedded in each report JSON/HTML above.

## Implementation notes discovered during verification (fixed)

1. `TcpListener::from_std` requires a non-blocking socket (caught by integration test).
2. `#[tokio::test]` default current-thread runtime deadlocks with blocking HTTP probes in tests (multi_thread flavor required).
3. Host entry must exist before the first `ng serve` build (template ships a placeholder; entry is written before spawn).
4. `createNgModuleRef` in Angular 20 takes the parent injector directly (no options object).
5. Manual `createApplication` + `createComponent` breaks the renderer chain (NG0407) — replaced by `bootstrapApplication(RootComponent)` + `ViewContainerRef.createComponent` (the standard dynamic-component path, with `ngModuleRef` support for D3).
6. A tsconfig `paths` mapping for `@angular/*` caused two runtime Angular copies (Vite deps chunk vs bundled app chunk) → NG0203 token identity failure. Removed; `preserveSymlinks: true` + the `external` symlink give correct resolution.
7. `ng serve` stderr is captured to `<host>/ng-serve.log` — compile failures are diagnosable (this surfaced fixes 4–6).
