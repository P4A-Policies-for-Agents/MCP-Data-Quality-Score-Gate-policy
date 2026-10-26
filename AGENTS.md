# Agent notes

"MCP Data Quality Score Gate": MuleSoft Omni/Flex Gateway custom policy (PDK 1.10,
Rust → wasm32-wasip1, `enable_stop_iteration`) that gates MCP `tools/call` on the
Informatica CDGC data quality scores of the called tool.

Keep this repo free of tenant-specific names, ids and hosts. Tests use placeholder ids;
real exports for `live_eval` live outside the repo. Record releases in `CHANGELOG.md`.

- After editing `dq-score-gate-definition/gcl.yaml`, run `make release-local` in the
  definition dir (local publish to `~/.exchange`), then regenerate the config struct from
  `dq-score-gate-flex/` with
  `cargo anypoint config-gen --manifest ../dq-score-gate-definition/gcl.yaml --output ./src/generated/config.rs`.
- Pure logic, with unit tests: `src/rules.rs` (rule occurrences → per-tool scorecards,
  score history), `src/gate.rs` (settings, verdict, `_dq` report, messages),
  `src/payload.rs` (JSON-RPC framing and annotation). `src/lib.rs` is the gateway wiring
  (IDMC calls, cache, request and response legs).
- A block is decided on the request leg, before the upstream is called. Keep the
  denial convention: JSON-RPC error with `data.policy = "mcp-data-quality-score-gate"`.
- Never log credentials, session ids, tokens or IDMC response bodies.
- `demo/`: `dq_scenes.py` writes demo rule occurrences to the catalog. `config.json`
  and `env.local.sh` hold real values and are gitignored. Only commit
  `config.json.example`, with placeholders.
