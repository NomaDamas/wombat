# wombat

Rust CLI that parses coding-agent session logs into a mirror directory of
normalized text, which the `minsync` CLI indexes and searches.

- MinSync is used only as an external binary (`minsync init|sync|query`); never
  link the `minsync` crate. Require minsync >= 0.6.0.
- Mirror files must be prefix-stable: new turns are only appended, so MinSync
  chunk hashes of existing content do not change. No volatile values in bodies.
- Strip tool-output dumps and system prompts in the parser, not downstream.
- Every mirror file records its source session path, session id and turn offsets.
- Code is the source of truth; do not trust prose docs over behavior.
