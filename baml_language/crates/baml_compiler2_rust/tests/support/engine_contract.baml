function compiled_leaf(x: int) -> int { x * 3 }
function direct_middle(x: int) -> int { compiled_leaf(x) + 1 }
function direct_entry(x: int) -> int { direct_middle(x) + 1 }
function captured_direct(x: int) -> int {
    direct_entry(x, $trace = trace.span(inputs = true, output = true, error = true))
}
function compiled_entry(x: int) -> int { slow(x) + 1 }
function slow(x: int) -> int {
    let retained = [x];
    log.info("parked");
    baml.sys.sleep(baml.time.Duration.from_milliseconds(200n));
    compiled_leaf(retained[0])
}
function fail(x: int) -> int { 10 / x }
function fail_middle(x: int) -> int { fail(x) + 1 }
function failure_entry(x: int) -> int { fail_middle(x) + 1 }
function cancel_entry(x: int) -> int { wait_cancel(x) + 1 }
function wait_cancel(x: int) -> int {
    log.info("cancel-ready");
    baml.sys.sleep(baml.time.Duration.from_milliseconds(5000n));
    x
}
function through_cleanup() -> int { cleanup_wait() + 1 }
function cleanup_wait() -> int {
    log.info("cleanup-ready");
    baml.sys.sleep(baml.time.Duration.from_milliseconds(20n));
    41
}
function shield_entry() -> int {
    defer { through_cleanup(); }
    7
}
function spawn_entry(x: int) -> int { spawned(x) + 1 }
function spawned(x: int) -> int {
    let child = spawn { compiled_leaf(x) };
    await child
}
function captured(x: int) -> int {
    compiled_entry(x, $trace = trace.span(inputs = true, output = true, error = true))
}
