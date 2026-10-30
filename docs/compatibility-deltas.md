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

## delta.session-ordered-json

- 来源：`xeHentai/core.py:390-427`、`task.py:434-440`
- 状态：Task6 已对齐（依赖 Core cdb0b798d631cd7ffab6dfed3fe4b69381ed3340）
- 输入：反向字典序的 `tasks` 成员、重复内部 GUID、非空 `page_q`/`img_q`
- 实测输出：RSS 与上游保存/重载后的任务表顺序、ready queue 顺序（含重复项）一致；普通 `Value::Map` 仍是无序 HashMap，只有显式 ordered object-entry codec 承担成员顺序
- 拟定合同：通过标准 `json::decode_object_entries`/`encode_object_entries` 在两个方向保持 `tasks`/`cookies` 成员顺序和每个 value 的原始 JSON 子值；不做全局 Map/VMBC 改造，不新增 preserve_order 或业务 sidecar。

## delta.session-save-json-formatting

- 来源：`xeHentai/core.py:390-400`
- 状态：Task6 已声明差异
- 输入：相同任务/ cookie 数据
- 实测输出：上游 `json.dumps` 默认分隔符/空格与 RSS 紧凑编码的字节不同；解码后的 tasks 成员顺序、Task 字段值、cookies 顺序、返回码一致
- 拟定合同：比较解码后文档、公开任务/queue/cookies/header/login 与磁盘状态，不比较 JSON 空白。

## delta.session-failure-classes

- 来源：`xeHentai/core.py:402-438`
- 状态：Task6 安全差异
- 输入：坏 JSON、缺 `tasks`/`cookies`、`tasks` 非对象、任务缺 `guid`、任务字段类型错误、读/写 IO 失败
- 实测输出：上游对坏 JSON 返回 `(1103, str(ex))`，对缺键/类型错误常抛出未捕获的 `KeyError`/`AttributeError`；RSS 对坏 JSON 返回 `1103`，其余情况返回带 class 的失败报告且不发布半个 Root，原 `h.json` 字节保持不变
- 拟定合同：使用显式 case 表，不把上游异常伪装成 1103，也不以“双方都失败”放行任意失败。固定映射为 `unreadable_session` PermissionError→host_failure/null、`bad_json` return 1103→parse/1103、`missing_tasks` KeyError→missing_tasks/null、`tasks_type` AttributeError→structure/null、`missing_guid` KeyError→missing_guid/null；每个失败 case 的 post tasks/queue/cookies/header/login/disk 必须等于该侧 pre-invocation baseline。错误的 class/code、插入 task、改 cookie/queue/header/login/disk 都由同一生产 comparator 拒绝。真实解析消息文本不逐字节比较。写路径另有数据保全差异：上游 `fopen(...,"w")` 在 try 外，权限/打开失败会抛异常且可能已截断；RSS 先完整序列化再同目录原子替换，任何 precommit 失败返回 1103+诊断并保留旧文件。

## delta.session-outside-root

- 来源：`xeHentai/task.py:173-206`（`get_fpath`/`scan_downloaded`）
- 状态：Task6 安全差异
- 输入：会话任务 `config.dir` 指向可信 download roots 之外
- 实测输出：上游 `os.path.exists` 对不存在路径静默返回 False 并发布任务（finished=0）；RSS 的 `xeh_fs` 在路径越界时以稳定的 `xeh_fs:root_denied` 拒绝，加载失败且不发布任务
- 拟定合同：所有 stat/remove/rename 受可信 download roots 限制，导入任务的 `config.dir` 不能扩大权限；差分只接受带 root-denied tag 的 host failure，其他错误不算该 case 的正确结果。Task7/Task10 接入生产配置目录时再显式加入可信根。

## delta.session-set-duplicate-map

- 来源：`xeHentai/task.py:434-440`（`json.dumps` 遇到 `set` 抛 `TypeError`）
- 状态：Task6 不可达差异
- 输入：`duplicate_map[fid] = set([...])`
- 实测输出：上游保存返回 `1103`；RSS 的 JSON 数据模型没有 set 值，离线会话中 `duplicate_map` 只以数组形式存在，因此不会构造该失败
- 拟定合同：不新增 Python 容器或 set 仿真；若未来运行期需要 set 等价物，必须用显式数组/顺序规则并由真实 oracle 重新验证。权限拒绝与 rename 前失败已覆盖“保旧文件”。

## delta.session-ticket-owner

- 来源：Task5 ticket 边界（无上游对应物）
- 状态：Task6 增强
- 输入：两个控制器使用相同 GUID、相同 epoch 计划；把 A 的 prepare 票交给 B
- 实测输出：B 以 `stale_owner` 拒绝，状态/完成计数不变；票中 owner 为每控制器 full UUID，不写入旧 `h.json`
- 拟定合同：prepare 与 event 同时校验 owner+guid+epoch+kind；恢复导入只重新分配运行期 epoch，不把磁盘数据变成 live 授权。

## delta.session-set-login-boundary

