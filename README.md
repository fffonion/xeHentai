# xeHentai (Rust / RustScript rewrite)

This repository is the Rust + RustScript rewrite of
[fffonion/xeHentai](https://github.com/fffonion/xeHentai). The Python release
remains the behavioural reference.

The application is a single executable. It downloads galleries, keeps a
session and history, serves the original JSON-RPC API and WebUI, and
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
  exposes the 13 original JSON-RPC methods at `/jsonrpc`, the original WebUI at
  `/ui/`, and signed `/img/` and `/zip/` gallery routes. Thumbnails are
  generated natively. `/ui/` and `/static/` are not read from local files:
  each request is proxied to the fixed public origin
  (`http://xehentai.yooooo.us`), and the answers are kept in a local
  `webui.gz` cache with the original one-hour TTL and `If-Modified-Since`
  revalidation. If the origin is unreachable and the cache has no usable
  entry, the original invalid-request JSON is returned. This repository does
  not track the built WebUI, and a release package does not ship it. With
  `rpc_secret` set, clients pass `token:<secret>` as the first positional
  parameter, exactly like the Python reference. `--rpc-open-browser` opens the
  WebUI after the listener is ready.
- **RSS business logic.** `rss/rpc/handlers.rss` selects handlers and
  config/task/session/login effects, `rss/filters/match.rss` holds the page
  filters, and `rss/updater/{github.rss,updater.rss}` hold the update
  check/download decisions. The native layer supplies the system primitives.
- **Updates.** `--auto-update` accepts `check`, `download` (the default) or
  `off`. An explicit `off` suppresses the updater entirely; this is the mode to
  use for offline or unattended runs.
- **What an update may replace.** Only the four RSS scripts this host consumes
  (`rss/filters/match.rss`, `rss/rpc/handlers.rss`, `rss/updater/github.rss`,
  `rss/updater/updater.rss`). The signed v2 descriptor must list exactly those
  four install-relative paths; an executable, native library, UI asset, locale,
  configuration, session or log entry is rejected, and so is an incomplete list.
  The host executable never changes and no helper, preflight or replacement
  process is started.
- **When it lands.** A `download` publishes one pending envelope and the running
  App, tasks and scripts stay on the version that started. The next ordinary
  start validates the envelope, materializes the candidate into private
  staging, compiles and binds all four scripts through their existing module
  entry points in the same process, and only then replaces the four files. A
  preparation failure discards the candidate and keeps the old scripts and the
  user session. A real I/O failure after the first replacement is reported and
  stops that start before the business App; there is no rollback, backup or
  journal. Signing is fail-closed: this repository ships no production
  verification key, so downloaded packages are rejected until the release stage
  binds it.
- **Where packages come from.** Commit metadata is requested from the API origin
  only. A same-repository GitHub Release asset redirects from the package origin
  to `https://release-assets.githubusercontent.com`, which the source binding
  authorizes as the one CDN origin — only as a redirect target of a package
  request, never for commit metadata, never as an initial URL, and every hop
  still checks the complete scheme/host/port tuple and the real public address.

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

`tools/package_dist.py` assembles a self-contained release package around an
**already built** executable. It is standard-library Python and does not run
Cargo, sign, or publish anything. Unix targets get `tar.gz`; Windows gets
`zip`:

```shell
cargo build --release
python3 tools/package_dist.py \
  --binary target/release/xeHentai \
  --output /path/to/xehentai-linux-x86_64
python3 tools/package_dist.py \
  --binary target/release/xeHentai.exe \
  --output /path/to/xehentai-windows-x86_64 \
  --format zip
```

The output directory must not exist; the packager creates it exclusively and
also writes the matching archive next to it. A failure removes the partial
output instead of leaving a mixed tree. The package contains:

```
xeHentai
rss/rpc/handlers.rss
rss/filters/match.rss
rss/updater/github.rss
rss/updater/updater.rss
locales/**
config.example.rss
LICENSE.txt
```

There is no `ui/` entry: the WebUI is proxied from its fixed upstream origin at
run time and cached locally, so nothing about the built pack is shipped. It
never copies `config.rss`, `config.py`, session files, logs, keys, `target/`,
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
python3 tools/browser/test_verify_webui_49.py
python3 tools/browser/test_run.py
```

`tools/compat.py` runs the fixed Python reference in isolated roots and compares
its real output with the Rust entry points. It is a development tool and is not
shipped. `XEH_UPSTREAM` must point at the pinned Python reference checkout;
`tools/test_compat_gallery.py` needs the same checkout and Pillow.
`tools/cases.json` is the feature inventory and per-case status.

## Browser verification

`tools/browser/` runs the fixed original WebUI build against the real
`tests::browser_fixture_server` and checks the exact 49 browser checks with an
independent verifier. The backend serves that local build, seeds fixture
tasks through the existing comparison seam and runs no downloader task loop.

Required inputs:

- a fresh run root inside an owned temporary directory (it must not exist);
- the built WebUI at `ui/` inside the worktree: build
  `fffonion/xeHentai-webui` at `4e3738ef463fef344e96df613de9a81b4cda4b4e`
  (`npm install`, `npm run build`) and copy its `dist/` to `ui/`. The directory
  is git-ignored, is never committed and is never packaged; CI materializes it
  from that pinned revision before the unit tests;
- Chromium for Playwright: pass `--exe` or set `XEH_CHROMIUM`, otherwise the
  executable is resolved from the installed Playwright package;
- a Python interpreter with Pillow and Playwright: pass `--python` or set
  `XEH_PYTHON`;
- the Cargo target from `CARGO_TARGET_DIR`, or an explicitly matching
  `--target-dir`.

```shell
python tools/browser/run.py \
  --run-root /fresh/browser-run-$(date +%s) \
  --temp-root /path/to/owned/temp \
  --worktree . \
  --port 8010 \
  --exe /path/to/chromium
```

The run prepares the fixture, starts the ignored Rust test, waits for
readiness and health, drives the original UI, runs the independent verifier,
then shuts the backend down through its sentinel and confirms the owned
process group is gone. Reports, screenshots, logs and `receipt.json` stay
under the fresh run root and are not committed.

The verifier and its tests can also run alone:

```shell
python tools/browser/verify_webui_49.py /fresh/browser-run/browser-report.json --port 8010
python tools/browser/test_verify_webui_49.py
python tools/browser/test_run.py
```

`tools/browser/fixtures/browser-report-accepted-historical-3c28436.json` is a
historical positive control for the verifier, not current evidence. For the
differential `webui` suite, set `XEH_WEBUI_CHECKOUT` to an independent build of
the original WebUI at `4e3738ef463fef344e96df613de9a81b4cda4b4e` and
`XEH_BROWSER_REPORT` to the report from a browser run.
`tools/browser/compare_webui_pack.py` byte-compares two such local builds,
excluding source maps and `PROVENANCE`; it is a development tool and CI no
longer needs it, because the tracked `ui/` it used to compare against is gone.

The dependency `pd-vm` is pinned to the RustScript revision
`cdb0b798d631cd7ffab6dfed3fe4b69381ed3340`.

## CI

`.github/workflows/ci.yml` runs on GitHub Actions:

- **Linux**: formatting, strict Clippy, a materialization of the fixed upstream
  WebUI build into the ignored `ui/` directory, the unit tests, the Python tool
  tests with the pinned oracle and Pillow, a release build, packaging and a
  package RPC smoke test, a real browser run of that UI with the independent
  49-check verifier, and one differential run of all suites (two internal
  rounds) followed by `--verify-all` bound to the checked-out HEAD.
- **macOS / Windows**: native consumer execution; these jobs run for a tag.

Branch and pull-request CI does no release publishing and uses no repository
secrets. `.github/workflows/release.yml` runs on a tag push (or an explicit
dispatch for an existing tag): it builds the release executable, packages one
archive per platform with `tools/package_dist.py`, smoke-starts the unpacked
package, and publishes the three assets on the GitHub release for that tag.

## Platform support

| Area | Linux | Windows | macOS |
| --- | --- | --- | --- |
| Build | supported | compile-checked in CI | compile-checked in CI |
| Runtime / CLI | supported and tested | not verified | not verified |
| Confined gallery, ZIP and task-root reads | supported | unsupported (refused explicitly) | unsupported (refused explicitly) |
| `--daemon` | fork implementation | unsupported (reported) | Unix code path, not verified |
| RSS-only update application (four scripts, no exe) | supported and tested | not verified | not verified |

The confined-read implementation uses Linux directory handles. On other
platforms those reads are refused rather than performed with weaker checks, so
gallery download/archive behaviour is currently Linux-only. See the table
before relying on any non-Linux build.

## Layout

- `src/` — Rust host code (`main.rs`, `cli.rs`, `config.rs`, `const.rs`,
  `i18n.rs`, `util/`, `worker.rs`, `rpc/`, `filters/`, `updater/`, `task/`).
- `rss/` — RustScript sources (kept separate from Rust).
- `locales/` — translated YAML resources (`en_us`, `zh_hans`, `zh_hant`).
- `ui/` — not tracked. CI materializes the fixed upstream WebUI build here for
  the browser proofs; at run time the server proxies `/ui/` and `/static/`
  instead of reading it.
- `tools/` — comparison harness, feature inventory, tool tests and the release
  packager.
- `config.example.rss` — example user configuration.

## License

GPLv3, same as the original project. See `LICENSE.txt` and the original
copyright notices.
