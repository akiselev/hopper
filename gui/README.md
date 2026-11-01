# hopper-gui

hopper's desktop viewer (milestone M2): a skelter window with a berth dock of source, stack,
locals, calls, query and timeline panes over an `Arc<dyn Debugger>`.

```sh
cargo run --release -- --fake                 # the scripted worked example (hopper-fake)
cargo run -- --fake --shot out.png --keys F10,F10,Alt+Left   # headless render (lavapipe works)
cargo run -- <trace-dir>                      # not wired yet: prints an error
```

## Building

`gui/` is its own cargo workspace. It builds against sibling checkouts beside the hopper checkout:

| Crate | Path |
|---|---|
| skelter, skelter-base/-core/-canvas/-compositor/-platform/-text/-widgets/-test | `../../svelters/crates/` |
| skelter_berth, berth-core | `../../berth/crates/` |
| skelter_quire, quire-core (and quire-text, through a `[patch]`) | `../../quire/crates/` |
| hopper-core, hopper-fake | `../crates/` |

`[patch]` sections point the git dependencies of skelter_berth, skelter_quire and skelter-widgets
(svelters and quire by `branch = "master"`/rev) at the same sibling paths, so the graph holds one
copy of each, as cadatomic's root manifest does. AccessKit comes from the fork svelters patches in
(`github.com/akiselev/accesskit` at `b22e1418`), fetched by cargo over the network on the first
build. `Cargo.lock` was seeded from svelters' lock so third-party versions match what svelters
builds.

No extra build steps: hopper uses skelter from Rust only, not the Svelte backend, so there is no
QuickJS, svelters compiler or `pnpm install`. fontconfig is loaded at run time (no
`libfontconfig-dev` needed to build).

### Running

The window needs, at run time on Linux (Debian/Ubuntu package names):

- a wgpu adapter: a GPU driver, or Mesa's software Vulkan (`mesa-vulkan-drivers`, which provides
  lavapipe) with `libvulkan1`; `libegl1`/`libegl-mesa0` for the GL fallback;
- on X11 (winit, skelter's Linux default): `libxkbcommon-x11-0`, `libxcursor1`, `libxi6`,
  `libxrandr2`.

A cloud container had none of these; after installing them both `--shot` and the real window under
`Xvfb :99` ran on llvmpipe.

## Keys

| Key | Does |
|---|---|
| F10 / Ctrl+F10 | step over / reverse step over |
| F11 / Ctrl+F11 | step into / reverse step into |
| Shift+F11 / Ctrl+Shift+F11 | step out / reverse step out |
| F5 / Shift+F5 (or Ctrl+F5) | continue / reverse continue |
| Alt+Left / Alt+Right | navigation back / forward |
| Ctrl+L | open a position URL (`hopper://…`); Enter opens it, Escape closes the box |
| Ctrl+Shift+C | copy the current position URL |

Mouse: a press on a local traces its origin into the Query pane (one row per hop, with its
ownership label and confidence); its triangle (or Left/Right) expands it lazily; a double click on
a Query row seeks to that hop. A press on a hit count lists every execution of the line. The
timeline seeks on press and drag (snapping to the nearest known moment). In Calls, a double click
expands a call or loads the next page; selecting one shows its entry and exit to jump to.

## Layout of the code

| Module | What |
|---|---|
| `app` | `Viewer`: the state; every `Debugger` call is a job, stale replies are dropped by stop generation |
| `jobs` | two std-thread lanes (replay, status), completions drained once per frame, an idle barrier |
| `tree` | `LazyTree`: a paged lazy tree flattened into visible rows (locals, calls) |
| `calls` | the call tree as a `RowModel` for skelter's virtual `table` |
| `nav` | the keymap and the navigation-history mirror |
| `gutter` | hit counts and arrows to gutter rows |
| `timeline_model`, `timeline` | the timeline's mapping, and its kind on skelter-canvas |
| `rows` | `hopper-rows`: virtual rows with indentation, triangles and per-cell tones |
| `source` | `hopper-source`: quire read-only plus hopper's gutter column and line bands |
| `ui` | the window's nodes, the berth dock and its default layout, the sync from the viewer |
| `document` | the `DocumentApi` around skelter's `CoreDocument`: keys first, event routing, frames |
| `run` | the platform handler, the headless harness and `--shot` |

What skelter, berth and quire lack is in `../docs/requests.md`.
