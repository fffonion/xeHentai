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


## delta.config-source-format

- 来源：`xeHentai/core.py:29-51`（原版导入 `config.py`）
- 状态：accepted（计划 §2.3 用户修订）
- 输入：同一组配置值，原版写成 Python 模块，新实现写成 `config.json` 对象。
- 实测输出：`diff --suite config` 中两边 25 个字段逐一相同；公开视图 21 个字段、`task_projection` 14 个字段的投影逐一相同。该比较只验证字段投影，不验证任务创建或更新后保留；真实 Task 生命周期是 Task5 首项。
- 拟定合同：只接受纯 JSON 对象；不执行 `config.py`/`config.rss`，不做 AST 求值或 Python 桥。配置源码执行语义明确移出兼容范围。

## delta.config-strict-validation

- 来源：`xeHentai/core.py:49-51`（原版直接合并任意 Python 属性）
- 状态：accepted（计划 §2.3）
- 输入：未知键 `urls`、错误类型（例如 `daemon` 为字符串）、非对象根。
- 实测输出：新实现分别返回 `unknown_key:urls`、`type:daemon`，非对象根在 Core map 解码处失败；原版不会做这些检查。
- 拟定合同：新增的显式拒绝属于有意的安全/可诊断性改进；不改变 RPC 方法自身的开放参数合同。

## delta.ignored-errors-order

- 来源：`xeHentai/core.py:67-68`
- 状态：accepted（计划 §2.3、§4.1）
- 输入：已有 `[1006, -5, 1006]`，更新 `[1009, 1006, -3]`。
- 实测输出：原版 `[1009, -5, -3, 1006]`（CPython set 顺序）；新实现 `[1006, -5, 1009, -3]`（保留首次出现顺序）。
- 原始记录：`diff --suite config` 的 `compare.json` 与原始输出。
- 拟定合同：集合语义一致，顺序差异按确定性策略保留并单列；不模拟 CPython hash/probe/resize，也不排序伪装相等。
- 实现约束：当前 `merge_errors` 是内部 helper，保序去重为 O(n²) 扫描；Task11 开放可变长公开更新前须使用现成 Core 查找结构或明确测试域预算。本轮不声称任意规模性能保证。

## delta.update-config-presence

- 来源：`xeHentai/core.py:64-71`
- 状态：open（Task11 公开合同验收；Task4 仅内部 typed helper）
- 输入：仅含部分键的运行时更新。
- 实测输出：原版直接索引 `cfg_dict['proxy']`/`cfg_dict['dir']`，缺键会抛 `KeyError`；Task4 的内部 typed helper 按存在性应用，缺失键不产生副作用。
- 拟定合同：Task4 只定义内部 helper 语义（`proxy`/`dir`/`ignored_errors` 的 presence），不把公开 `KeyError` 差异当作已接受。Task11 须独立对照 `rpc_*`/`urls` 过滤、缺 `proxy` 的 no-op、含 `proxy` 但缺 `dir` 的原异常或经批准的显式差异；依赖 Task10 与 Task8 完成。

## delta.config-dir-creation

- 来源：`xeHentai/core.py:80-84`
- 状态：open-gap（Task10 os/fs owner）
- 输入：`update_config` 携带一个不存在的 `dir`。
- 实测输出：原版调用 `os.makedirs` 创建目录；Task4 只更新配置值。当前 pin 的 Core `io` 仅提供 open/read/write/flush/close/exists，没有目录创建调用。
- 拟定合同：由 Task10 的 `crates/xeh-host/src/os/fs.rs` owner 按 Core 优先规则实现最小目录创建能力，真实验证目录不存在、已存在和失败路径；完成前不开放公开配置更新。本轮不新增 native mkdir 或 host function 扩大范围。`diff --suite config` 仍将该差异输出为 `dir-creation` delta，不视为已验证。

## delta.core-trace-jit-default

- 来源：Core pin `99890b6` 的 `JitConfig::default`
- 状态：SDK gap（已记录，未修改 Core）
- 输入：在未启用 `cranelift-jit` 的构建中执行超过热循环阈值的配置键循环。
- 实测输出：`JitNative("native JIT backend is disabled (feature 'cranelift-jit' is not enabled)")`；默认 `enabled` 在 x86_64 Linux 上为 true。
- 拟定合同：配置入口在创建 VM 后显式 `set_jit_config(enabled: false)`，保持解释器执行。该工作区不修改 Core 或依赖 pin。

