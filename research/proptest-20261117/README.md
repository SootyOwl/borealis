# Property-testing investigation — 2026-11-17

Base: `17f2a970` (existing audit work, promoted and pushed to `main` at the user's request).
Discovery branch: `codex/proptest-discovery-20261117`.

## Confirmed findings

| Fix branch | Witness | Repair |
| --- | --- | --- |
| `codex/fix-history-compaction-sequence` | Append → compact all → append hides the new message from summary-filtered history | Allocate above both live sequence numbers and the persisted summary boundary |
| `codex/fix-memory-pagination-offset` | One note, offset `usize::MAX` returns the first page | Return an empty page when offset is beyond total |
| `codex/fix-duration-overflow` | `(u64::MAX / 3600 + 1)h` panics in debug and wraps in release | Check unit-conversion multiplication and return an error |
| `codex/fix-system-id-roundtrip` | `System { event_name: ":" }` cannot parse its displayed canonical ID | Preserve the complete `system:` suffix |
| `codex/fix-history-budget-overflow` | Costs `[1, usize::MAX]` overflow during selection | Accumulate the longest fitting suffix with checked addition |

Each fix branch contains only a minimal production fix and standalone deterministic regression. Regressions were observed failing before their fix and passing afterward. An independent agent reviewed all five patches without finding must-fix issues. Broad models and generators remain on the discovery branch.

Scope: the history issue has direct context-loading impact. Pagination is a public storage API defect; the tool layer already clamps offsets. System-ID failure violates canonical roundtrip but no production parse caller was established. Budget overflow is public-API arithmetic correctness; ordinary 64-bit in-memory text histories are not expected to reach it. Duration repair covers conversion only, not every downstream scheduler range constraint. No security-sensitive implication was established for these five findings.

## Validation and reproducibility

```sh
PROPTEST_CASES=1024 cargo test -- --test-threads=1
```

The consolidated suite passed **440 tests**, including **8 properties × 1,024 cases**, plus persisted regressions. `combined-green.log` retains the complete output. The tests include independent Vec history models, note/tag/link/FTS mutation histories, wide-integer arithmetic oracles, enum serialization, and minute-enumeration active-hours checks.

Boundary generators deliberately include colons, threshold-adjacent durations, small and maximum budgets, zero and maximum costs. Memory histories cover retagging, deletion, link replacement, reopens, invalid operations and result multiplicity. Disabling the FTS update trigger was detected and shrank to `Create(zebra) → Update(quartz) → Link(self)`; the deliberate mutation was restored. Original failures and shrunk seeds are retained under `tests/*.proptest-regressions` and this directory.

The desktop distrobox needed a host GCC wrapper (`/run/host/usr/bin/gcc -B/run/host/usr/bin/ -fuse-ld=bfd --sysroot=/run/host`) with `LD_LIBRARY_PATH=/run/host/usr/lib64` scoped to the compiler, and `OPENSSL_INCLUDE_DIR=/run/host/usr/include`, `OPENSSL_LIB_DIR=/run/host/usr/lib64`. Local wrapper scripts are `/tmp/borealis-build-tools/cc`, `ar`, and `cargo-test`. On a normal development host, ordinary `cargo test` suffices. Provider integration tests require localhost socket access.

Shared Cargo target directories across distinct workspaces produced stale Borealis artifacts. Final runs used isolated output directories and a clean Borealis package rebuild. Do not reuse a compiled package across these workspaces without cleaning it. Dependency artifacts may be reused. The combined run used `/tmp/borealis-proptest-root-target`.

An existing parallel integration test race was observed: multiple memory tests use the same process-ID persona file. Serial execution passed; this unrelated fixture issue was not altered.

## Coverage limits and follow-up leads

- Finite generated histories are not proof of repository-wide correctness. No live bot, remote provider, or production database was exercised.
- History model checks two conversations and reused turns; does not inject transaction faults or prove concurrent compaction behavior.
- Memory model uses three content tokens and checks FTS membership, not complete query grammar or ranking; pagination ordering is not independently modeled.
- Soft-deleted note link-retention semantics remain ambiguous and were not called a defect.
- Follow-up candidates: concurrent ModeRouter first-use initialization may split digest buffers; very large jitter durations can exceed chrono/signed ranges. These remain unconfirmed here and have no patch or PR.
