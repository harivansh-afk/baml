class InventoryBatch {
    stock int[]
    changes int[]
    reorder_below int
}

class InventoryReport {
    stock int[]
    total int
    low_stock int
}

class InvalidBatch {
    message string
}
