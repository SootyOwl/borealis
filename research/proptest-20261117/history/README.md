# History compaction sequence investigation

Base: audited commit `17f2a970` (subsequently promoted to local `main`).

Confirmed contract violation: messages appended after all live history disappears can receive a sequence number at or below the stored compaction boundary. `load_summary_and_messages` then omits new messages from the context it returns. The smallest generated witness is `Append → Compact(all) → Append`. A second deterministic regression compacts a prefix, deletes the remaining turn, and appends again.

The allocator previously used only `MAX(messages.seq) + 1`; deletion removes that watermark while `conversation_summaries.compacted_up_to` remains durable. The fix allocates above the maximum of both values. This is a visibility repair, not a promise that arbitrary deletion without a summary preserves every previously issued sequence number.

`tests/history_compaction_regression.rs` contains two standalone deterministic regressions. Both failed before the production change with `[]` instead of `["new"]`. The broad suite lives in `tests/proptest_history.rs` and compares raw and summary-filtered message contents after every operation against independent Vec models. It generates two conversations, a small reused-turn domain, appends, prefix/full compaction, turn deletion, reads, and targeted append-after-compaction families. The model selects compaction prefixes; production sequence numbers serve only as opaque handles for those selected messages. No SQL sequence formula is copied into the oracle. Persisted seeds and readable minimized traces are retained.

The harness found the actual defect immediately, demonstrating sensitivity to missing compaction watermark handling. Quiet deletion and multi-conversation variants remain part of the harness after repair. The finite generated histories do not establish concurrency correctness; invalid/corrupt database rows, sequence exhaustion, and failure injection are outside this bounded investigation.

Commands (normal development toolchain):

```sh
PROPTEST_CASES=1024 cargo test --test history_compaction_regression --test proptest_history
PROPTEST_CASES=1024 cargo test
```

This desktop execution environment required a host GCC wrapper and explicit OpenSSL paths. Prefix commands here with:

```sh
PATH=/tmp/borealis-build-tools:$PATH \
CC=/tmp/borealis-build-tools/cc AR=/tmp/borealis-build-tools/ar \
OPENSSL_LIB_DIR=/run/host/usr/lib64 OPENSSL_INCLUDE_DIR=/run/host/usr/include \
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=/tmp/borealis-build-tools/cc \
CARGO_TARGET_DIR=/tmp/borealis-proptest-history-target CARGO_BUILD_JOBS=2
```

The first sandboxed full run passed all 378 unit tests, 10 config tests, 2 deterministic regressions, 5 history integration tests, 18 memory integration tests, and both property tests, but all 15 provider integration tests failed because localhost mock-server binding was blocked. These are environment failures; unrestricted verification is tracked separately below.

Final verification: a clean package rebuild in the isolated target directory passed the complete suite with `PROPTEST_CASES=1024`: 378 unit tests, 10 config tests, 2 deterministic regressions, 5 history integration tests, 18 memory integration tests, 2 property tests, and 15 provider tests (430 total). Each property ran 1,024 generated cases plus persisted regressions. Localhost access was enabled for the provider mocks. The final output is retained in `full-green.log`.

Build isolation finding: sharing one Cargo target directory across distinct workspaces produced stale package artifacts (fixed source ran an older library). Copying the cache alone retained the stale result. Validation was therefore repeated after `cargo clean -p borealis` in `/tmp/borealis-proptest-history-target`, which is dedicated to this worker. Reuse dependency artifacts only; clean or rebuild the package when changing workspaces.
