class State { values: int[] }
function prepare(raw: string) -> State { baml.json.deserialize<State>(raw) }

// Identical kernel to 12_quick_sort_native.
function sort_kernel(a: int[], lows: int[], highs: int[]) -> int {
    let top = 1;
    lows[0] = 0; highs[0] = a.length() - 1;
    while (top > 0) {
        top -= 1; let lo = lows[top]; let hi = highs[top];
        if (lo < hi) {
            let pivot = a[lo + (hi - lo) / 2];
            let lt = lo; let i = lo; let gt = hi;
            while (i <= gt) {
                if (a[i] < pivot) { let v = a[lt]; a[lt] = a[i]; a[i] = v; lt += 1; i += 1; }
                else if (a[i] > pivot) { let v = a[gt]; a[gt] = a[i]; a[i] = v; gt -= 1; }
                else { i += 1; }
            }
            if (lt - lo > hi - gt) {
                lows[top] = lo; highs[top] = lt - 1; top += 1;
                lows[top] = gt + 1; highs[top] = hi; top += 1;
            } else {
                lows[top] = gt + 1; highs[top] = hi; top += 1;
                lows[top] = lo; highs[top] = lt - 1; top += 1;
            }
        }
    }
    a.length()
}

// Indexed restore + sort, repeated; everything here is inside the native subset.
function bench(src: int[], a: int[], lows: int[], highs: int[], reps: int) -> int {
    let check = 0;
    for (let r = 0; r < reps; r += 1) {
        for (let i = 0; i < src.length(); i += 1) { a[i] = src[i]; }
        sort_kernel(a, lows, highs);
        check += a[0] + a[a.length() - 1];
    }
    check
}

// Setup is bytecode, outside the hot loop: 16 sorts of the same 1024 ints per job.
function run(state: State) -> string {
    let src: int[] = []; let a: int[] = [];
    for (let v in state.values) { src.push(v); a.push(v); }
    let lows: int[] = []; let highs: int[] = [];
    for (let i = 0; i < 64; i += 1) { lows.push(0); highs.push(0); }
    let check = 0;
    if (a.length() > 1) { check = bench(src, a, lows, highs, 16); }
    baml.json.to_string(check)
}
