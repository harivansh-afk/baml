function apply_changes(stock: int[], changes: int[]) -> int[] {
    for (let i = 0; i < stock.length(); i += 1) {
        stock[i] += changes[i];
        if (stock[i] < 0) { stock[i] = 0; }
    }
    stock
}

function summarize(stock: int[], reorder_below: int) -> int[] {
    let total = 0;
    let low = 0;
    for (let i = 0; i < stock.length(); i += 1) {
        total += stock[i];
        if (stock[i] < reorder_below) { low += 1; }
    }
    [total, low]
}
