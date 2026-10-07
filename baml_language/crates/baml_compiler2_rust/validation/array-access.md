# Native array access — 7 October 2026

Generated `int[]` reads, writes and length checks now call concrete shared Rust
functions. The measured release sorting body inlines all of them. Its only
remaining indirect call site is cooperative GC polling.

Implementation: `10ec31cbff`, compared with PR foundation `03c4a3569a`.
Intermediate `dc052d85da` made the calls static; LLVM still kept get/set out of
line. The final change marks only those two hot accessors `inline(always)`.

## Why this boundary

The old generated body used `&mut dyn CompiledRuntime` for every array access.
Its release assembly contained 25 indirect array-operation call sites plus the
GC poll. Code generation now calls `bex_vm_types::compiled` array functions
directly and passes the existing `PermitProof` for the resume interval.

The helpers are unsafe because a permit alone does not make an arbitrary saved
pointer current: operands must also be live, rooted values from that heap.
Generated state and call-boundary rooting supply that invariant. No borrowed
heap storage or guard escapes a helper, and the runtime restores saved state
before parking. Allocation and control/telemetry hooks remain on the runtime
trait. Neither the frame representation nor the collector/scheduler was replaced.

The same container lock still protects each access. Integer writes expose only a
mutable slice, which cannot change the backing allocation's length/capacity.
They therefore incur no payload allocation delta; the stored integer cannot
create a GC reference. Resizing retains metered guards, and heap-valued writes
still require a reference barrier. The negative-index resolver moved to the
shared types crate rather than being copied into the native backend.

## Matched measurements

Both revisions used Rust 1.98.0 / LLVM 22.1.8, release O3, fat LTO, one codegen
unit, CPUs 15/16 and two Tokio workers on shared aarch64 Spark. No native-entry
counters were present in timing binaries. Bytecode/native modes within a build
used the same binary and embedded program, differing only in native installation.

The first randomized matrix retained 800 trials across all four telemetry modes.
Other host work produced large wall-time outliers. A separate 160-trial repeat
compared all four old/new bytecode/native arms consecutively in randomized
blocks, with off/high telemetry and ten processes per cell. Neither collection
was pooled with the other or had inconvenient trials replaced.

Paired-repeat medians, milliseconds per job:

| Workload / telemetry | Old bytecode | Old native | New bytecode | New native |
| --- | ---: | ---: | ---: | ---: |
| Full sort job, off | 1.4792 | 1.2223 | 1.4758 | 1.1311 |
| Full sort job, high | 1.5163 | 1.2740 | 1.5129 | 1.1733 |
| Restore + 16 sorts, off | 16.9559 | 12.9108 | 16.9140 | 11.2758 |
| Restore + 16 sorts, high | 17.0271 | 12.9358 | 17.0741 | 11.3620 |

The complete sort job uses about 7–8% less CPU than the previous native
implementation and about 23% less execution time than the current bytecode
control (roughly 1.3x faster). The restore/sort batch uses about 12% less CPU than
previous native execution and is roughly 1.5x faster than bytecode. The latter
includes indexed restoration and fixed setup; it is not a pure-sort timing.
Current bytecode controls do not show a material shift on these two workloads;
this does not establish performance for every interpreted program.

The full-job boundary still includes bytecode copying, scratch-array creation,
JSON serialization and engine-call work. Classes/strings/iterators were not added
to native admission. These results do not establish a general multi-X gain or
a win against V8.

An additional 160-trial confirmation explicitly unset `LD_PRELOAD`, `CPUPROFILE`,
`CPUPROFILE_FREQUENCY` and `CPUPROFILE_REALTIME`, with BAML telemetry off. CPU medians
were 1.5128 / 1.2516 / 1.1627 ms for previous bytecode / previous native / new native
whole-job execution, and 17.5247 / 13.2361 / 11.6071 ms for the restore/sort batch.
The original timing runs also had no CPU profiler loaded; the confirmation
reproduces their CPU improvement under additional shared-host scheduling noise.
[All confirmation rows are retained](array-access-unprofiled.csv).

## What the CPU profiles explain

Separate whole-process gperftools samples used telemetry off and long execution
to make setup a small fraction. Sampling is statistical and optimized code has
limited source-line fidelity.

- Original full-job profile: about 30% flat in `__aarch64_cas1_acq`.
- New full-job profile: about 40% flat in that routine.
- New restore/sort profile: about 64% flat in that routine.

This is the outlined atomic compare-exchange used to acquire array locks, not
evidence that the workload spends that fraction waiting on contended locks.
Inlining removed the helper calls and exposed type checks to optimization, but
did not remove synchronization. Static call-site counts are not dynamic operation
counts. Larger gains need investigation of atomic lowering or safe reductions in
lock frequency, with explicit aliasing, concurrency and safepoint contracts.

## Costs and limits

The sort resume function grew from 2,780 to 7,004 machine-code bytes. The full
measurement executable grew by 62,144 bytes, about 0.16%; source grew from 37,970
to 38,623 bytes. These are measurement-host artifacts, not minimal deployment
sizes. Build wall observations had differing cache/load conditions and support
no build-speed conclusion.

Native installation/setup still raises full-host readiness from roughly 16.8 ms
to 27.1 ms in the new build. This change does not optimize that cost. Peak RSS
was approximately 29.8 MiB for both new execution modes; no broad memory win is
claimed. Full application coverage, runtime pruning, startup optimization and
deployment/platform acceptance remain separate work.

## Validation and reproduction

- VM types: 134 normal / 131 `heap_debug` tests; VM library: 107 in each mode.
- Eight compiled VM contracts and all 15 generated-code groups passed in each
  mode, plus the emitter's assignment-analysis unit test.
- 504 release/debug host correctness processes checked the independent full
  sort or checksum oracle. This includes 84 diagnostic processes proving actual
  native factory entries by object identity.
- 40 small matrix checks, 800 primary timings, 160 paired timings and another
  160 explicitly unprofiled/off confirmation trials passed;
  all 680 enabled recordings in the two timing collections passed the host's
  structural/root/function-count and delivery checks.
- Strict targeted Clippy, formatting, Stow and the shared VM Wasm check passed.

`BAML_NATIVE_TEST_RELEASE=1 cargo test -p baml_compiler2_rust --test native
--features heap_debug generated_ -- --test-threads=1` builds the generated GC and
engine-contract executables with O3/fat LTO/one codegen unit. This opt-in test mode
allows runtime contracts to be exercised under the production optimization profile.
Both optimized generated contracts passed, including forced moving collections,
mixed suspension, cancellation/cleanup and recording assertions.

The exact [full-job source](array-access-whole.baml),
[restore/sort source](array-access-compute.baml) and
[1,024-element input](array-access-input.json) are checked in.

Raw process rows and summaries are retained in
[the evidence record](array-access-2026-10-07.json),
[the randomized matrix](array-access-timings.csv) and
[the paired repeat](array-access-paired.csv).
The complete local collection is
`boundary/instrumentation/measurements/2026-10-07-array-access/`, including build
provenance, bytecode/source inputs, observations, decoded recordings, assembly,
CPU sample files, DOT/Callgrind graphs and entry diagnostics. The runner is
`instrumentation/machinery/rust_backend/array_access.py` (`build`, `verify`, `run`).

The earlier kernel-only oracle incorrectly expected an array instead of its
checksum. It was corrected independently, with a test confirming that the
diagnostic and full-output case contain identical sorting kernels. Earlier
failed results remain failed. One preparation build whose source changed while
building was rejected and excluded before timing. No artifact validation was
disabled and no executed artifact was normalized or rewritten.
