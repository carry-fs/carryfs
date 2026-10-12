#!/usr/bin/env python3
"""Compare names-only, list-then-stat and interleaved-stat callers.

Each cold comparison needs a fresh mount: running these patterns in sequence
warms name and attribute caches. The driver owns mounts and cache eviction.
"""

import argparse
import json
import os
import time

PATTERNS = ("names", "list-stat", "interleaved")


def measure(directory, pattern):
    """Return timings, names and stat results; validation is outside timing."""
    if pattern not in PATTERNS:
        raise ValueError(f"unknown listing pattern: {pattern}")
    directory = os.fspath(directory)
    attributes = []
    start = time.perf_counter()
    if pattern == "interleaved":
        names = []
        with os.scandir(directory) as entries:
            for entry in entries:
                names.append(entry.name)
                attributes.append(entry.stat(follow_symlinks=False))
        listed = None
    else:
        names = sorted(os.listdir(directory))
        listed = time.perf_counter()
        if pattern == "list-stat":
            attributes = [os.stat(os.path.join(directory, name), follow_symlinks=False)
                          for name in names]
    end = time.perf_counter()
    result = {"pattern": pattern, "entries": len(names), "seconds": end - start,
              "entries_per_second": len(names) / (end - start)}
    if listed is not None:
        result["listing_seconds"] = listed - start
        if pattern == "list-stat":
            result["stat_seconds"] = end - listed
    return result, names, attributes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory")
    parser.add_argument("pattern", choices=PATTERNS)
    args = parser.parse_args()
    result, _, _ = measure(args.directory, args.pattern)
    print(json.dumps(result))


if __name__ == "__main__":
    main()
