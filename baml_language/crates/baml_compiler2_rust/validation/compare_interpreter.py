"""Compare two built runtime_benchmark executables; retain every trial output.

Run with uv run --no-project. Validate workload results with
baml_tests/examples/verify_runtime_result.rs before collecting timings.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import statistics
import subprocess


CASES = [
    "array_build_sum_100k",
    "call_chain_100x10k",
    "closure_apply_1m",
    "nested_loops_500x500",
    "pure_call_1m",
]


def nanoseconds(value):
    match = re.fullmatch(r"\s*([\d.]+)\s*(ns|µs|us|ms|s)\s*", value)
    if not match:
        raise ValueError(f"unrecognized Divan duration: {value!r}")
    return float(match[1]) * {"ns": 1, "µs": 1000, "us": 1000, "ms": 1e6, "s": 1e9}[match[2]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", type=Path, required=True)
    parser.add_argument("--head", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--cpus", default="15,16")
    parser.add_argument("--trials", type=int, default=5)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output must be a new file")
    binaries = {"base": args.base.resolve(), "head": args.head.resolve()}
    data = {
        "shared_host": True,
        "cpus": args.cpus,
        "telemetry": "off",
        "tokio_workers": 2,
        "binary_sha256": {key: hashlib.sha256(path.read_bytes()).hexdigest()
                          for key, path in binaries.items()},
        "divan_arguments": ["--bench", "--color", "never", "--timer", "os",
                            "--sample-count", "16", "--sample-size", "1", "--max-time", "2"],
        "trials": [],
    }
    plan = [(case, revision, trial) for trial in range(args.trials)
            for case in CASES for revision in binaries]
    random.Random(20261007).shuffle(plan)
    environment = dict(os.environ, BAML_TELEMETRY="off", TOKIO_WORKER_THREADS="2")
    for case, revision, trial in plan:
        benchmark = f"vm_speedtest_compute_{case}"
        command = ["taskset", "-c", args.cpus, str(binaries[revision]),
                   *data["divan_arguments"], benchmark]
        result = subprocess.run(command, env=environment, text=True,
                                capture_output=True, check=True, timeout=120)
        rows = [line for line in result.stdout.splitlines() if benchmark in line and "│" in line]
        if len(rows) != 1:
            raise ValueError(f"missing/ambiguous benchmark result: {result.stdout}")
        columns = rows[0].split("│")
        record = {"case": case, "revision": revision, "trial": trial,
                  "median_ns": nanoseconds(columns[2]), "mean_ns": nanoseconds(columns[3]),
                  "samples": int(columns[4]), "iterations": int(columns[5]),
                  "stdout": result.stdout, "stderr": result.stderr}
        data["trials"].append(record)
        args.output.write_text(json.dumps(data, indent=2) + "\n")
        print(case, revision, trial, record["median_ns"], flush=True)
    data["summary"] = []
    for case in CASES:
        values = {revision: [r["median_ns"] for r in data["trials"]
                             if r["case"] == case and r["revision"] == revision]
                  for revision in binaries}
        base, head = (statistics.median(values[key]) for key in ("base", "head"))
        data["summary"].append({"case": case, "base_ns": base, "head_ns": head,
                                "change_percent": 100 * (head / base - 1),
                                "trial_median_ranges_ns": {key: [min(v), max(v)] for key, v in values.items()}})
    args.output.write_text(json.dumps(data, indent=2) + "\n")


if __name__ == "__main__":
    main()
