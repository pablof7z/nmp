#!/usr/bin/env python3
"""Print an available iPhone UDID from the newest installed iOS runtime."""

import json
import re
import sys


def main() -> int:
    data = json.load(sys.stdin)
    runtimes = []
    for runtime, devices in data["devices"].items():
        match = re.search(r"iOS-(\d+)-(\d+)$", runtime)
        if match:
            runtimes.append(((int(match.group(1)), int(match.group(2))), runtime, devices))
    if not runtimes:
        print("no iOS Simulator runtime found", file=sys.stderr)
        return 1
    _, runtime, devices = max(runtimes)
    candidates = sorted(
        (device for device in devices if device.get("isAvailable") and "iPhone" in device.get("name", "")),
        key=lambda device: device["name"],
    )
    if not candidates:
        print(f"no available iPhone for {runtime}", file=sys.stderr)
        return 1
    chosen = candidates[0]
    print(chosen["udid"])
    print(f"selected {chosen['name']} ({runtime})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
