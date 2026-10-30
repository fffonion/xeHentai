# RustScript 嵌入与 Core host 复用

锁定 revision：`cdb0b798d631cd7ffab6dfed3fe4b69381ed3340`。依赖包名是 `pd-vm`，库名是 `vm`。默认 feature 含 JIT，本嵌入关闭默认 feature，只打开 `runtime`、`async`、`http-client`。不复制 Core 源码，不启用 SQLite 或 CLI。该 revision 的有序 object-entry codec 仍是会话成员顺序的唯一来源；`io::read_all` 已按 `IoPolicy::max_read_bytes` 在 Reader 层最多读取 `limit + 1` 字节后拒绝，App 不复制 Reader、也不以 stat 预查冒充硬上限。

编译和绑定使用同一份 catalog。Core 的 `json::encode` / `json::decode` / `re` 没有 guest catalog，走 namespaced builtin，VM 上安装 `standard_composition()` 后才能调用；`xeh_json` / `xeh_io` 是应用自己的 named typed host，不覆盖这些 builtin。`io`、`timer`、`http` 由 `standard_host_modules()` 的 catalog surface 经 `install_from_catalog` 安装，adapter 是 Core descriptor，不是应用侧重写。

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
| JSON 严格校验 | `xeh_json::is_valid`（应用侧 named typed host；解析语义来自 Core `json::decode` descriptor 的 StaticArgs adapter） | 应用 `standard_catalog` 的 named 函数 + `register_catalog_function` | `runtime` | `guest_json_validation_matches_the_strict_decoder`、`pinned_core_decode_descriptor_is_an_args_only_adapter` |
| 整文件文本写入 | `xeh_io::try_write_text`（应用侧 named typed host；`IoPolicy` snapshot + `Vm::submit_host_future`） | 应用 `standard_catalog` 的 named 函数 + `register_catalog_function` | `async` | `guest_text_writer_enforces_policy_and_continues`、`guest_text_writer_denies_without_policy_and_counts_bytes` |
| UUID v4 文本 | `xeh_system::uuid_v4`（应用侧 named typed host，OS entropy） | `register_catalog_function` | `async` | `production_guids_and_seed_boundary` |

`io` 的 catalog surface 包含 `open`、`read_all`、`write`、`flush`、`close`。`IoResource` 报告 `io.file`。应用不复制这些资源型入口，不伪造资源键或数字句柄；`xeh_io::try_write_text` 是应用侧不返回资源的整文件文本写入，只按同一份 `IoPolicy` 工作。`io::popen`、`io::read_line`、`io::exists` 仍走 namespaced builtin；测试不调用它们。

`xeh_json::is_valid` 在 registry 安装时只解析一次 Core `json::decode` descriptor，并校验它确实是 args-only adapter，之后用保证为 string 的 `Value` 调用同一解析路径；成功只回 `true` 并丢弃解码值，严格拒绝回 `false`，其他 VM/类型错误原样传播。`xeh_io::try_write_text` 在 await 前快照 `IoPolicy`（缺少 policy 视为拒绝），在 open/truncate 前检查 `allow_write`、UTF-8 字节上限和允许根（缺失目标经已存在父目录解析，最终符号链接必须解析到根内），普通拒绝/超限/OS 失败返回命名 `IoWriteTextResult`，取消与 bridge 生命周期不变。

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

## Task 5 任务控制

控制入口是 `app/domain/state.rss`。`crates/xeh-host/src/task_control.rs` 的 `TaskControl` 编译一次并绑定一个 VM，`create` 返回真实 RSS 根值；此后每个命令都是一次 `dispatch(root, request)` 调用，返回的新根值进入下一次调用。Rust 只做 JSON 请求/报告的字符串运输，不保存 Task 表、不决定状态转换。测试 `first_task_keeps_its_configuration_across_a_global_update` 与 `task-api` 差分都是在同一 owner 上连续调用，没有什么地方重建空 store，也没有把 store 编码成 JSON 字符串。

RustScript 的几项语言事实决定了这个布局，实测记录在 `crates/xeh-host/tests/state.rs`：

