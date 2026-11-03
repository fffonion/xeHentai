"""Prepare a fresh browser-evidence backend fixture (owned TEMP only).

Portable copy of the accepted browser3c prepare step. The fixture geometry,
backend task seeds and the synthetic secret are unchanged; only the hardcoded
roots are now explicit arguments:

  --run-root   fresh, exclusive run directory (must not exist yet)
  --worktree   the candidate worktree (UI pack + rss resources)
  --port       real backend listener port (default 8010)
  --temp-root  owned TEMP group the run root must stay inside

Everything (backend root, ready file, shutdown sentinel, request) is created
under the run root. An existing run root, backend root or sentinel is refused
instead of deleted, so old evidence is never reused or overwritten.
"""
import argparse
import json
import sys
from pathlib import Path

from PIL import Image

SECRET = "s3cret"


def assert_inside(path, parent, label):
    assert path == parent or parent in path.parents, f"{label} escapes {parent}: {path}"


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--run-root", required=True, type=Path,
                        help="fresh exclusive run directory (must not exist)")
    parser.add_argument("--worktree", required=True, type=Path,
                        help="candidate worktree providing ui/ and rss/")
    parser.add_argument("--temp-root", required=True, type=Path,
                        help="owned TEMP group the run root must stay inside")
    parser.add_argument("--port", type=int, default=8010,
                        help="real backend listener port (default 8010)")
    return parser.parse_args()


def image(downloads, fmt, size, color, name):
    path = downloads / name
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.new("RGB", size, color).save(path, format=fmt)
    return path


def main():
    args = parse_args()
    run_root = Path(args.run_root).resolve()
    worktree = Path(args.worktree).resolve()
    temp_root = Path(args.temp_root).resolve()
    port = args.port

    assert_inside(run_root, temp_root, "run root")
    assert run_root != temp_root, "run root must be a child of the TEMP group"
    assert 1 <= port <= 65535, f"port out of range: {port}"
    assert worktree.is_dir(), f"worktree missing: {worktree}"
    assert (worktree / "ui" / "index.html").is_file(), "worktree UI pack missing"
    assert (worktree / "rss" / "rpc" / "handlers.rss").is_file(), "worktree rss missing"

    root = (run_root / "backend").resolve()
    ready = (run_root / "ready.json").resolve()
    shutdown = (run_root / "shutdown").resolve()
    request_path = (run_root / "request.json").resolve()
    assert_inside(root, run_root, "backend root")
    assert_inside(ready, run_root, "ready")
    assert_inside(shutdown, run_root, "shutdown")

    # Fresh-exclusive: refuse any pre-existing run root or evidence file.
    for stale, label in ((run_root, "run root"), (root, "backend root"),
                         (ready, "ready file"), (shutdown, "shutdown sentinel"),
                         (request_path, "request file")):
        assert not stale.exists(), f"refusing to reuse existing {label}: {stale}"

    run_root.mkdir(parents=True)
    downloads = root / "downloads"
    downloads.mkdir(parents=True)

    alpha = "alpha"
    specs = [
        ("JPEG", (64, 48), (200, 30, 30), "01.jpg"),
        ("PNG", (48, 64), (30, 160, 60), "02.png"),
        ("GIF", (40, 40), (40, 60, 200), "03.gif"),
        ("WEBP", (56, 42), (180, 140, 20), "04.webp"),
        ("BMP", (44, 52), (120, 30, 160), "05.bmp"),
        ("JPEG", (72, 54), (10, 120, 160), "06.jpg"),
        ("PNG", (30, 30), (250, 250, 30), "07.png"),
        ("JPEG", (80, 60), (90, 90, 90), "08.jpg"),
        ("GIF", (36, 64), (200, 80, 160), "09.gif"),
        ("WEBP", (60, 60), (20, 20, 20), "10.webp"),
        ("BMP", (50, 40), (240, 120, 40), "11.bmp"),
        ("JPEG", (64, 64), (60, 200, 200), "12.jpg"),
    ]
    for fmt, size, color, name in specs:
        image(downloads, fmt, size, color, f"{alpha}/{name}")
    image(downloads, "PNG", (42, 42), (255, 0, 128), f"{alpha}/extra.png")

    beta = "beta"
    for fmt, size, color, name in [
        ("JPEG", (64, 48), (30, 200, 30), "1.jpg"),
        ("PNG", (48, 48), (200, 30, 200), "2.png"),
        ("GIF", (40, 40), (30, 30, 200), "3.gif"),
        ("WEBP", (40, 56), (200, 200, 30), "4.webp"),
    ]:
        image(downloads, fmt, size, color, f"{beta}/{name}")

    gamma = "gamma"
    image(downloads, "PNG", (32, 32), (128, 128, 128), f"{gamma}/01.png")

    tasks = [
        {
            "guid": "alpha0001",
            "url": "http://e-hentai.org/g/9001/abcdef0001/",
            "state": 10,
            "meta": {
                "title": alpha,
                "gnname": "Alpha Gallery",
                "gjname": "アルファギャラリー",
                "tags": ["artist:probe", "language:english", "female:sole"],
                "total": 13,
                "finished": 4,
            },
            "renamed": {
                "2": "02.png",
                "3": "03.gif",
                "4": "04.webp",
                "5": "05.bmp",
                "13": "extra.png",
            },
        },
        {
            "guid": "beta0002",
            "url": "http://e-hentai.org/g/9002/abcdef0002/",
            "state": 20,
            "meta": {
                "title": beta,
                "gnname": "Beta Finished",
                "gjname": "ベータ完結",
                "tags": ["artist:probe", "language:japanese", "full color"],
                "total": 4,
                "finished": 4,
            },
            "renamed": {"2": "2.png", "3": "3.gif", "4": "4.webp"},
        },
        {
            "guid": "gamma003",
            "url": "http://e-hentai.org/g/9003/abcdef0003/",
            "state": 0,
            "meta": {
                "title": gamma,
                "gnname": "Gamma Paused",
                "gjname": "ガンマ停止",
                "tags": ["artist:other", "language:chinese"],
                "total": 1,
                "finished": 0,
            },
            "renamed": {"1": "01.png"},
        },
    ]
    request = {
        "root": str(root),
        "resource_root": str(worktree),
        "port": port,
        "secret": SECRET,
        "ready": str(ready),
        "shutdown": str(shutdown),
        "tasks": tasks,
    }
    request_path.write_text(json.dumps(request, indent=1), encoding="utf-8")
    print(json.dumps({
        "prepared": str(run_root),
        "backend_root": str(root),
        "port": port,
        "files": sum(1 for _ in downloads.rglob("*") if _.is_file()),
        "tasks": [t["guid"] for t in tasks],
    }, indent=1))


if __name__ == "__main__":
    sys.exit(main())
