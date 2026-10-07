class State { values: int[] }
function prepare(raw: string) -> State { baml.json.deserialize<State>(raw) }

// Scalar/int[] kernel: inside PR #1's native subset. Smaller partition is
// processed first, so the explicit stack never exceeds log2(n) + 1 entries.
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

// Same job boundary as 10_quick_sort: copy, stack allocation, sort, serialize.
function run(state: State) -> string {
    let a: int[] = [];
    for (let v in state.values) { a.push(v); }
    let lows: int[] = []; let highs: int[] = [];
    for (let i = 0; i < 64; i += 1) { lows.push(0); highs.push(0); }
    if (a.length() > 1) { sort_kernel(a, lows, highs); }
    baml.json.to_string(a)
}
