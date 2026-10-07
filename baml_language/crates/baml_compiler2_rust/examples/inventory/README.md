# Inventory batch demo

A small CLI application with separate models, request validation, and array
operations. It applies stock adjustments, clamps shortages to zero, and reports
total stock and the number of entries below a reorder threshold.

`apply_changes` and `summarize` compile to Rust. `main` uses bytecode because it
handles class objects and throws a validation error. The existing stock array
passes between both backends without copying its contents; `summarize` allocates
a new two-element BEX array for its result.

From `baml_language`, using the workspace's Rust toolchain:

```sh
rustup run 1.98.0 cargo build -p baml_cli --bin baml-cli
target/debug/baml-cli pack main \
  --from crates/baml_compiler2_rust/examples/inventory \
  --emit-rust-project target/int-array-inventory \
  --runtime-source .
rustup run 1.98.0 cargo build --release \
  --manifest-path target/int-array-inventory/Cargo.toml --target-dir target
target/release/baml-app \
  --json-args @crates/baml_compiler2_rust/examples/inventory/request.json
```

The export directory must be new. `native-support.txt` reports two compiled
functions and one fallback. Expected output for `request.json`:

```json
{"stock":[7,7,4,0],"total":18,"low_stock":2}
```

For the interpreter comparison:

```sh
target/debug/baml-cli run main \
  --from crates/baml_compiler2_rust/examples/inventory --output-format json \
  -- --json-args @crates/baml_compiler2_rust/examples/inventory/request.json
```

Set `BAML_TELEMETRY=off` or `BAML_TELEMETRY=high` before running either command
to exercise both modes. This demo establishes application behavior and mixed
backend execution; it is not a performance benchmark.

Run deterministic batch checks, including empty input, validation failures and
integer overflow, against both executables with telemetry off and high:

```sh
uv run --no-project crates/baml_compiler2_rust/examples/inventory/verify.py \
  --cli target/debug/baml-cli --binary target/release/baml-app
```
