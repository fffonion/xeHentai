# Configuration

The application reads one `config.json` file. The file is a plain JSON object;
there is no Python source, expression evaluation, `config.py` import, or
executable `config.rss`. The runtime uses the pinned RustScript Core `json`
decoder and `io` builtins.

## File selection

1. An explicit path supplied by the entry is used as-is.
2. Otherwise `config.json` under the working directory is used.
3. A missing implicit `config.json` means "use built-in defaults".
4. A missing explicit path, invalid JSON, a non-object root, an unknown key, or
   a wrong field type is an error. A bad file is never silently replaced with
   defaults.

Paths are passed from the entry. The loader does not rewrite relative paths to
absolute ones; that belongs to the CLI layer.

## Keys and types

All 25 keys are optional. Missing keys keep the built-in default. Explicit
`false`, `0`, `""`, `[]`, and accepted nulls replace the default; the loader
never uses truthiness to skip a supplied value.

| Key | Type | Default |
|---|---|---|
| `daemon` | bool | `false` |
| `dir` | string | `"."` |
| `download_ori` | bool | `false` |
| `jpn_title` | bool | `true` |
| `rename_ori` | bool | `false` |
| `proxy` | array of strings | `[]` |
| `proxy_image` | bool | `true` |
| `proxy_image_only` | bool | `false` |
| `rpc_interface` | string | `"localhost"` |
| `rpc_port` | int or null | `null` |
| `rpc_secret` | string or null | `null` |
| `rpc_open_browser` | bool | `true` |
| `make_archive` | bool | `false` |
| `download_range` | array of `[start, end]` int pairs, or null | `null` |
| `scan_thread_cnt` | int | `1` |
| `download_thread_cnt` | int | `5` |
| `download_timeout` | number (int or float) | `10` |
| `ignored_errors` | array of ints (negatives allowed) | `[]` |
| `log_path` | string | `"eh.log"` |
| `log_verbose` | int | `2` |
| `save_tasks` | bool | `false` |
| `delete_task_files` | bool | `false` |
| `low_speed_threshold` | number (int or float) | `10` |
| `auto_update` | string | `"download"` |
| `update_beta_channel` | bool | `false` |

`download_range` pairs must contain exactly two integers. `ignored_errors`
accepts any integer, including negative values.

A minimal example is `config.example.json`.

## Overlays

1. Built-in defaults.
2. The user file above.
3. CLI / interactive overlay (this layer provides the overlay operation; argv
   parsing is a later task).

A runtime update applies only the keys present in its overlay document. Key
presence is tracked for the effects that depend on it: `proxy`, `dir`, and
`ignored_errors`.

`ignored_errors` has two distinct rules:

- A user file replaces the list exactly, including duplicates and order.
- A runtime update with a present, non-empty list merges it with the current
  list, keeping the first occurrence order. A present empty list leaves the
  current list unchanged, matching the upstream `if cfg_dict['ignored_errors']`
  behavior.

## Views

- `getConfig` hides the four `rpc_*` keys and `urls`. The fixed record has no
  `urls` key, so the public view has 21 keys.
- A task snapshot copies the 14 keys used by `add_task` when the task is
  created. Later global updates do not change that snapshot.

## Errors

The loader returns an `ok` flag, an `error` code, and a report. Error codes are
stable strings:

- `missing_file` — explicit path does not exist.
- `missing_overlay` — update overlay does not exist.
- `unknown_key:<key>` — key is not one of the 25 known keys.
- `type:<key>` — field does not match its fixed type.
- Invalid JSON and non-object roots surface as Core `json_decode`/map type
  errors from the decode boundary.

`crates/xeh-host/tests/config.rs` exercises the full path through the real
RustScript entry, including defaults, file overlays, partial documents, error
rejection, runtime updates, snapshots, and two independent fixture roots.

## Commands

```bash
cargo test -p xeh-host --test config
XEH_LEASE_TMP=<lease tmp> cargo run -p xeh-xtask -- rss-test --suite config
XEH_LEASE_TMP=<lease tmp> XEH_PYTHON=<python with requests[socks]> \
  cargo run -p xeh-xtask -- diff --suite config
```

`diff --suite config` runs the pinned upstream implementation as a test oracle
with the same JSON data and compares values field by field. Known differences
are listed in `docs/compatibility-deltas.md` and printed as `delta=` lines.
