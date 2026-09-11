# render-component

> **Executive summary**: a lightweight Rust CLI that renders a single UI component in the browser from a navigable file explorer — and lets AI agents discover, serve and hot-swap components over MCP.

## Problem

Developing UI components in isolation is slow and not solved simply:

- Booting the whole app (`ng serve`, `npm run dev`) to look at one component loads the entire project, takes time and drags in unrelated dependencies.
- Existing isolation tools (Storybook, playgrounds) require configuration, extra `.stories` files and project conventions that often don't exist in the repo.
- An AI agent has no natural way to see what a component looks like: it reads code, but cannot mount it and look at it.

The concrete pain: going from "I touched this file" to "I see it rendered alone, with its SCSS and imports" takes too many manual steps today.

## Solution

`render-component` is a single binary that:

1. Shows a file explorer in the terminal (ratatui TUI), navigable with keyboard and mouse.
2. Visually highlights component files (Angular today; React/Svelte/Vue classified for the future).
3. On selection, starts a dev server on a **free port** that renders **only that component**, including template, SCSS and imports.
4. If the server is already up, selecting another component **replaces the view** without restarting (hot-swap).
5. Exposes an **MCP server** so AI agents can discover, inspect, serve and replace components using the same pipeline as the TUI.

The value proposition: **zero configuration, zero extra files, one tool for humans (TUI) and agents (MCP)** sharing the same render pipeline.

## Target users

- **The author / personal tooling** *(confirmed decision)*: a personal tool for the daily component-development flow, not aimed at a market.
- Frontend devs with the same use case (isolate a component without booting the full app or configuring anything).
- AI agents that need visual verification of components: a model that can call `select_component` and get a real preview URL back.

## Platform

**CLI / TUI in the terminal + browser as the render surface.** The explorer lives in the terminal (ratatui + crossterm) because that is where the developer already is; the render lives in the browser because Angular components are web. A single Rust binary, with Node only as a requirement of the render pipeline (Angular host + builder).

## Differentiation

- **MCP / AI agents as a first-class citizen** *(confirmed decision)*: not an add-on — a first-class surface backed by the same pipeline as the TUI (detection, validation, dev-server, hot-swap).
- Zero-config: no `.stories` files, no Storybook config, no project conventions; point it at a directory and it works.
- Hot-swap without restarting: the server starts once and subsequent selections replace the view via selection IPC.
- Keyboard-first TUI with an integrated fuzzy finder (`/`), Telescope-style.

## Scope

- **MVP (v1 stable)**
  - TUI: tree explorer with keyboard/mouse navigation, in-place expand/collapse, fuzzy finder, log viewer.
  - Component-file highlighting per framework (Angular, React, Svelte, Vue) with a clear message for the ones not supported yet.
  - Angular-only rendering: standalone and sibling-NgModule (`.module.ts`), including template, SCSS and imports.
  - Dev server on a free port; hot-swap when switching components without restarting.
  - Headless `serve` (scriptable / E2E) and an MCP server with 9 tools.
  - Early detection and validation with clear feedback (broken/missing component).

- **Out of scope (for now)**
  - Rendering React/Svelte/Vue (they are listed and highlighted; selecting them says "not supported yet").
  - App-global providers in NgModule mounts (out of scope in v1).
  - Multi-user, auth, distribution/installable packaging.
  - Preview styling/theme configuration.

## Success metrics

*(Early stage; only where reasonable)*

- Time from "selection → rendered preview" (target: seconds, not a full-project build).
- Hot-swap success rate (replacement without restarting).
- Usage frequency of the MCP surface vs the TUI (signal of where the tool pays off most).

## Risks and assumptions

- **Node.js required at runtime**: the binary is Rust, but rendering needs Node + the Angular host toolchain. Unacceptable on machines without Node.
- **Angular version drift**: the pre-built host is pinned to a builder/esbuild; a moving Angular may break the pipeline. The host needs maintenance.
- **Recompilation speed**: hot-swap depends on rebuild; very large projects may degrade it.
- *[assumption]* It is personal/local tooling: no multi-user security or distribution concerns.
- *[assumption]* Future frameworks will reuse the detection + serve architecture (classification already exists).
- *[assumption]* The owner keeps the repo on `master` with no remote; this README targets the current local-only flow.