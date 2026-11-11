#!/usr/bin/env python3
"""Turn Cargo.lock into flatpak-builder sources so the sandboxed build is offline.

Same output layout as flatpak-builder-tools' flatpak-cargo-generator, without its
extra dependencies (crates.io registry crates only; Parley has no git deps).
Usage: flatpak/gen-cargo-sources.py Cargo.lock > flatpak/cargo-sources.json
"""
import json
import sys
import tomllib

lock = tomllib.load(open(sys.argv[1] if len(sys.argv) > 1 else "Cargo.lock", "rb"))
sources = []
for pkg in lock["package"]:
    src = pkg.get("source", "")
    if not src:
        continue  # the workspace itself
    if not src.startswith("registry+"):
        sys.exit(f"unsupported source for {pkg['name']}: {src}")
    name, version, sha = pkg["name"], pkg["version"], pkg["checksum"]
    dest = f"cargo/vendor/{name}-{version}"
    sources.append({
        "type": "archive",
        "archive-type": "tar-gzip",
        "url": f"https://static.crates.io/crates/{name}/{name}-{version}.crate",
        "sha256": sha,
        "dest": dest,
    })
    sources.append({
        "type": "inline",
        "contents": json.dumps({"package": sha, "files": {}}),
        "dest": dest,
        "dest-filename": ".cargo-checksum.json",
    })
sources.append({
    "type": "inline",
    "contents": '[source.vendored-sources]\ndirectory = "cargo/vendor"\n\n'
                '[source.crates-io]\nreplace-with = "vendored-sources"\n',
    "dest": "cargo",
    "dest-filename": "config.toml",
})
json.dump(sources, sys.stdout, indent=1)
print()
