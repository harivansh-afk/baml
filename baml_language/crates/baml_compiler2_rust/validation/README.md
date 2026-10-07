# Foundation validation — 2026-10-07

The cleanup keeps the supported value types unchanged. It prepares admission
facts/source sites once, uses typed protocol indices, and requires execution of
each expected native function instead of accepting any native activity.

## Bytecode-only comparison

Base: `2bfe3889c015785e001f09b26518463b2a5346d9`.
Runtime under review: `9824b278ce` (subsequent changes concern native emission,
test coverage and documentation, not the bytecode execution loop).

These five sampled workloads show no slowdown. The small reductions do not
establish a general speedup. This is neither a native-backend benchmark nor a
claim about startup, other hardware, telemetry-enabled costs or all programs.

| Existing speedtest workload | Base median, ms | Changed median, ms | Change |
| --- | ---: | ---: | ---: |
| array build sum 100k | 36.060 | 35.960 | -0.28% |
| call chain 100x10k | 19.030 | 18.740 | -1.52% |
| closure apply 1m | 179.500 | 176.600 | -1.62% |
| nested loops 500x500 | 6.010 | 5.864 | -2.43% |
| pure call 1m | 38.180 | 37.300 | -2.30% |

Method: existing `baml_tests/benches/runtime_benchmark.rs`, O2 BAML, Rust 1.98.0,
release optimization 3, fat LTO, one codegen unit; aarch64 Linux on Spark.
Five shuffled process trials per workload/revision, up to 16 single-iteration
samples per trial with a two-second cap, OS timer, CPUs 15/16, two Tokio workers,
`BAML_TELEMETRY=off`. The table takes the median of the five trial medians.
Compilation and engine construction are outside timing; engine call/context
overhead remains inside. The host was shared, not isolated or frequency-locked.
Per-trial ranges, sample counts, binary hashes, compiler identity and all Divan
output are retained in [the raw results](interpreter-2026-10-07.json).

Both revisions returned the independently calculated integer result for every
selected workload before timing. `baml_tests/examples/verify_runtime_result.rs`
uses the same O2 compiler entry as the benchmark. The expected values are in
the raw results; they come from sums/counts in the workload definitions.

To reproduce:

1. Check out the base and changed revisions in separate worktrees. Use separate
   Cargo target directories; reusing one between worktrees produced stale local
   dependency artifacts during setup, and that failed build was discarded.
2. In each `baml_language/`, build with the pinned toolchain:
   `cargo bench --offline -p baml_tests --bench runtime_benchmark --no-run`.
   Preserve each resulting executable from its target's `release/deps/`.
3. Export source with `uv run --no-project tools/speedtest/export_baml.py`.
   Build/run `verify_runtime_result` with each selected source and expected int
   on both revisions, with telemetry off. Copy the example into the base
   worktree for this check; it was added after that revision.
4. Run `uv run --no-project compare_interpreter.py --base BASE_BINARY
   --head HEAD_BINARY --output NEW_RESULTS.json --cpus 15,16` from this folder.
   The runner passes Divan's `--bench` flag explicitly and retains every trial.

## Correctness and integration

- 987 tests passed across the selected base/type/MIR/emitter/database/linker,
  VM/engine and Btel library suites.
- Compiler corpus: 771 passed; four pre-existing ignored `Array.filled`
  mutable-alias warning tests remain ignored. Existing snapshots passed without
  changes.
- Offline BAML corpus: 5,249 passed plus two intentionally tolerated failures
  testing the runner's pass-rate/fail-fast behavior; aggregate passed. Three
  telemetry-off cases passed. The two missing-provider configuration tests
  also passed separately with `ANTHROPIC_API_KEY` absent.
- Native differential/contracts: all 15 groups passed normally and with
  `heap_debug`, including the generated child executables in that mode.
- VM compiled contracts: all eight passed normally and with `heap_debug`.
- Bytecode source locations: independent expected lines passed at O0/O1/O2;
  multiline attribution deliberately uses the normalized expression start.
- The exported Cargo project built, executed and enforced artifact telemetry
  overrides. VM `wasm32-unknown-unknown` checking passed.
- Strict targeted Clippy (including native `heap_debug` and the result-verifier
  example), formatting and Stow passed. Dependency-tree checks keep the engine
  out of production emitter dependencies and Rust generation out of the
  standalone packed host.

The differential oracle shares this branch's interpreter, which is why the
existing corpus, independent source/arithmetic assertions and base comparison
are separate checks. Local validation does not establish remote CI success,
SDK/platform coverage, two-person MIR/Emit design acceptance, or deployed Btel
consumer acceptance of minor 11. Those remain review/release boundaries.
