function leaf(n: int) -> int { n + 1 }
function middle(n: int) -> int { leaf(n) + 1 }
function outer(n: int) -> int { middle(n) + 1 }
function entry(n: int) -> int {
    let total = 0;
    while (total < n) { total = outer(total); }
    total
}
function failing(n: int) -> int { outer(n) / 0 }
function errors(n: int) -> int { failing(n) }