## delta.task-control-epoch

- 来源：`xeHentai/core.py:142-162`（原版无 epoch）
- 状态：Task5 新增控制语义
- 输入：同 app 的 add/pause/resume/delete、`prepare` 票据、完成/失败事件
- 实测输出：RSS 报告中的 `epochs`、`completions`、`event.{accepted,reason}`
- 拟定合同：RSS 唯一拥有 task state、ready queue 与 epoch。pause/delete/resume、终态记录的 GUID 复活都会推进该 GUID 的 incarnation epoch；旧票据/旧完成事件返回 `stale_epoch`/`unknown_task`/`not_running`，不改变状态、不重开任务、不增加完成计数。epoch 单调递增，达到 `9007199254740991` 后 fail-closed 为 `epoch_exhausted`，不回绕、不重用。原版没有票据/epoch，故该段不参与原版差分；差分只比较返回码、任务表、config、queue 和 update presence。

## delta.task-delete-files-pending

- 来源：`xeHentai/task.py:48-58`、`core.py:133-140`
- 状态：Task5 明确待接线
- 输入：`delete_task_files=true`、`meta.title` 非空、状态为 paused/failed/finished 的 delete
- 实测输出：`{"ok": false, "error": "pending_effect", "pending": {"kind": "delete_task_files", "guid": ..., "directory": ..., "title": ...}}`，任务仍在表中
- 拟定合同：物理删除属于 Task10 文件 owner。当前只产生明确的待执行请求，不报告删除成功、不提前移表；默认无文件效果的删除完整可用并返回 `(0, "")`。Task10 接线并验证真实文件效果前，Task11 不得对外宣称文件已删除。

## delta.task-guid-source

- 来源：`xeHentai/task.py:33`（`str(uuid.uuid4())[:8]`）
- 状态：Task5 新增宿主原语
- 输入：pinned Core 99890b6 无 UUID/随机 builtin（实测无同名函数）
- 实测输出：`xeh_system::uuid_v4` 返回规范 UUID v4 文本；RSS 取前 8 字符作为 GUID；测试通过 `ControlOptions::fixture` 注入与 oracle 相同的 8 位十六进制前缀
- 拟定合同：host 只提供系统随机原语，GUID 形状/切片留在 RSS；生产入口不接受任意 GUID 参数，也不只用测试 ID。差分用同一组计划前缀，生产路径由真实 UUID 生成。

## delta.task-url-strip

- 来源：`xeHentai/core.py:107`（`url.strip()`）
- 状态：已对齐（原实现只覆盖 ASCII 的差异已修复）
- 输入：URL 首尾空白（ASCII 与 Unicode）
- 实测输出：RSS 与 Python `str.strip()` 一致，去除 29 个 Unicode 空白字符（U+0009..000D、U+001C..0020、U+0085、U+00A0、U+1680、U+2000..200A、U+2028..2029、U+202F、U+205F、U+3000）；URL 中间空白保留
- 拟定合同：一次绝对首尾锚定的 Core `re::replace`，只删除首尾空白，不改变大小写或中间字符。RED：U+3000 包裹时原版返回 0/guid 而旧候选返回 1000/null；GREEN 由真实 add 与 task-api 差分验证，纯 ASCII 包裹原本即通过。

## delta.task-warnings

- 来源：`xeHentai/core.py:113-114`
- 状态：Task5 数据面差异
- 输入：`download_ori=true` 且未登录的表站 add
- 实测输出：任务照常创建并入队，RSS 报告 `warnings: ["download_ori_requires_login"]`；原版只记录本地化日志行
- 拟定合同：警告以稳定 key 暴露，本地化文本/日志输出属于 Task13；差分比较返回码、任务表与 queue，不比较警告文本。

## delta.task-monitor-pause

- 来源：`xeHentai/core.py:148-149`
- 状态：Task5 明确未接线
- 输入：pause
- 实测输出：RSS 只把控制状态置 paused 并推进 epoch；不发网络/文件取消
- 拟定合同：native monitor/worker 的取消由 Task9 经 Task3 opaque registration 接线；当前不声称已实现网络或文件取消，也不以 Rust DTO 断言替代。