- 命名 struct 类型按模块解析，`app/domain/task.rss` 声明的记录无法作为 sibling module 的字段/参数类型；Task 记录、Store、Step 因此同处 `state.rss`，`task.rss` 只保留 URL 识别、空白裁剪和 14 键 task config 合并，`queue.rss` 只保留 FIFO 操作。
- 读取非标量 struct 字段会把它移出记录；所有读取都经 copy-consuming accessor 或 `value.copy()`，线性 helper 才整体 move store。把同一个 struct 在多个 if 分支里 move 会在后续 copy 时得到缺字段的 store（表现为 `map key not found`），控制入口因此统一传 `store.copy()`。
- struct 参数是副本：对参数的修改不会回传给调用者，所以根值必须显式往返。probe 中两次 bump 后第二次读到第一次的计数，证明同一 owner 的根值跨调用连续。
- `json::encode` 不接受 optional 值；`set` 写入 null 会删除 map 键，因此带 null 的已解析 JSON（25 键配置数据、task overrides）用一次 Core JSON decode 构造/合并，而不是逐键 set。

配置仍由 `app/config/types.rss` 唯一拥有：`defaults_data`、`baseline_from_data`、`update_data` 把 25 键记录以已解析 Core map 数据交给 task 域，校验/更新仍走 `apply_update`。`data_of` 用本模块自己的 25 键 JSON 编码解码出数据，`with_null` 只在键确实缺失时补 `null`，避免重复键。

GUID：当前 pin 的 Core 没有 UUID/随机 builtin。新增唯一宿主原语 `xeh_system::uuid_v4`（named typed，返回规范 UUID v4 文本），RSS 取前 8 字符；测试通过 `ControlOptions::fixture` 注入与 oracle 相同的计划前缀，生产 `open` 不暴露任意 GUID 参数。

epoch/queue：RSS 唯一拥有 task 表、ready queue、每 GUID incarnation epoch 和完成计数。pause/delete/resume、终态记录被同 GUID add 复活都会推进 epoch；pause/delete/resume 之前准备的票据在之后返回 `stale_epoch`、`unknown_task` 或 `not_running`，不改状态、不重开、不重复计数。epoch 达到 2^53-1 后 fail-closed。队列保留已删除任务的旧 guid（与原版 `Queue` 一致），resume 一律入队，finished resume 保持 20。

Task5 不实现 session、站点扫描、代理、下载器、文件归档或 RPC server；`delete_task_files` 需要物理效果时只返回 `pending_effect` 待执行请求并保留任务。native worker/monitor 的取消与真实扫描/下载接线属于 Task9，当前没有声称网络或文件取消已验证。

## Task 6 会话、扫描与文件权限

会话恢复入口仍是 `app/domain/state.rss` 的同一 owner VM。`app/domain/session.rss` 只拥有文档编解码、路径/文件名与扫描/剩余计划规则；`crates/xeh-host/src/os/fs.rs` 只提供 Core 未公开的文件原语。

文件能力按来源分开，不合并成一组 root：

- **Core 只读来源**：`IoPolicy.allowed_roots` 只放当前存在的 exact session 文件与 exact legacy cookie 文件，绝不放它们的父目录；`xeh_fs::session_paths` 每次调用先按当前存在性刷新这份 policy，因此 absent→save→load 的同 VM 顺序也可用。不存在的目标不登记为 root，RSS 通过 `session_exists`/`legacy_exists` 决定是否调用 `io::open`，不会拿父目录做替代读权限。
- **session exact target**：只有 `FsState` 记录的 exact session 文件可通过 `write_atomic` 原子替换；`write_atomic` 不写 download roots，也不写 legacy。目标若是 symlink 明确拒绝替换。
- **legacy exact file**：只读；Core 读取之外没有任何写/删/改名入口。
- **显式 download roots**：`stat`（含 follow symlink 的元数据）、零字节文件删除、图片 stage 的 `rename_atomic` 只按这些 roots 授权。session/legacy 父目录即使等于某个 download root，exact session/legacy 文件仍是保留目标：`remove_zero_file`、`rename_atomic` 的 source/target 命中它们时以 `xeh_fs:root_denied` 拒绝，不能用同名 rename 绕过写预算或改 legacy。

零字节图片 symlink 保持上游 `os.stat` + `os.remove` 语义：元数据跟随链接（目标为空才进入删除），删除的是调用方传入的链接路径，不删除规范目标。普通零文件仍直接删除；不安全的外向链接也只会被移除链接本身。

