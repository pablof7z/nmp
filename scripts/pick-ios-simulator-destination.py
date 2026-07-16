#!/usr/bin/env python3
"""Pick a concrete iOS Simulator device for `xcodebuild test -destination
"id=<udid>"` (issue #465's CI gate).

Reads `xcrun simctl list devices available --json` from stdin, selects the
highest-versioned installed iOS runtime, then the alphabetically first
available iPhone device under that runtime. Prints the chosen UDID to
stdout and a human-readable `::notice::` line (GitHub Actions annotation
syntax) to stderr so the exact pinned destination is visible in the run log
even though it is resolved dynamically rather than hardcoded -- CI images
change their bundled Xcode/Simulator runtimes over time, and a hardcoded
`OS=` version would eventually stop existing on the runner.

Usage: xcrun simctl list devices available --json | python3 pick-ios-simulator-destination.py
"""

import json
import re
import sys


def main() -> int:
    data = json.load(sys.stdin)

    best_runtime = None
    best_version = None
    for runtime_key in data["devices"]:
        match = re.search(r"iOS-(\d+)-(\d+)$", runtime_key)
        if not match:
            continue
        version = (int(match.group(1)), int(match.group(2)))
        if best_version is None or version > best_version:
            best_version = version
            best_runtime = runtime_key

    if best_runtime is None:
        print("no iOS Simulator runtime found on this runner", file=sys.stderr)
        return 1

    candidates = [
        device
        for device in data["devices"][best_runtime]
        if device.get("isAvailable") and "iPhone" in device.get("name", "")
    ]
    if not candidates:
        print(f"no available iPhone device for runtime {best_runtime}", file=sys.stderr)
        return 1

    candidates.sort(key=lambda device: device["name"])
    chosen = candidates[0]

    print(chosen["udid"])
    print(
        f'::notice::pinned iOS Simulator destination: {chosen["name"]} ({best_runtime})',
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
