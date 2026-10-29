# RustScript 嵌入与 Core host 复用

锁定 revision：`d9387f98df78dd88d2d1c04eebac4415f844efaf`。依赖包名是 `pd-vm`，库名是 `vm`。默认 feature 含 JIT，本嵌入关闭默认 feature，只打开 `runtime`、`async`、`http-client`。不复制 Core 源码，不启用 SQLite 或 CLI。

编译和绑定使用同一份 catalog。`json` / `re` 没有 guest catalog，走 namespaced builtin，VM 上安装 `standard_composition()` 后才能调用。`io`、`timer`、`http` 由 `standard_host_modules()` 的 catalog surface 经 `install_from_catalog` 安装，adapter 是 Core descriptor，不是应用侧重写。

每个 VM 自有 registry、async bridge 和 `EmbeddingTimer`。没有进程级 registry 缓存，没有 VM 池，不把 `Vm` 或 `Value` 送到另一条线程。取消走 `reset_for_reuse` 和 `poll_reset_for_reuse`，bridge 的 `cancel_op` 丢掉尚未完成的 future。HTTP 的 in-flight permit 跟这条 future 一起释放；`set_http_max_in_flight(1)` 时，取消后的同 VM 仍能发出下一笔请求。

`EmbeddingTimer` 的 owner 是创建它的线程。`drive_due`、`drive_with` 和 `shutdown` 只在该线程执行。其他线程得到 typed host error，调用方把命令交回 owner。这个 API 不从另一条线程抢占正在执行的 callback 字节码。

准入在同一把 scheduler 锁里完成：到期项离开 pending 的同时占一个 running 槽。`Waiting` 和 `Yielded` 继续占用该槽，超额到期项留在 pending。callback 开跑前安装独立 `EmbeddingBridge`，之后由 `OwnedTimerCallback::poll_reporting` 推进到再次 pending 或终态。guest callback 执行期间不持有 scheduler 锁，所以 callback 可以再注册 timer；它不能重入 drive 或 shutdown。

owner 线程 shutdown 先停止准入和后续排程。尚未开跑的 callback 以 `premature=true` 运行一次后取消；已经 active 的 callback（含 Waiting）直接取消并释放 callback VM。某个 callback 报错不会跳过其余项。成功返回时 pending 和 running 都是 0。再次 shutdown 不再调用 callback，也不重排 periodic。

测试夹具目录由各测试的 RAII 根创建，位置跟随进程 `TMPDIR`。IO policy 只授权该测试根。

## 复用矩阵

| 能力 | Core 入口 | 安装方式 | feature | 验证 |
| --- | --- | --- | --- | --- |
| JSON 编码/解码 | `json::encode`、`json::decode`（`src/builtins/runtime/json.rs`，descriptor-only） | builtin + `standard_composition` | `runtime` | `app/main.rss` 往返；`json_decode_error_reaches_the_caller` |
| 正则 | `re::match`（`src/builtins/runtime/regex.rs`，descriptor-only） | builtin + `standard_composition` | `runtime` | `app/main.rss` 的 `^probe$` |
| 文件打开 | `io::open`（`src/builtins/runtime/io`，catalog surface 只有 `open` / `read_all` / `close`） | `install_from_catalog` + `IoHostExt::configure_io`；`write` / `flush` 另授 namespaced builtin | `async` | `io_catalog_open_rejects_the_unstamped_handle` |
| 定时器 | `timer::at`、`timer::every`、`timer::pending_count`（`src/builtins/runtime/timer.rs`） | `install_from_catalog` + `TimerHostExt::install_timer_runtime` | `runtime` | `timer_max_running_keeps_a_due_callback_pending_while_http_waits`、`timer_shutdown_closes_an_active_http_callback`、`timer_rejects_drive_and_shutdown_from_another_thread` |
| 缓冲 HTTP | `http::request::new`、`http::client::request`、`http::response::status`、`http::response::body` | `install_from_catalog` + `HttpHostExt::configure_http` | `http-client` | `http_policy_allow_and_deny`、`cancelled_http_request_closes_and_vm_is_reused` |

`io::popen` 不调用，`IoPolicy.allow_process` 保持关闭。HTTP 策略只允许测试夹具的 `127.0.0.1` 和当次端口。

异步 `IoResource` 没有实现 `HostResource::resource_type_key()`。`standard_host_catalog()` 要求键 `io.file`，所以 `install_from_catalog` 之后，`io::open` 的返回句柄在回到 guest 时被拒绝，实际键是 `<none>`。owned descriptor 里的 `io::write` 参数是 `int`，和 `io::open` 返回的 `resource<io.file>` 对不上，不能靠换一份 catalog 绕过。缺的公开接口是：异步 IO 资源在插入时报告 `io.file`。本嵌入不包装、不伪造这个键。

测试资源 `xeh.test.probe` 只在 `crates/xeh-host/tests` 注册，用来证明句柄的 wrong-owner 和 close-once。生产 `run_app` 不注册它。

跨模块 named struct 在 `app/types.rss`。guest schema 不允许省略已声明字段，空的 `string?` 写成 `note: null`；运行时这个键不在 map 里，读取要用 `?.`，否则会报 map key not found。`app/main.rss` 链接该模块，正则命中后再加上两个分数。结果 22 来自 RSS：解码后的 score 20，`note` 为 null 的 probe 加 1，正则命中加 1。库和二进制都不写死这个数；`three.rss` 走同一 runner，结果是 3。

## Task 3 仍缺的原语

本任务不实现这些缺口：

- Core HTTP 没有把响应体按块交给调用方、且不在 VM 中保留全量字节的下载流。现有 `http::client::request` 返回缓冲 body。`http::client::sse` 是事件流，不能当作图片下载流。
- 没有 per-thread VM 池、有界进度事件、暂停与完成同时发生的通道，也没有大于 Core 默认缓冲仍能流式落盘的 host。
- 应用配置、站点、cookie、下载调度、RPC 和更新器都不在本嵌入里。
