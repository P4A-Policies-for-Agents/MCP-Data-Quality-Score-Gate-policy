# Changelog

## 1.0.1

- Shorter asset description: Exchange limits it to 256 characters, so 1.0.0 had no
  implementation.

## 1.0.0

- First release. Gates MCP `tools/call` on the CDGC data quality rule occurrences
  catalogued on the MCP Server's tools and output fields.
- Combines rule scores into a tool score (minimum or average). Blocks below
  `blockBelow` before the upstream is called, and warns below `warnBelow`, below a
  rule's own threshold, or on stale scores.
- Adds a `_dq` report and `x-dq-*` headers to delivered results. The report includes the
  previous score when a rule's score changed, so a data fix shows as before → after.
- `enforcement: annotate` reports without refusing. Fail-open on IDMC errors by default.