- 来源：Task5 Q5 / Task6 登录边界
- 状态：Task6 收口
- 输入：生产控制器收到 `set_login=true`
- 实测输出：返回 `set_login_not_allowed`，`has_login` 不变；fixture `allow_seed` 仍可注入以复用 oracle 计划
- 拟定合同：生产登录状态只来自真实导入 cookie 与后续 Task7 登录响应；不以 bool RPC 构造认证入口。通用 dispatch report 对生产控制器只返回 `has_login`，cookies/cookie_header 为 `null`；只有可信 fixture（`allow_seed`）报告有序合成 cookie，供 session 差分与测试核对。

## delta.session-remaining-download

- 来源：Task6 部分下载闭环
- 状态：Task6 已执行（本地合成 HTTP）
- 输入：固定 43fc55c 原版 `xeHentai.save_session()` 实际写出的 `h.json`（同一任务已含已下载 fid 1 与缺页 11/12，`img_q`/`page_q`/`reload_map` 三者齐全）；原字节直接交给新 VM，不经 Rust JSON 重写
- 实测输出：RSS 载入/扫描后由 done 集/`remaining_plan` 过滤已完成的 fid 1；三个 fid 都有本地 HTTP 路由，但只有 11/12 实际被请求并经 Task3 worker 下载到 `.xehpart`，再由 `xeh_fs::rename_atomic` 同目录换名；重新 load/scan 得到 finished=3、plan 为空，保存后第三 VM 读回相同旧 GUID/queue/cookie；原版在同一磁盘布局上的 load/scan 结果实际对照
- 拟定合同：Rust 只运输计划与执行文件原语，页面/图片选择留在 RSS；不提前实现 Task7/Task9/Task10 的解析、调度或归档，真实站点/账号/公网代理不参与。跨版本 producer 与同 layout 原版对照固定在 `crates/xeh-xtask/tests/session_e2e.rs`，不覆盖已冻结的 Task6 原始证据。

## delta.session-authority-separation

- 来源：Task6 R1 文件权限修正
- 状态：Task6 安全收紧
- 输入：session/legacy 父目录位于 download roots 之外，且各含零字节 fid 文件；或默认生产 download root 与 session 父目录重叠
- 实测输出：Core 只读 roots 只含当前存在的 exact session/legacy 文件；`stat`/`remove_zero_file`/`rename_atomic` 只按显式 download roots 授权；exact session/legacy 文件即使落在 download root 内也不能被 remove/rename，legacy 永不可写；`write_atomic` 只接受 exact session target。拒绝时 task/queue/cookies 不发布，文件字节与存在性不变
- 拟定合同：能力按“只读 exact 文件 / 可写 exact session / download mutation roots”分开，不构造通用 ACL 框架；导入 JSON 与普通请求不能提升 cap。

## delta.session-zero-symlink

- 来源：`xeHentai/task.py:173-206`（`os.stat` 跟随、`os.remove` 删除链接）
- 状态：Task6 对齐
- 输入：download root 内指向零字节目标的图片 symlink
- 实测输出：上游跟随目标看到 size 0 后删除链接本身；此前 RSS 先 canonicalize 再删除了目标。当前 RSS 在授权后删除传入的链接路径，目标文件保留；指向非零目标的链接不删除
- 拟定合同：只修零字节图片清理的直接相邻语义，不扩展为同 UID 全文件系统竞态工程。

## delta.session-byte-budget

- 来源：Task6 R1 读写预算
- 状态：Task6 安全收紧
- 输入：session 文件恰好 16 MiB、16 MiB+1、以及 UTF-8 多字节字符恰好/超过 byte 预算
- 实测输出：Core `read_all` 在 Reader 层以同一预算硬拒绝；App 在打开已知超限文件前用 exact 元数据预检并以 `session:read_limit` 拒绝，旧文件保留；`write_atomic` 在 staging 前按 UTF-8 字节数拒绝超限载荷，返回 `committed=false` 且旧文件精确字节不变；失败后同 VM 小文件 load/save 正常，资源/pending 归零
- 拟定合同：读写共用一个 `SESSION_READ_LIMIT_BYTES`（16 MiB）来源；测试可通过可信 `SessionOptions` 设置更小预算，但不增加 `config.json` 权限键或导入文件开关；Core 的 Reader 上限仍是硬边界，stat 预检只为避免已知超限文件占用句柄，不冒充上限。

## delta.session-scan-budget

- 来源：Task6 R1 资源预算
- 状态：Task6 安全收紧
- 输入：带 `filelist` 的任务 `meta.total` 超过累计预算、为负数，或导入 JSON 试图扩大预算
- 实测输出：RSS 在任何 scan/零文件删除/新 Root 发布前预检所有任务 total，拒绝为稳定的 `session:scan_budget`；原文件、Root、磁盘与队列不变。默认一次恢复最多 100000 个 fid，预算只来自可信 `SessionOptions`
- 拟定合同：不实现通用 schema 框架或完整 Task9 调度，不增加 sidecar/配置键；超大 total 不进入 fid 遍历。
