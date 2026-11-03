# xeHentai (Rust / RustScript rewrite)

This repository is the Rust + RustScript rewrite of
[fffonion/xeHentai](https://github.com/fffonion/xeHentai). The Python release
remains the behavioural reference.

The application is a single executable. It downloads galleries, keeps a
session and history, serves the original JSON-RPC API and frozen WebUI, and
checks/installs application updates. Page matching, RPC handler decisions and
update decisions are written in RustScript under `rss/`; native networking,
configuration, filesystem and process operations live in Rust under `src/`.

## What it does

- **Configuration.** Reads `config.rss` from the working directory. If it does
  not exist, a legacy `config.py` in the same directory is imported **once**
  with a regex-based reader (no Python execution, no AST) and `config.rss` is
  written atomically. After `config.rss` exists, `config.py` is never read
  again. The import subset is `key = value` assignments with integers,
  `True`/`False`, quoted strings and flat arrays (plus
  `download_range = [[start, end], ...]`); unsupported lines are rejected with
  a line number.
- **CLI and lifecycle.** All reference flags are available, including
  `--username`, `--key`, `--cookie`, `--dir`, `--proxy`, `--archive`,
  `--download-range`, `--thread`, `--timeout`, `--force`, `--auto-update`,
  `--logpath`, `--verbose`, `--daemon`, the `--rpc-*` options and `--version`/
  `--help`. A run owns its download tasks, session writer, RPC server and log,
  and retires them together on SIGINT/SIGTERM. `--daemon` forks on Unix; on
  platforms without a daemon implementation the option is reported as
  unsupported and the process exits normally.
- **RPC and WebUI.** With `rpc_port` configured (or `--rpc-port`), the server
  exposes the 13 original JSON-RPC methods at `/jsonrpc`, the original frozen
  WebUI at `/ui/`, and signed `/img/` and `/zip/` gallery routes. Thumbnails
  are generated natively; the frozen static pack is served with the original
  one-hour cache semantics. With `rpc_secret` set, clients pass
  `token:<secret>` as the first positional parameter, exactly like the Python
  reference. `--rpc-open-browser` opens the WebUI after the listener is ready.
- **RSS business logic.** `rss/rpc/handlers.rss` selects handlers and
  config/task/session/login effects, `rss/filters/match.rss` holds the page
  filters, and `rss/updater/{github.rss,updater.rss}` hold the update
  check/download decisions. The native layer supplies the system primitives.
- **Updates.** `--auto-update` accepts `check`, `download` (the default) or
  `off`. An explicit `off` suppresses the updater entirely; this is the mode to
  use for offline or unattended runs. An update package is verified against its
  configured verification key before any file is replaced, and installation is
  fail-closed: this repository ships no production verification key, so
  downloaded packages are rejected rather than installed.

## Running

```shell
# from a directory containing config.rss (or config.py for one-time import)
./xeHentai --auto-update off
./xeHentai --auto-update off --rpc-port 8010 --rpc-secret "$RPC_SECRET"
./xeHentai --daemon --auto-update check
```

Runtime resources are resolved from the directory of the executable when it
contains `rss/`, otherwise from the working directory. A packaged release
therefore starts from any working directory and never reads the build tree.

## Distribution package

`tools/package_dist.py` assembles a self-contained Linux package around an
**already built** executable. It is standard-library Python and does not run
Cargo, sign, or publish anything:

```shell
cargo build --release
python3 tools/package_dist.py \
  --binary target/release/xeHentai \
  --output /path/to/xehentai-linux-x86_64
```

The output directory must not exist; the packager creates it exclusively and
also writes `xehentai-linux-x86_64.tar.gz` next to it. A failure removes the
partial output instead of leaving a mixed tree. The package contains:

```
xeHentai
rss/rpc/handlers.rss
rss/filters/match.rss
rss/updater/github.rss
rss/updater/updater.rss
ui/index.html
ui/static/**
ui/PROVENANCE
locales/**
config.example.rss
LICENSE.txt
```

It never copies `config.rss`, `config.py`, session files, logs, keys, `target/`,
`.git/` or the source tree. `locales/` is compiled into the executable but is
also shipped as a language resource, and `config.example.rss` documents the
configuration fields.

## Build and test

```shell
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
python3 tools/test_cases_inventory.py
python3 tools/test_compat_gallery.py
python3 tools/test_update_compat.py
python3 tools/test_package_dist.py
```

`tools/compat.py` runs the fixed Python reference in isolated roots and compares
its real output with the Rust entry points. It is a development tool and is not
shipped. `XEH_UPSTREAM` must point at the pinned Python reference checkout;
`tools/test_compat_gallery.py` needs the same checkout (and Pillow) and is
therefore not part of the repository-only CI job. `tools/cases.json` is the
feature inventory and per-case status.

The dependency `pd-vm` is pinned to the RustScript revision
`cdb0b798d631cd7ffab6dfed3fe4b69381ed3340`.

## CI

`.github/workflows/ci.yml` runs on GitHub Actions:

- **Linux**: formatting, strict Clippy, the unit tests, the Python tool tests
  that use in-repository fixtures, a release build, packaging and a package RPC
  smoke test.
- **macOS / Windows**: compilation and strict Clippy only. These jobs are not
  runtime or feature-parity verification.

CI has no deployment, release publishing, push or repository secret access.

## Platform support

| Area | Linux | Windows | macOS |
| --- | --- | --- | --- |
| Build | supported | compile-checked in CI | compile-checked in CI |
| Runtime / CLI | supported and tested | not verified | not verified |
| Confined gallery, ZIP and task-root reads | supported | unsupported (refused explicitly) | unsupported (refused explicitly) |
| `--daemon` | fork implementation | unsupported (reported) | Unix code path, not verified |
| Native update replacement / restart | supported | refused explicitly | not verified |

The confined-read implementation uses Linux directory handles. On other
platforms those reads are refused rather than performed with weaker checks, so
gallery download/archive behaviour is currently Linux-only. See the table
before relying on any non-Linux build.

## Layout

- `src/` — Rust host code (`main.rs`, `cli.rs`, `config.rs`, `const.rs`,
  `i18n.rs`, `util/`, `worker.rs`, `rpc/`, `filters/`, `updater/`, `task/`).
- `rss/` — RustScript sources (kept separate from Rust).
- `locales/` — translated YAML resources (`en_us`, `zh_hans`, `zh_hant`).
- `ui/` — frozen original WebUI static pack and its provenance notice.
- `tools/` — comparison harness, feature inventory, tool tests and the release
  packager.
- `config.example.rss` — example user configuration.

## License

GPLv3, same as the original project. See `LICENSE.txt` and the original
copyright notices.