读写使用同一个 `SESSION_READ_LIMIT_BYTES`（16 MiB）byte 预算。Core `io::read_all` 的 Reader 层硬上限由已审 Core 提供（最多 `limit + 1` 字节再拒绝）；App 在 `session_paths` 里用 exact 文件元数据做同预算预检，已知超限的文件不会先打开句柄，失败后同 VM 仍可继续小文件读写且资源/pending 归零。`write_atomic` 在创建 staging、写入或 rename 之前检查完整 UTF-8 载荷的字节数（不是字符数），超限返回 `committed=false` 并保持旧文件精确字节；正常 pre/post-commit 崩溃语义不变。

扫描预算来自可信 `SessionOptions`（默认一次恢复累计 100000 个待扫描 fid），导入 JSON 与 `config.json` 都不能扩大。RSS 在任何 `xeh_fs::stat`、零文件删除或新 Root 发布之前检查全部带 `filelist` 任务的 `meta.total`：非整数/缺失沿用既有 scan 结构错误，负数、累加溢出或超预算统一以稳定的 `session:scan_budget` 拒绝，原文件、Root 与磁盘不变。

会话差分使用显式失败 case 表，而不是“任何双方失败”放行：`unreadable_session`/`bad_json`/`missing_tasks`/`tasks_type`/`missing_guid` 的 oracle outcome/exception/return 与 RSS class/code 都是固定 tuple；`outside_root` 只接受带 `xeh_fs:root_denied` 的 host failure 且不发布任务。所有失败还比较 post tasks/queue/cookies/header/login/disk 与各自 pre-invocation baseline（`outside_root` 的 upstream 发布行为单独记录为安全差异）。失败 mutation gates 与成功四负例共用同一生产 comparator。

跨版本剩余图验收在 `crates/xeh-xtask/tests/session_e2e.rs`：固定 43fc55c 的 `xeHentai.save_session()` 通过共享 `compat/oracle/session_runner.py` 写出原始 `h.json`（输入任务同时含已下载 fid 1 与缺页 11/12）；新 VM 加载后由 RSS done 集/`remaining_plan` 过滤 fid 1，薄 driver 只运输计划到已接受 Task3 worker 与 `xeh_fs::rename_atomic`。本地 HTTP fixture 为三个 fid 都准备响应，但只允许 fid 11/12 被请求；重扫 finished=3、plan 为空、保存后第三 VM 保持旧 GUID/queue/cookie，并与原版在同一磁盘布局上的 load/scan 结果对照。


## Task 7 站点过滤与 RSS 规则内核

站点/规则入口是 `app/site/entry.rss`，它只做一次委托；`app/rules/engine.rss` 持有命名的 `SiteContext`/`StreamInput` 和有序分发。engine 只决定哪个过滤器先跑，不解析夹具 id、不重写站点行为；站点提取仍在 `app/site/{metadata,pages,mpv,image,login,support}.rss`，配额与流规则在 `app/rules/{image,download}.rss`，`app/rules/metadata.rss` 是 metadata 的规则相位包装。

`support.rss` 负责固定源 regex 组合（`re::replace` 标记 + literal split + 逐段 captures）、单遍 HTML 实体替换与十进制转换。Unicode `Nd` 数字不依赖内嵌 regex 引擎的 Unicode 版本：`nd_digits()` 的十组十进制值字符类由 oracle 解释器的 `unicodedata`（Unicode 16.0.0）生成，`ascii_digits()` 只归一化被捕获的数字 token，之后才进入既有 ASCII 累加；标题、URL 与整页文本不经过该归一化。

`download.rss` 保持原有配额/404/长度/回调预检顺序，并把流处理拆成 Task9 可复用的有界状态机：`stream_begin(expected_length)` 返回命名 `StreamState`（只含计数、终端标志与失败码，不保存镜像字节），`stream_chunk`、`stream_connection_error`、`stream_end` 每次只接收一个有界 chunk 并返回 `StreamStep{state,events,decisions}`。夹具批量 runner 与跨调用测试驱动同一组 transition，没有第二套事件模型；空 chunk、连接/回调异常、短/长完成和互不相关的流都由同一实现处理。

站点差分在 `crates/xeh-xtask/src/site.rs`：先按 Git 树与 blob 逐字节核对固定源 `43fc55c0c662f195d048becf6276a26ce06b09d9` 的 19 个包文件，再由 `compat/oracle/site_runner.py` 在隔离 cwd/env 中执行真实上游 `filters.py`/`util`。比较覆盖 callbacks/顺序、return、exception class、debug 字节与 stream 事件/chunk 字节；9 个 mutation 由同一生产 comparator 拒绝。`rss-test --suite rules` 执行 17 个 RSS 断言 case，`diff --suite site` 执行 36 个 oracle 差分 case（site+rules）。已知差异只有三个窄边界，逐条记录在 `docs/compatibility-deltas.md`，没有全局放行或业务 URL/状态忽略。

