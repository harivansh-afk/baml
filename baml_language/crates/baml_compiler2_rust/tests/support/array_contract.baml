function scalar_step(n: int) -> int { n + 1 }
function allocated_loop(n: int) -> int[] {
    let a = [n, n + 1];
    let alias = a;
    let i = 0;
    while (i < 10) { i = scalar_step(i); }
    alias[0] += 1;
    a
}

function make_array(n: int) -> int[] { [n, n + 1] }
function mutate_array(a: int[]) -> int { a[0] += 2; a[0] }

function interpreted_array(a: int[]) -> int[] {
    baml.sys.sleep(baml.time.Duration.from_milliseconds(1n));
    mutate_array(a);
    a
}

function mixed_array(n: int) -> int[] {
    let a = make_array(n);
    let alias = interpreted_array(a);
    alias[-1] += 3;
    a
}

function pause_array(n: int) -> int {
    log.info("array-parked");
    baml.sys.sleep(baml.time.Duration.from_milliseconds(500n));
    n
}

function suspended_array(n: int) -> int[] {
    let a = make_array(n);
    let alias = a;
    pause_array(n);
    mutate_array(alias);
    a
}

function allocation_loop(n: int) -> int {
    let a = [0, 1];
    let i = 0;
    while (i < n) {
        a = [i, i + 1];
        i += 1;
    }
    a[0] + a[1]
}

function spawned_arrays(n: int) -> int[] {
    let a = make_array(n);
    let child = spawn { mutate_array(a) };
    await child;
    a
}

function increment_slot(a: int[], slot: int, n: int) -> int {
    let i = 0;
    while (i < n) { a[slot] += 1; i += 1; }
    a[slot]
}

function concurrent_arrays(n: int) -> int[] {
    let a = [0, 0];
    let child = spawn { increment_slot(a, 0, n) };
    increment_slot(a, 1, n);
    await child;
    a
}
