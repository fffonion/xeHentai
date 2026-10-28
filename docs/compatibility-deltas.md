# 兼容差异

本文记录固定上游 `fffonion/xeHentai` `43fc55c0c662f195d048becf6276a26ce06b09d9` 在隔离夹具中的实测结果。
夹具数据均为合成值。应用重写尚未实现，下列条目不构成迁移合同。

项目状态：兼容迁移中。

## delta.token-placement

- 来源：`xeHentai/rpc.py:363`
- 状态：oracle_characterized
- 输入：`{"placement": "params[0][0] must contain token:<secret>"}`
- 实测输出：`{"prefixed": "HTTP/1.0 200 OK", "embedded": "HTTP/1.0 200 OK", "bare": "HTTP/1.0 403 Forbidden", "missing": "HTTP/1.0 403 Forbidden"}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.update-config-proxy

- 来源：`xeHentai/core.py:71`
- 状态：oracle_characterized
- 输入：`{"call": "update_config()"}`
- 实测输出：`{"exception": "KeyError"}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.get-image-range

- 来源：`xeHentai/rpc.py:488`
- 状态：oracle_characterized
- 输入：`{"request_range": "1,3"}`
- 实测输出：`{"status_line": "HTTP/1.0 200 OK", "json": {"id": "img", "error": null, "result": ["/img/14d5f1a7/g1/1/1.jpg"]}}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.falsy-id

- 来源：`xeHentai/rpc.py:109`
- 状态：oracle_characterized
- 输入：`{"id": 0}`
- 实测输出：`{"status_line": "HTTP/1.0 200 OK", "json": {"id": null, "error": null, "result_type": "dict"}}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.token-regex-log

- 来源：`xeHentai/rpc.py:61`
- 状态：oracle_characterized
- 输入：`{"token": "xxxtoken:<synthetic>TAIL", "log": "RPC started URL"}`
- 实测输出：`{"embedded": {"status_line": "HTTP/1.0 200 OK", "json": {"id": "tok", "error": null, "result_type": "dict"}}, "log_contains_token_marker": true}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.argparse-bool

- 来源：`xeHentai/cli.py`
- 状态：oracle_characterized
- 输入：`{"argv": ["--jpn-title", "False", "--rename-ori", "0"], "user_config": {"jpn_title": false}}`
- 实测输出：`{"exit": 0, "stdout": "{\"exit\": 0, \"jpn_title\": true, \"rename_ori\": true, \"download_range\": null, \"daemon\": true, \"username\": null}"}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.download-range-default

- 来源：`xeHentai/cli.py:183`
- 状态：oracle_characterized
- 输入：`{"user_config_download_range": [[9, 9]], "argv": []}`
- 实测输出：`{"exit": 0, "stdout": "{\"exit\": 0, \"jpn_title\": false, \"rename_ori\": false, \"download_range\": null, \"daemon\": true, \"username\": null}"}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.proxy-availability

- 来源：`xeHentai/proxy.py:46`
- 状态：oracle_characterized
- 输入：`{"calls": ["empty", "one http proxy"]}`
- 实测输出：`{"empty_pool": true, "one_proxy": false}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.rename-key-types

- 来源：`xeHentai/task.py:434`
- 状态：oracle_characterized
- 输入：`{"renamed_map_key": 1, "duplicate_map_value": "set"}`
- 实测输出：`{"exception": "TypeError: Object of type set is not JSON serializable", "ok": false}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.scan-downloaded-comparison

- 来源：`xeHentai/task.py:206`
- 状态：oracle_characterized
- 输入：`{"files": [1, 2, 3], "zero_byte": 2}`
- 实测输出：`{"state_before": 10, "state_after": 10, "finished": 2, "done": [1, 3], "assigned": false, "zero_removed": true}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.unfinished-end

- 来源：`xeHentai/task.py:299`
- 状态：oracle_characterized
- 输入：`{"total": 3, "done": [1, 3]}`
- 实测输出：`{"ids": [2], "total": 3}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.get-extra-newline

- 来源：`xeHentai/rpc.py:319`
- 状态：oracle_characterized
- 输入：`{"requests": ["OPTIONS", "GET /nope", "GET /jsonrpc"]}`
- 实测输出：`{"options": {"status_line": "HTTP/1.0 200 OK"}, "unmatched": {"status_line": "HTTP/1.0 404 Not Found"}, "get_jsonrpc": {"status_line": "HTTP/1.0 200 OK"}}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.malformed-json

- 来源：`xeHentai/rpc.py:335`
- 状态：oracle_characterized
- 输入：`{"body": "{"}`
- 实测输出：`{"parsed": {"status_line": "HTTP/1.0 400 Bad Request"}, "client_error": null}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.content-length

- 来源：`xeHentai/rpc.py:391`
- 状态：oracle_characterized
- 输入：`{"malformed": "{"}`
- 实测输出：`{"status_line": "HTTP/1.0 400 Bad Request"}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

## delta.xehdone-zip-recovery

- 来源：`xeHentai/task.py:176`
- 状态：oracle_characterized
- 输入：`{"xehdone": true, "zip_exists": true, "make_archive_remove": false}`
- 实测输出：`{"xehdone_finished_count": 2, "xehdone_state": 10, "zip_finished_count": 1, "archive": {"path": "archive.zip", "compress_type": 0, "store": true, "comment": "xeHentai Archiver v2.023\nTitle:archive\nOriginal URL:https://e-hentai.org/g/1/aaaaaaaaaa/", "directory_remains": true}}`
- 原始记录：`compat/golden/` 中对应文件。
- 拟定合同：待批准。应用侧不把该输出当作已实现行为。

