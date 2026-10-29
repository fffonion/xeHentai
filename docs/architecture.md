# RustScript 嵌入与 Core host 复用

锁定 revision：`99890b68119715df9ddd19f6a6362ff8d6258ffa`。依赖包名是 `pd-vm`，库名是 `vm`。默认 feature 含 JIT，本嵌入关闭默认 feature，只打开 `runtime`、`async`、`http-client`。不复制 Core 源码，不启用 SQLite 或 CLI。

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
| 文件读写 | `io::open`、`io::read_all`、`io::write`、`io::flush`、`io::close`（`src/builtins/runtime/io`；`write` / `flush` / `read_all` 借用 `io.file`，`close` 消耗该资源） | `grant_owned_builtins` 后 `install_from_catalog`，再 `IoHostExt::configure_io` | `async` | `standard_catalog_file_round_trip_reuses_the_vm`、`io_policy_authorizes_only_the_test_root`、`io_policy_rejects_write_when_allow_write_is_false` |
| 定时器 | `timer::at`、`timer::every`、`timer::pending_count`（`src/builtins/runtime/timer.rs`） | `install_from_catalog` + `TimerHostExt::install_timer_runtime` | `runtime` | `timer_max_running_keeps_a_due_callback_pending_while_http_waits`、`timer_shutdown_closes_an_active_http_callback`、`timer_rejects_drive_and_shutdown_from_another_thread` |
| 缓冲 HTTP | `http::request::new`、`http::client::request`、`http::response::status`、`http::response::body` | `install_from_catalog` + `HttpHostExt::configure_http` | `http-client` | `http_policy_allow_and_deny`、`cancelled_http_request_closes_and_vm_is_reused` |

`io` 的 catalog surface 包含 `open`、`read_all`、`write`、`flush`、`close`。`IoResource` 报告 `io.file`。应用不另写文件 host，不伪造资源键或数字句柄。`io::popen`、`io::read_line`、`io::exists` 仍走 namespaced builtin；测试不调用它们。

`standard_catalog_file_round_trip_reuses_the_vm` 使用同一份 standard catalog 和 registry，关闭 default host fallback，并用只授权当次 RAII `TestRoot` 的 `IoPolicy`。RSS 执行 writable open、borrowed write、flush、consuming close、reopen、`read_all`、close。`io::write` 的返回值与宿主从磁盘读到的字节数一致，`read_all` 与这些磁盘字节一致；flush 和两次 close 成功。结束时 resource count 与 pending IO 操作为 0。`release_for_reuse` 后同一 VM 再执行该脚本，结果相同，资源再次归零。字节数来自 RSS 返回值和宿主读盘，文档不写死计数。

`io_policy_authorizes_only_the_test_root` 拒绝授权根以外的路径。`io_policy_rejects_write_when_allow_write_is_false` 在 `allow_write=false` 时拒绝写，且不创建文件。两条失败路径结束后资源与 pending IO 操作为 0。

`io::popen` 不调用，`IoPolicy.allow_process` 保持关闭。HTTP 策略只允许测试夹具的 `127.0.0.1` 和当次端口。

测试资源 `xeh.test.probe` 只在 `crates/xeh-host/tests` 注册，用来证明句柄的 wrong-owner 和 close-once。生产 `run_app` 不注册它。

跨模块 named struct 在 `app/types.rss`。guest schema 不允许省略已声明字段，空的 `string?` 写成 `note: null`；运行时这个键不在 map 里，读取要用 `?.`，否则会报 map key not found。`app/main.rss` 链接该模块，正则命中后再加上两个分数。结果 22 来自 RSS：解码后的 score 20，`note` 为 null 的 probe 加 1，正则命中加 1。库和二进制都不写死这个数；`three.rss` 走同一 runner，结果是 3。

## Task 3 worker

固定数量的 OS 线程组成 `WorkerPool`。每条线程自建 current-thread Tokio runtime、VM、registry、async bridge 和 `EmbeddingTimer`，编译同一份只读 `app/worker/transfer.rss`。跨线程复制 `JobKey`、typed DTO 和 registration lease 的 `Arc`。不发送 `Vm`、`Value` 或 VM 资源句柄。`register` 返回不透明 lease。`submit` 只接收该 lease 预先包装的 `NativeSubmission`，授权身份在包装时固定，提交时不再按 task id 读取当前登记。`cancel` 与 epoch advance 携带同一 lease。同 id、同 epoch 的再次登记分配新身份，旧 envelope 不能绑定到新登记。身份是可跨线程复制的普通 `Arc`。VM 资源句柄仍留在 worker owner 线程。

`json`、`re`、`io`、`timer` 和缓冲 HTTP 仍走上面的 Core catalog。公开 SDK 没有通用 binary reader，`http::client::request` 继续返回缓冲 body，`http::client::sse` 不是图片流。图片下载是最小 typed extension：`xeh_transfer::job` / `xeh_transfer::run`。传输持有 Hyper HTTP/1 connection driver 的 `JoinHandle`。spawn 之后立刻记入该 worker 的 owner。取消、状态错误、header 失败和 body 早退都 abort 并等待 driver 退出；超时未退出的 handle 进入有界 retirement queue，不在 `Drop` 里丢掉。worker 在复用和下一次 quiescence 前 await 这些 handle。`shutdown(&self)` 借用 pool，到期报告 `draining` 并保留 pool，可再次 drain；只有线程已 join 且 driver 计数为 0 才报告资源归零。TLS 用 rustls 校验证书，不跳过校验。进程不能强制终止不可中断的 OS 调用。不读环境代理，不跟随重定向。owner、build、executing 样本按 worker 唯一身份有界，重建和执行次数用 scalar 累加。body 空闲超过 `stream_idle_timeout` 即关闭，不等整体 deadline。成功 link 后的终态不可再 cancel；permit 和 flight 在等待终态事件之前归还。

响应体按 chunk 写入同一文件系统的独占 staging 文件。EOF、flush、close 之后，epoch 锁只包住准入检查和同目录 hard link。既有目标文件不覆盖。代次前进或取消先于发布时，staging 删除，当前代次完成计数不变。进度事件按 job 合并；consumer 还在时终态不丢。队列、事件和 epoch 表都有上限。

`two_rss_workers_enter_the_server_barrier_on_distinct_owner_threads` 用真实 TCP 门闩证明两个 owner 线程同时进入请求。`worker_streams_png_larger_than_core_body_cap` 先记录 Core body-limit 拒绝，再由 worker 流式写盘，并按块比较原文件和下载文件。取消、写失败、worker panic、队列满和路径拒绝由 `tests/transport.rs` 覆盖。站点、cookie、下载调度、RPC 和更新器仍不在本嵌入里。
