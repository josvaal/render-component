# Brief — component-renderer-mvp

Date: 2026-09-09

## Original request (verbatim)

> Este será una herramienta CLI que sea capaz de renderizar componentes (React, Angular, Svelte, Vue, etc...) osea (.tsx, .module.ts, .svelte, .vue, etc....) Pero por el momento la base será angular, su flujo pienso que será el siguiente:
>
> Mostrar un file explorer (puede ser con alguna dependencia) y capacidad para navegar, abrir carpetas, mover la seleccion del archivo, carpeta. Y las extensiones de archivos previamente mencionados se resalten de alguna manera y que al dar click se levante un servidor en un puerto libre y renderice el componente (incluyendo todas sus importaciones scss, etc....) pero solamente ese componente. Y si el servidor ya está levantado, al seleccionar otro componente, la vista se reemplazará con este nuevo

## Explicit requirements extracted (no paraphrased additions)

- R1. CLI tool able to render UI components from their source files (`.tsx`, `.module.ts`, `.svelte`, `.vue`, etc.).
- R2. Angular is the base target for now; React/Svelte/Vue are future work.
- R3. File explorer (a dependency is allowed) with: navigation, open folders, move the selection across files and folders.
- R4. The mentioned component-file extensions are visually highlighted in the explorer.
- R5. On selection (click), start a server on a **free port** that renders ONLY that component.
- R6. The render includes ALL of the component's imports (SCSS, templates, etc.).
- R7. If the server is already up, selecting another component REPLACES the current view instead of restarting.

## Decisions

Registered at GATE 1 (user accepted all recommended options, 2026-09-09):

- D1 (Q1) **Explorer lives in the terminal**: TUI (ratatui + crossterm), keyboard + mouse navigation; on selection the tool opens the browser with the render.
- D2 (Q2) **Render pipeline**: pre-built Angular host app + dev-server (Angular builder/esbuild) served on the free port; the selected component is injected and recompiled on selection change. Requires Node on the machine. Real Angular render (standalone, SCSS, children).
- D3 (Q3) **NgModule scope v1**: standalone components mount directly; if the component has a sibling `.module.ts`, it mounts with its module. App-global providers are out of scope for v1.
- D4 (Q4) **Future frameworks in v1**: `.tsx` / `.svelte` / `.vue` files are listed and highlighted as components; selecting one shows a clear "not supported yet" message.
