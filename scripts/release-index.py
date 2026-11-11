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
            # Older releases have no FreeBSD assets; newer ones need the whole family.
            if any(asset["name"].startswith(f"carryfs-{version}-freebsd") for asset in release["assets"]):
                required.update(f"carryfs-{version}-{platform}.tar.gz" for platform in (
                    "freebsd14-x86_64", "freebsd14-arm64", "freebsd15-x86_64", "freebsd15-arm64"))
            uploaded = {asset["name"] for asset in release["assets"] if asset["state"] == "uploaded"}
            if required <= uploaded:
                versions.add(tuple(map(int, match.groups())))
    for version in sorted(versions):
        print(".".join(map(str, version)))


if __name__ == "__main__":
    main()
