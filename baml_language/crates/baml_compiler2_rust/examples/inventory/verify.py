"""Compare the built inventory app and interpreter with a deterministic oracle."""

import argparse
import json
import os
from pathlib import Path
import random
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    project = Path(__file__).resolve().parent
    commands = [
        [str(args.binary.resolve())],
        [str(args.cli.resolve()), "run", "main", "--from", str(project),
         "--output-format", "json", "--"],
    ]
    random_source = random.Random(20261002)
    cases = [json.loads((project / "request.json").read_text())["batch"],
             {"stock": [], "changes": [], "reorder_below": 0}]
    for size in [1, 2, 3, 8, 17, 64, 128, 256]:
        for _ in range(2):
            cases.append({
                "stock": [random_source.randrange(0, 200) for _ in range(size)],
                "changes": [random_source.randrange(-300, 100) for _ in range(size)],
                "reorder_below": random_source.randrange(0, 100),
            })
    errors = [
        ({"stock": [1], "changes": [], "reorder_below": 0},
         "stock and changes must have the same length"),
        ({"stock": [(1 << 62) - 1], "changes": [1], "reorder_below": 0},
         "overflows int"),
    ]
    executions = 0
    for mode in ["off", "high"]:
        environment = dict(os.environ, BAML_TELEMETRY=mode)

        def run(command, batch):
            return subprocess.run(
                [*command, "--json-args", json.dumps({"batch": batch})],
                env=environment, text=True, capture_output=True, timeout=30,
            )

        for batch in cases:
            stock = [max(0, count + change)
                     for count, change in zip(batch["stock"], batch["changes"])]
            expected = {"stock": stock, "total": sum(stock),
                        "low_stock": sum(count < batch["reorder_below"] for count in stock)}
            for command in commands:
                result = run(command, batch)
                assert result.returncode == 0, result.stderr
                assert json.loads(result.stdout) == expected, (mode, batch, result.stdout)
                executions += 1
        for batch, message in errors:
            for command in commands:
                result = run(command, batch)
                assert result.returncode != 0 and message in result.stderr, result
                executions += 1
    print(f"{executions} executions passed: release app and interpreter, telemetry off/high")


if __name__ == "__main__":
    main()
