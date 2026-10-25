#!/usr/bin/env python3
"""Read `gh api --paginate --slurp .../releases` JSON and emit releases.txt."""
import json
import re
import sys


def main():
    versions = set()
    for page in json.load(sys.stdin):
        for release in page:
            if release["draft"] or release["prerelease"]:
                continue
            match = re.fullmatch(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", release["tag_name"])
            if not match:
                continue
            version = match.group(0)[1:]
            required = {"SHA256SUMS", f"carryfs-{version}-windows-x86_64.zip"}
            required.update(f"carryfs-{version}-{platform}.tar.gz" for platform in ("linux-x86_64", "macos-arm64"))
            uploaded = {asset["name"] for asset in release["assets"] if asset["state"] == "uploaded"}
            if required <= uploaded:
                versions.add(tuple(map(int, match.groups())))
    for version in sorted(versions):
        print(".".join(map(str, version)))


if __name__ == "__main__":
    main()
