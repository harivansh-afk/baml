# MIR to Rust

This backend generates resumable Rust bodies for BAML functions. The existing BEX runtime owns their invocation, heap, scheduling, errors and telemetry. Bytecode remains the development interpreter, the implementation of unsupported functions, and the executor for runtime-compiled code.

```
checked BAML -> MIR -> bytecode -> interpreter
                  \-> Rust -> rustc/LLVM -> compiled bodies
                                  |
                       shared BAML invocation frames
                       and existing BEX services
```

## Ownership

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

A generated body owns typed locals and a continuation. `resume` returns `Return`, `Call` or `Yield`, or a runtime error. The VM drives calls and suspension; Rust's call stack never stores a suspended BAML caller. Recursion therefore uses BAML's existing frame limit.

`BamlFrame` owns the common invocation state. Its execution state selects bytecode or a compiled body. Compiled BAML functions retain BAML identity and observation rules; they are distinct from hidden Rust builtins. Both backends use the same successful-return and exception-unwind machinery.

Arguments remain rooted on the eval stack until a compiled body takes ownership. `RootHaver` exposes and forwards array references owned by suspended compiled state. Array locals hold BEX references, so copying or passing an array preserves its identity without copying its contents. Allocating an array creates a new object through the VM's existing TLAB.

`CompiledHeap` exposes checked integer-array operations during one active heap permit. Array accesses use the existing container locks, index rules and mutation barrier. Helpers return values, never borrowed heap storage or guards; nothing borrowed survives a call or yield. Roots are currently conservative: an array local stays rooted until overwritten or its activation ends. Precise last-use clearing is future optimization work; MIR `Drop` evaluates and discards a value and does not end the source local's lifetime.

Loops cooperate through the existing yield checker. GC waits for permits to be released. Cancellation remains a language checkpoint at sys-op/await boundaries, with shielding inherited through compiled calls from interpreted cleanup. A GC yield does not introduce a new cancellation point.

Source sites are independent of resume block numbers. Both emitters derive diagnostic lines from the same normalized span start, independent of bytecode sequence-point placement. Btel records compiled functions and exact compiled-site coordinates using format minor 7. Entry, call, panic and restored-caller locations stay meaningful across yields and hidden builtin continuations. Calls can be observed or hidden through the existing policies; explicit input/output captures also work across compiled boundaries.

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

The current host links the full runtime, including runtime compilation. Neither binary shrinking nor a speedup is established by this implementation. The shared VM can compile for Wasm; the packed executable host is native-only.

## Validation and remaining work

The differential suite builds generated Rust and runs it through the real engine. It checks arithmetic boundaries, evaluation order, control flow, recursion, fallback, errors and actual compiled execution. Array cases require every named target to compile and execute, and check allocation, returned arrays, alias-visible mutation, rebinding and error order. The array-contract fixture forces real moving collections with arrays rooted only by generated state, collects while a compiled caller is suspended, and exercises mixed calls, spawn and sustained allocation. The engine-contract fixture checks cancellation, cleanup shielding, logical call relationships, explicit captures, and recorded source locations through the recorder/reader. Separate VM tests exercise state-only roots, forced loop yields, stale bindings and hidden continuation failures.

On the checked 64-bit build without `heap_debug`, the common frame is 128 bytes (previously 112). Each compiled activation has one state allocation; calls currently construct argument vectors and return through the VM. These costs are explicit. Rust can optimize inside a resume body and visible arithmetic helpers, but native-to-native calls do not yet become direct inlinable Rust calls.

Before making this the default release backend:

1. Measure optimized execution, call boundaries, allocation, build/startup time, binary size and telemetry modes on the same programs. Inspect generated assembly before assuming block dispatch disappears.
2. Extend heap operations beyond integer arrays through existing access guards and barriers, and add liveness-aware roots. Preserve object identity and distinguish parameter rebinding from object mutation.
3. Admit handlers, closures and generics only with their full runtime contracts and differential cases.
4. Add a bounded direct-call optimization that preserves logical frames, error sites, stack limits and cooperative checkpoints. Keep the resumable path as its semantic reference.
5. Design capability-based runtime pruning and versioned runtime-source distribution. An absence of source-level reflection alone is not proof that LTO can discard the compiler or interpreter.
