#!/usr/bin/env python3
"""Byte-compare a rebuilt original WebUI pack with the frozen candidate pack.

Both sides exclude source maps; the candidate also excludes its PROVENANCE
notice, which is pack metadata rather than a served asset. The comparison is
whole-file byte equality with no hashes or digests.

Usage:
  python tools/browser/compare_webui_pack.py --dist <built/dist> \
      --candidate <repo/ui> [--expected-count 7]
"""
import argparse
import sys
from pathlib import Path


def servable_files(root, skip=()):
    return {
        path.relative_to(root).as_posix(): path
        for path in sorted(root.rglob("*"))
        if path.is_file()
        and path.suffix != ".map"
        and path.relative_to(root).as_posix() not in skip
    }


def compare(dist, candidate, expected_count):
    built = servable_files(dist)
    frozen = servable_files(candidate, skip=("PROVENANCE",))
    problems = []
    if len(built) != expected_count:
        problems.append(f"built pack has {len(built)} servable files, expected {expected_count}")
    missing = sorted(set(frozen) - set(built))
    extra = sorted(set(built) - set(frozen))
    if missing:
        problems.append(f"built pack is missing: {missing}")
    if extra:
        problems.append(f"built pack has extra: {extra}")
    for name in sorted(set(built) & set(frozen)):
        built_bytes = built[name].read_bytes()
        frozen_bytes = frozen[name].read_bytes()
        if built_bytes != frozen_bytes:
            problems.append(f"{name}: bytes differ ({len(built_bytes)} != {len(frozen_bytes)})")
        else:
            print(f"equal {name} {len(built_bytes)} bytes")
    return problems


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--dist", required=True, type=Path)
    parser.add_argument("--candidate", required=True, type=Path)
    parser.add_argument("--expected-count", type=int, default=7)
    args = parser.parse_args(argv)
    for root, label in ((args.dist, "dist"), (args.candidate, "candidate")):
        if not root.is_dir():
            print(f"pack error: {label} is not a directory: {root}", file=sys.stderr)
            return 2
    problems = compare(args.dist, args.candidate, args.expected_count)
    if problems:
        print("WEBUI-PACK REJECT")
        for problem in problems:
            print("-", problem)
        return 1
    print(f"WEBUI-PACK ACCEPT {args.expected_count} files byte-equal")
    return 0


if __name__ == "__main__":
    sys.exit(main())
