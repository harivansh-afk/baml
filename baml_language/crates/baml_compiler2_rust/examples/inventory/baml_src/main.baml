function main(batch: InventoryBatch) -> InventoryReport {
    if (batch.stock.length() != batch.changes.length()) {
        throw InvalidBatch { message: "stock and changes must have the same length" };
    }
    let stock = apply_changes(batch.stock, batch.changes);
    let summary = summarize(stock, batch.reorder_below);
    InventoryReport { stock: stock, total: summary[0], low_stock: summary[1] }
}
