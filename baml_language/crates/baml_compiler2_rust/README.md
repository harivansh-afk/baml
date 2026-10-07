# MIR to Rust

This backend generates Rust bodies for BAML functions. Entry from the runtime remains resumable; bounded scalar calls between generated bodies use named Rust functions. The existing BEX runtime owns their invocation, heap, scheduling, errors and telemetry. Bytecode remains the development interpreter, the implementation of unsupported functions, and the executor for runtime-compiled code.

```
checked BAML -> MIR -> bytecode -> interpreter
                  \-> Rust -> rustc/LLVM -> compiled bodies
                                  |
                       shared BAML invocation frames
                       and existing BEX services
```

## Ownership

Compiler hosts call `baml_db::rust::emit_module`, enabled by the opt-in
`baml_db/rust-backend` feature. The driver supplies the exact linked package map,
live source hash and canonical declaration-coordinate resolver. The Rust emitter
uses name/type/MIR queries and runtime data; it has no bytecode-emitter or engine
implementation dependency. Runtime-only hosts leave this compiler feature off.

| Responsibility | Owner |
| --- | --- |
| Admission, typed locals, continuations, Rust emission | `baml_compiler2_rust` |
| Integer range and arithmetic | `baml_type::Int63` |
| Runtime panic payloads, compiled body protocol and image bindings | `bex_vm_types` |
| BAML call frames, calls, returns, unwinding and backend dispatch | `bex_vm` |
| Heap access, moving collection and permit coordination | `bex_heap` |
| Futures, spawn, I/O and cancellation | `bex_engine` |
| Logical invocation records and source maps | Btel |

There is no separate `bex_native` runtime. The prototype's integer helpers now have shared owners. VM tagged fast paths remain representation-specific; differential tests compare their results with generated code.

## Execution contract

The production integration worktree tracks canary `907a6d9c50` (2026-10-06).
Recording describes logical BAML calls independently of the executor. Declaration
trace hooks are a separate selection phase: they can execute BAML and suspend
before the target starts. Functions with any declared hook currently retain
bytecode, with an explicit admission reason; their callers can remain native.
Installation also rejects compiled overrides that would skip a hook prologue.
This preserves built-in Hidden/Timing/Span/Rich modes, custom hooks, suppression,
and telemetry-off bypass through the current VM rather than duplicating policy.

A generated body owns typed locals and a continuation. `resume` returns `Return`, `Call` or `Yield`, or a runtime error. The VM drives calls and suspension; Rust's call stack never stores a suspended BAML caller. Recursion therefore uses BAML's existing frame limit.

`BamlFrame` owns the common invocation state. Its execution state selects bytecode or a compiled body. Compiled BAML functions retain BAML identity and observation rules; they are distinct from hidden Rust builtins. Both backends share logical completion and exception-unwind rules. Bytecode retains its specialized in-place return housekeeping.

## Direct-call experiment

`direct.rs` conservatively selects scalar functions with acyclic control flow and an acyclic eligible call graph. A region is limited to 256 work units (local slots plus MIR statements/terminators, including all transitive calls), 16 calls deep and 128 locals per function. Branches are summed rather than choosing a path. These limits bound generated work between the existing cooperative checks; they are not a wall-clock latency guarantee. Heap-valued functions, loops, recursion and calls to unsupported functions remain resumable. A looping or array-using caller can still call an eligible scalar callee directly.

Each resumable-to-direct call charges that complete transitive work bound to the
existing yield checker before entering the region, including calls that fail.
Nested direct callees are not charged again. Exhaustion makes the next existing
resumable block checkpoint poll; it never parks inside a direct Rust chain or
adds a cancellation point. The counter saturates at a pending poll, so a region
cannot wrap the counter or postpone an already-due check.

Generated `direct_*` functions use typed Rust parameters, locals and results. They have no boxed continuation or heap argument vector. They still use `CompiledRuntime` entry/exit hooks to retain a logical BAML frame, check the existing stack limit and record telemetry. Hooks do not dispatch a body, park or yield. This path is active with telemetry enabled; it does not hide calls or disable captures. `native-support.txt` includes the per-function eligibility and exclusion reason.

On error, direct activations leave their logical frames and deepest source site for the existing unwinder. On success, their result stays in Rust storage and their metadata is popped without touching the evaluation stack. Bindings are checked against the installed descriptor before entering a direct body.

During `resume`, the owning boxed state is temporarily held outside the frame vector so hooks can grow that vector safely. The heap permit stays active throughout; state is restored before any GC handoff or error materialization. Direct regions contain only scalars. The yield checker is cloned once per resume and its exact counter restored afterward, keeping loop checks statically callable while avoiding overlapping mutable borrows of the VM. This adds flag-reference refcount work per resume and must be included in measurements.

Both entry shapes reuse the same MIR operation emitter. Admission is established before call-graph specialization, so a rejected callee cannot accidentally get a direct entry point. The bounded helper and resumable entry currently duplicate generated body code; measure binary/build size as well as execution time. Runtime hooks, global/descriptor checks, logical metadata and telemetry costs remain. This branch establishes an experiment, not a speedup claim.

Arguments remain rooted on the eval stack until a compiled body takes ownership. `RootHaver` exposes and forwards array references owned by suspended compiled state. Array locals hold BEX references, so copying or passing an array preserves its identity without copying its contents. Allocating an array creates a new object through the VM's existing TLAB.

