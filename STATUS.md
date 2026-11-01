# hopper status

## Handoff (coordinator, 2026-11-01)

Implementing the plan (`docs/plan/2026-11-01-first-implementation.md`) in lanes:
- A: hopper-rsp, hopper-rr, experiment scripts (M0, M1 transport)
- B: hopper-symbols, hopper-eval (M1 values)
- D: hopper-sidecar, hopper-build (M5)
- F: hopper-fake, hopper-mcp, hopper-dap, hopper-cli (M4/M7 adapters, M1 CLI)
- E: gui/ (M2)
Next: hopper-index (M3, M6) and hopper-engine (implements `Debugger`), wired into the CLI, MCP, DAP and GUI.

## User decisions

- 2026-11-01: hopper may depend on unreleased svelters, berth and quire code (sibling checkouts
  `../svelters`, `../berth`, `../quire`).
- Open: licence; toolchain policy for sidecar builds (pinned nightly); where recording runs;
  whether recordings holding Parasolid memory may leave the user's machines; asking cadabra3 for
  a serialised decision-event stream.