## Task 8 代理传输与 RSS 池、Glype

代理路由在入队前固定：`NativeSubmission` 私有保存 `Option<NativeProxyRoute>`（protocol、proxy host/port、`trusted_remote_dns`），`TaskRegistration::submission` 仍是 Direct 默认，`submission_with_route` 只供明确路径使用，`submission_with_options` 才启用 follow-redirect。原生 `net/connectors.rs` 只做协议机制：代理 TCP/外层 TLS、HTTP absolute-form、CONNECT、SOCKS4/4a/5/5h 与有界握手读取；`net/client.rs` 按 route 选择连接器并复用既有 hyper1/Tokio/rustls/driver/cancel/deadline/staging 所有权。代理失败没有任何直连回退，环境代理变量不被读取；remote-DNS 模式没有显式 trusted embedding grant 时以 `policy_denied` 关闭。Task3 旧入口继续拒绝重定向，显式 Task8 `OpenOptions.allow_redirect_response/follow_redirects` 才改变。

RSS 池状态由 `app/domain/state.rss` 的 Store 持有（`proxy: map?`），`app/proxy/pool.rss` 只做纯状态操作：插入顺序、首可用、成功/失败计数、默认 5 阈值、not_good 的相对过期、trace 的永久零、`has_available_proxies` 的取反命名以及真实 CPython sweep 的 partial RuntimeError。`app/proxy/policy.rss` 是源 anchored regex 的 RSS 组合（image-only 用同一正模式的取反，无 Python regex 层），`app/proxy/glype.rss` 完成 mkurl/argname/b、Referer 与 site cookie 重映射、两次 sslagree、Set-Cookie 解包、恢复 URL、text-only 链接重写与非文本字节保留。`core.update_config` 的 pool 分支在 `op_update` 内按 truthy incoming proxy 执行，只追加/重置当前配置列表，不删除旧地址。

新增 host 能力及真实缺口证据：
- `xeh_url::quote_plus/unquote`：锁定的 Core 没有 percent/form 原语，Core HTTP 也不提供；真实 Glype fixture 需要 Python `quote_plus` 的安全集/空格加号/大写 hex 与 `unquote` 的 UTF-8 replacement（不把 `+` 当空格）。实现在 `app_host/url.rs`，无业务策略。
- `xeh_proxy::request`：Core HTTP client 固定 follow redirects 且没有 proxy route，无法观测 sslagree 的 redirect-disabled 响应或池选出的原生连接器。已核对锁定 cdb0b79 catalog/source：`http::request::{new,set_header}`、`http::client::request`、`http::response::{status,url,header_names,header_values,body}` 均存在，但 `HttpConfig` 只有 `max_redirects`，为 0 时重定向直接报错而不是返回 3xx，也没有 connector/proxy 选择接口；因此新增本 host。参数/返回均为命名字符串/整数/布尔数组与命名 `ProxyResponse`，不做 URL/cookie 选择；RSS 仍拥有全部业务决策。`run_proxy_scenario` 的授权 envelope 在启动前固定，guest 不能放宽。

原生协议验收在 `crates/xeh-host/tests/transport.rs`（含 `proxy_fixture/transport_proxy.rs`）：HTTP absolute-form、CONNECT+HTTPS、HTTPS 代理外层 TLS、TLS→CONNECT→内层 TLS、SOCKS4/4a/5/5h、remote DNS 证明、直连陷阱、环境变量、跨域 redirect header 剥离、外层/内层证书失败、取消与 worker 落盘。固定源差分在 `crates/xeh-xtask/src/proxy.rs` + `compat/oracle/proxy_runner.py`：真实 `proxy.Pool`、真实 `core.xeHentai.update_config`、`util.get_proxy_policy` 与真实 requests Glype handler 对本地脚本服务器运行；12 个 case 全量比较 pool 状态/计数/禁用/选择、config pool、policy 布尔和 Glype 请求 transcript/文本/cookie/bytes，12 个 mutation 由同一 comparator 拒绝。已知窄决策记录在 `docs/compatibility-deltas.md`。Task8 未标 accepted；Task9 调度与 Task10/11 公共 RPC/目录/归档仍待实现。