`CompiledHeap` exposes checked integer-array operations during one active heap permit. Array accesses use the existing container locks, index rules and mutation barrier. Helpers return values, never borrowed heap storage or guards; nothing borrowed survives a call or yield. Roots are currently conservative: an array local stays rooted until overwritten or its activation ends. Precise last-use clearing is future optimization work; MIR `Drop` evaluates and discards a value and does not end the source local's lifetime.

Loops cooperate through the existing yield checker. GC waits for permits to be released. Cancellation remains a language checkpoint at sys-op/await boundaries, with shielding inherited through compiled calls from interpreted cleanup. A GC yield does not introduce a new cancellation point.

The inlined checkpoint decrements the loaned checker; only a due checkpoint
calls the runtime to settle TLAB payload debt before polling pressure. Array
allocation notifies that same checker when spending crosses the GC budget, and
array writes use the current allocation-metered guard. Installed compiled
bindings participate in the heap footprint census. GC root spans measure the
collection pause, not the mutator's park wait; keep those measurements separate.

Source sites are independent of resume block numbers. Both emitters derive diagnostic lines from the same normalized span start, independent of bytecode sequence-point placement. Btel records compiled functions and exact compiled-site coordinates using format minor 11. Entry, call, panic and restored-caller locations stay meaningful across yields and hidden builtin continuations. Calls can be observed or hidden through the existing policies; explicit input/output captures also work across compiled boundaries.

## Admission and binding

The current generated subset is `int`/`bool` and `int[]` locals, arguments and results, arithmetic, branches, indexed loops, integer switches, short-circuit logic and direct calls. Integer arrays support literals (including empty arrays), length, indexed reads and writes, aliasing and parameter rebinding. Negative indices and bounds panics follow the interpreter. Array equality calls the existing runtime builtin, preserving content equality. A direct callee may remain interpreted and may suspend. Other heap types, iterator-based `for-in`, array methods such as `push`, closures, methods, generics, default/literal parameters and compiled handlers remain bytecode when unsupported. `native-support.txt` explains each fallback.

Unsupported features are a per-function decision. Missing linked identities, malformed MIR and violated initialization/type invariants are build errors, never reasons to silently interpret broken compiler output.

Bindings use the declaring source root and structured declaration path, then the exact linker's package position and global slot. Display names do not bind code. Generation checks the source-content identity. Installed descriptors retain a serialized-image fingerprint and object index; the loader validates them before transformation. Serialization drops machine-code pointers. Relinking or grafting into another index domain removes the compiled implementation and retains the portable body.

## Build an executable

From a BAML toolchain that includes this backend:

```sh
baml pack main --file main.baml \
  --emit-rust-project ./native-app \
  --runtime-source /path/to/baml/baml_language
cd native-app
cargo build --release
./target/release/baml-app --help
```

The runtime crates are not published, so the source checkout is explicit and must match the compiler revision. The generated project copies the dependency lock and toolchain, configures fat LTO, one codegen unit and optimization level 3, and reuses `baml_pack_host` for arguments, output, exit codes and runtime behavior. Existing bytecode-only `baml pack` output is unchanged. This export produces a Cargo project; it does not claim to have built the executable.

Both host paths verify the current artifact telemetry policy before runtime setup, including permitted environment overrides. The current host links the full runtime, including runtime compilation. Neither binary shrinking nor a speedup is established by this implementation. The shared VM can compile for Wasm; the packed executable host is native-only.

## Validation and remaining work

The differential suite builds generated Rust and runs it through the real engine. It checks arithmetic boundaries, evaluation order, control flow, recursion, fallback, errors and actual compiled execution. Array cases require every named target to compile and execute, and check allocation, returned arrays, alias-visible mutation, rebinding and error order. The array-contract fixture forces real moving collections with arrays rooted only by generated state, collects while a compiled caller is suspended, and exercises mixed calls, spawn and sustained allocation. The engine-contract fixture checks cancellation, cleanup shielding, logical call relationships, explicit captures, and recorded source locations through the recorder/reader. Separate VM tests exercise state-only roots, forced loop yields, stale bindings and hidden continuation failures.

On the previously checked 64-bit build without `heap_debug`, the common frame was 128 bytes (previously 112). Resumable entries retain a state allocation and general calls construct argument vectors. Eligible direct callees avoid those allocations and expose concrete Rust calls, but keep logical BAML metadata. Remeasure layout, generated code and execution on this branch before attributing a performance change.

Before making this the default release backend:

1. Measure optimized execution, call boundaries, allocation, build/startup time, binary size and telemetry modes on the same programs. Inspect generated assembly before assuming block dispatch disappears.
2. Extend heap operations beyond integer arrays through existing access guards and barriers, and add liveness-aware roots. Preserve object identity and distinguish parameter rebinding from object mutation.
3. Admit handlers, closures and generics only with their full runtime contracts and differential cases.
4. Measure the bounded direct-call experiment against the corrected resumable baseline, including GC latency and telemetry modes. Keep the resumable path as its semantic reference; widen eligibility only with a new safety argument.
5. Design capability-based runtime pruning and versioned runtime-source distribution. An absence of source-level reflection alone is not proof that LTO can discard the compiler or interpreter.
