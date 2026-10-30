# xeHentai → RustScript 全功能兼容重写 Implementation Plan

> **For Hermes:** 实施时加载 `subagent` / `test-driven-development`，按本文依赖顺序执行；每个范围由独立实现者负责，完成范围审查后再集成。本文仅为计划，文中目标目录、测试工具和命令均为待创建项。

**Goal:** 用 RustScript（`.rss`、pd-vm）承载 xeHentai 的应用逻辑，保留原版 CLI、任务与会话、扫描和多线程下载、规则处理、代理池、JSON-RPC、gallery、自动更新、用户配置及三语行为；现有 WebUI 可直接连接。

**Architecture:** 独立应用项目，RSS 负责业务状态、规则及策略，Rust 原生宿主负责系统 I/O、网络传输、工作线程和资源生命周期。先建立 Python 原版的行为基线和差异账本，再逐项迁移并做同输入差分验收。RPC/gallery 和旧文件格式视为公开兼容合同。

**Tech Stack:** RustScript / pd-vm；Rust / Tokio 多线程运行时；应用自有 typed host extensions；HTTP 传输采用 reqwest/rustls 并补足 SOCKS4/4a 等连接器；ZIP64、图片缩放、Serde；Python 原版作为测试 oracle，Playwright 驱动冻结的原版 Vue WebUI。

**用户修订：** 配置读取改为简单的纯数据 `config.json` parser，优先直接复用 Core JSON/文件 I/O；取消 Python 配置解析器、AST 求值、动态执行桥和可执行 `config.rss`。Python 仅供原版测试 oracle/离线资料提取使用。Task 1–3 保留，未验收 Task4 已回滚到 `cfbdbe86abb2c6fbd0ee9417613541a15d55154e` 后重做；旧 Task4 实现及 address 合同作废。

---

## 1. 调研基线与证据

### 1.1 上游应用

- 用户给出的仓库 `fffonion/xehentai` 经 GitHub 解析为 `fffonion/xeHentai`。
- 默认分支：`master`。
- 本次读取的固定 revision：`43fc55c0c662f195d048becf6276a26ce06b09d9`。
- 提交时间：`2024-10-27T16:53:58Z`；`const.py` 中版本值为 `2.023`。
- 许可证：GPLv3；迁移源码、文案及兼容资源时保留署名和许可证。
- 已读取 README、CHANGELOG、CLI、配置、常量、core、task、worker、filters、proxy、RPC、updater、i18n 和入口代码；未执行上游应用或用户配置。
- 仓库树未见现成测试目录，因此“先补原版特征测试”是第一项交付，不能把 README 当唯一规范。

源码固定链接前缀：
`https://github.com/fffonion/xeHentai/blob/43fc55c0c662f195d048becf6276a26ce06b09d9/`

关键证据位置：
- `xeHentai/config.py:6-74`：25 个默认配置项。
- `xeHentai/const.py:79-122`：任务状态及错误码。
- `xeHentai/cli.py:26-102,108-256`：运行模式、参数、交互、退出。
- `xeHentai/core.py:106-162,164-359`：任务 API 与流水线。
- `xeHentai/core.py:390-473`：会话和登录。
- `xeHentai/task.py:120-221,223-440`：重复图片、落盘、命名、归档、序列化。
- `xeHentai/worker.py:34-138,140-255,284-438`：连接、重试、速度、监控与错误投票。
- `xeHentai/filters.py:15-268`：站点解析与错误判定。
- `xeHentai/proxy.py:22-194`：选择、失效、冷却、SOCKS/HTTP/Glype。
- `xeHentai/rpc.py:183-397,409-515`：HTTP/RPC/gallery 合同。
- `xeHentai/updater/github.py:17-35`、`updater/updater.py:21-63`、`xeH.py:14-42`：检查、下载、下次启动加载。

### 1.2 真实客户端基线

单独读取 `fffonion/xeHentai-webui`，默认分支 `master`，固定 revision：
`4e3738ef463fef344e96df613de9a81b4cda4b4e`。

证据链接前缀：
`https://github.com/fffonion/xeHentai-webui/blob/4e3738ef463fef344e96df613de9a81b4cda4b4e/`

- `src/assets/js/rpc.js:60-87`：真实请求为 `params: [args, kwargs]`，token 插入 `args` 的第一项；gallery 链接签名由客户端生成。
- `src/components/Gallery.vue:61-74,93-109`：`getImage(guid)`、单/双页阅读、点击翻页、加载错误处理。
- `src/components/DownloadList.vue`：直接拼接 `/img/...`、`/zip/...`，任务操作、标题和标签检索。
- `src/App.vue`：`getInfo/getConfig/updateConfig/addTask/listTasks`，含 `finished` 和 `!finished` 过滤。
- `src/components/Config.vue`：URL fragment 中的 `host,port,https,token` 和浏览器连接配置。

目标不要求重写 WebUI；优先冻结并复用现有前端资产，通过新服务端完成相同流程。必要的现代浏览器修复独立记录，不与协议迁移混在一起。

### 1.3 RustScript 本地能力基线

已只读检查 `/home/wow/rustscript/rustscript`：
- 分支 `master`，origin 指向 `rustscript-lang/rustscript`，远端默认分支 `master`。
- 本地 HEAD：`d9387f98df78dd88d2d1c04eebac4415f844efaf`。
- 当前存在用户的未跟踪 `.hermes/`，实施时保留，不清理或覆盖。
- `Cargo.toml`：库 crate 名为 `vm`，支持 `runtime/async/http-client` 等 feature。
- `docs/http-client.md` 与 `src/builtins/runtime/http/config.rs`：标准 HTTP 提供有界缓冲响应和 SSE；使用直接 Hyper HTTP/1，忽略环境代理，没有隐式 cookie jar；默认响应体上限 8 MiB。
- `src/builtins/runtime/host_modules.rs:151-173`：现有标准 host 模块中没有供本应用直接使用的 RPC HTTP 服务端、代理池和通用并行下载 API。
- `docs/host-sdk-descriptors.md` 和 `tests/fixtures/external-host-extension/src/lib.rs`：存在应用侧扩展、强类型资源和宿主操作接口。

上述为源码调研证据，尚未通过可运行 spike 证明本项目的完整嵌入路径。实施第一阶段必须验证 RSS 模块加载、typed host、跨线程事件传递和取消，不能将设计中的接口写成现成功能。

## 2. 兼容口径与边界

### 2.1 “一比一”的验收定义

每项功能必须具有：上游来源 → 原版输入/输出或状态轨迹 → RSS 目标模块 → 自动测试 → 验证证据。新增的正确性和安全处理必须列入差异账本，禁止通过减少验收范围来宣称全量完成。

比较公开语义，包括：
- CLI 选项、默认值、配置覆盖、退出/驻留模式。
- RPC 方法名、参数布局、HTTP 状态、错误码、结果字段、空值和过滤行为。
- 文件命名、任务状态、会话恢复、下载重试与代理切换。
- gallery 图片顺序、链接、缩略图、归档读取、前端连接。
- 更新通道、检查/下载开关、应用时点及失败保留行为。
- 三语消息、错误消息回退和占位符。

不以 Python 2、IronPython/PyPy 解释器本身为重写目标；保留它们原来提供的用户功能。保留 Linux/macOS/Windows 支持；daemon 按原版仅支持 POSIX。

### 2.2 已发现、必须先特征化的差异

以下源代码异常不能被悄悄忽略，也不能机械复制为新的安全缺陷：

1. README 的鉴权示例把 token 放在 `params` 外层；实际 Handler 与 WebUI 把 token 放在 `params[0][0]`。
2. `rpc.update_config` 只有 payload 包含 `proxy` 才调用 core 更新；部分不完整 payload 还可能触发 core 的必填索引异常。
3. `get_image(guid, request_range)` 对带逗号参数只使用第一项决定结束位置；不能擅自改为常规起止区间语义。
4. `jsonrpc_resp` 将假值 id（例如 `0`）写为 null；batch/notification 没有完整标准支持。
5. token 验证采用正则搜索，且部分日志可能打印敏感值。
6. Python `argparse(type=bool)` 对 `False` 字符串的行为与一般布尔解析不同；`--download-range` 默认值也不能直接依据 README 推断。
7. `Pool.has_available_proxies()` 的返回值与名称含义相反；默认选择首个可用代理，未实现轮询或随机均衡。
8. 重命名映射可能混用整型/字符串键；`scan_downloaded` 有比较表达式代替状态赋值；未完成索引遍历的末端也需测试。
9. GET 响应存在额外换行；非法 JSON 的代码有局部变量写错；Content-Length 相关行为需要实际抓取，不能仅按源码推测网络结果。
10. `.xehdone`、ZIP 的存在会影响恢复判定；原版归档可能在尚未完成时由 gallery 请求生成。

执行 Task 1 时建立 `docs/compatibility-deltas.md`：
- 可观察、无安全问题的旧行为：默认 compatibility codec 保留，改进放独立 opt-in 模式。
- 崩溃、损坏输出和安全缺陷：记录原版表现、拟定新行为、客户端影响及回归用例；确认差异后才纳入迁移合同。
- 尚未实测：明确标为 `unverified`，不能将静态阅读推断写成测试结论。
- 差异账本未关闭前，项目状态为“兼容迁移中”，不得标为无差异一比一。

### 2.3 用户配置：简单数据 parser

按用户最新指示，仅实现常规配置文件读取、类型检查和覆盖，不实现或模拟 Python 语言。配置输入格式变更明确记入兼容差异，取消“任意 config.py 源码兼容”的验收要求。

- 文件为 `config.json`，根必须是对象。支持现有 25 项配置所需的 string、bool、number（按字段支持 int/float）、null 和数组，不引入新配置 DSL。
- 优先复用 Core JSON decoder 和文件 I/O，先用真实 RSS compile/bind/run 证明接口。禁止自写 JSON 语法解析器、引入 rustpython-parser 或其他 Python 语法依赖。
- RSS 只做已知字段映射/校验、defaults overlay 和业务更新。使用固定配置对象及必要的字段存在性标记；不为任意 Python 对象搭建递归值框架、typed arena 或解释器。
- 入口可传显式路径；否则读当前目录 `config.json`。默认文件不存在时用内建默认；显式路径不存在、JSON 错误、根类型错误、未知键或字段类型错误都明确报错，不忽略坏文件。
- 缺失键沿用默认；显式 null、false、0、空字符串、空数组依各键原有类型和含义保留，不用 truthy 判断覆盖。
- 不加载或执行 `config.py`、`config.rss`，不引入 Python 解释器、可信执行开关、子进程管理或 const 表解释器。旧配置由用户把实际设置写成 JSON；提供字段对照与示例，不做自动执行旧代码的迁移工具。
- 保留 25 个默认值、配置优先级、任务覆盖、公开视图及代理/ignored_errors 的业务效果；配置源码执行语义不纳入重做。
- `ignored_errors` 按错误码集合合并，采用保留首次出现的确定性顺序；不移植 CPython hash/probe/resize 布局。与原版具体顺序的差异须记账，不靠排序比较器伪装一致。
- 文件 parser 的已知键策略不改变 RPC 方法自身的参数/过滤/额外字段合同；后者仍在 Task11 按原版请求独立验收。
- Python 仅作为原版 oracle/离线资源提取工具；这些测试工具沿用官方模块，发行物和新配置读取不依赖 Python。

### 2.4 规则引擎的实际范围

原版没有独立的用户规则 DSL。现有“规则”分布在 `filters.py`、`util.get_proxy_policy`、`Monitor._check_vote`、重试和文件策略中。

迁移为 RSS 内的 typed rule pipeline：
- 阶段：metadata → page-list / MPV → image-page → download-response → stream-complete → task-monitor。
- 输入：命名的上下文类型；输出：成功结果或 `Fail / RetryPage / ReloadImage / DisableProxy / MigrateSite / StopTask / Ignore` 决策。
- 默认规则逐项映射旧函数、错误码和先后次序。
- HTTP 状态、最终 URL、原请求 URL、Content-Length、页面文本及下载事件都保留来源，防止重定向后丢失重试上下文。
- 可选用户 RSS 规则入口只在明确配置时加载；没有配置时运行内建规则。自定义规则的类型校验、冲突顺序、失败回退写入独立测试，不引入额外 DSL 或图形编辑器。

## 3. 架构决策

### 3.1 方案取舍

- **推荐：RSS 应用层 + 应用自有 Rust host。** 可以保持业务真正迁移到 RSS，同时覆盖多线程、代理、ZIP、图片缩放和平台更新。
- 仅依赖标准 RSS CLI/HTTP：目前缺少必要系统能力，且缓冲全图无法满足原版流式下载和内存目标；不作为主路线。
- Rust 完成整个下载器、RSS 只调用 `download_gallery()`：难以体现 RustScript 重写目标，也无法独立验证规则迁移；不采用。

### 3.2 所有权和线程模型

1. 控制 VM 独占应用状态、任务表、配置版本和代理状态；通过类型化事件收集结果。
2. Rust 多线程运行时执行真实的并行网络和文件 I/O；`scan_thread_cnt`、`download_thread_cnt` 对应独立受限 worker 槽位，不能用串行循环模拟并发。
3. 必须执行 RSS 的扫描/规则 worker 各自在所属 OS 线程创建独立 VM，加载同一只读程序和独立 host state。不得把未证明为 Send/Sync 的 Vm、Value、资源句柄跨线程传递。
4. 跨线程只传命名 Rust DTO / 有界字节块 / 应用 ID；VM resource 由创建者拥有和关闭。
5. 控制 VM 下发带任务 epoch 的命令，worker 回报带相同 epoch 的事件。暂停/删除/退出之后，旧 epoch 的完成事件不能提交文件或增加完成计数。
6. 通道有界；HTTP body 直接写临时文件，不把完整图片载入 VM。背压、内存上界、取消和 draining 在 Task 3/9 实测。
7. 不自动放大 gallery 并发；先特征化上游跨任务流水线和归档重叠，保留公开行为；每个任务内部的扫描、下载必须真实并行。
8. file reader、rename 和 archiver 协调使用独立文件租约；网络读取期间不持有全局文件锁。

### 3.3 RSS 与 host 分工

RSS：
- 参数语义与配置优先级、任务状态机和策略。
- 站点解析结果组合、规则判定、重试/忽略/换站决策。
- 代理选择、失败计分、封禁冷却和恢复策略。
- RPC 方法分发、参数到领域命令的转换、公开结果视图。
- 更新模式/通道/版本判断和应用流程决策。
- i18n 键选择、回退和业务消息。

Rust host：
- 环境/信号、终端输入、daemon spawn、浏览器启动、文件原子操作。
- HTTP 客户端和 HTTP 服务端、连接器、TLS、DNS、流式 body。
- 工作线程、定时、时钟、事件通道和取消原语。
- JSON wire 字节边界、ZIP64、图片解码/缩放、平台文件替换；配置文件读取优先复用 Core I/O，不新增 Python 桥。
- 公开 host API 均为命名类型/显式可选字段；禁止原始 `map/unknown` host 合同。RPC 任意 JSON 只存在 wire codec 层，解析后立即转 typed DTO。

RustScript Core 在本项目实施中保持只读。公开 SDK 或现有 host function 经真实执行仍有能力缺口时，只在本项目 `crates/xeh-host` 中新增最小 host function；不得因此修改 Core、建立 Core 修改前置项，或通过切换依赖引入专为本应用添加的 Core 接口。只有用户另行明确授权，才能改变这一边界。

### 3.4 Core host function 优先复用

- **Core 已提供且能够满足需求的 host function，必须直接复用，不在应用侧重复实现或新增等价 host function。**
- 实施前按锁定的 Core revision 核对实际 catalog、源码、feature gate 和调用合同，通过真实 RSS compile/bind/run 验证可用性；不可凭旧文档或函数名称判定缺失。
- **强制优先顺序：直接调用锁定 revision 中已有且可用的 Core host function → 用 RSS 组合这些现有函数 → 仅对真实执行证实仍无法覆盖的部分，在本项目 `crates/xeh-host` 增加最小、命名且强类型的 host function。** 不得跳过前两步，也不得把应用缺口转成 Core 修改任务。
- 应用侧适配优先复用公开 SDK、现有 descriptor/adapter、异步 bridge、资源和权限机制，只补缺失的接口行为；禁止复制 Core 实现、自建等价解析器或为单个缺口替换整套现有能力。
- 第 3.3 节中的 Rust host 职责及第 5 节的目标文件仅表示能力归属和候选组织方式，不代表必须自建；已有 Core 能力覆盖时，删除对应自建模块计划并复用 Core。
- 每个新增应用侧 host function 必须在 `docs/architecture.md` 记录对应需求、已检查的 Core 接口、不能满足需求的具体证据、扩展边界及测试；缺少能力缺口证据不得加入。
- 对 JSON、正则、文件 I/O、定时器、HTTP 等能力逐项检查，不因某一高级场景缺少支持就整套替换 Core 已可用功能；共享底层能力，扩展限制在缺失部分。

## 4. 全功能迁移清单

### 4.1 配置与 CLI

必须保留的 25 个配置键及默认值：

```text
daemon=false
dir="."
download_ori=false
jpn_title=true
rename_ori=false
proxy=[]
proxy_image=true
proxy_image_only=false
rpc_interface="localhost"
rpc_port=null
rpc_secret=null
rpc_open_browser=true
make_archive=false
download_range=null
scan_thread_cnt=1
download_thread_cnt=5
download_timeout=10
ignored_errors=[]
log_path="eh.log"
log_verbose=2
save_tasks=false
delete_task_files=false
low_speed_threshold=10
auto_update="download"
update_beta_channel=false
```

- 基础优先级：内建默认 < 用户配置 < CLI < 交互覆盖；任务创建时保存允许覆盖项的快照。
- RPC 配置更新保留原有隐藏项：`rpc_*` 与 `urls` 不在公开 get/update 视图中。
- `ignored_errors` 的错误码集合合并与用户配置覆盖分别测试；输出顺序依第2.3节确定性策略，原版顺序差异单列，不仿真 CPython 容器。
- 原 CLI 别名：`-u/--username`、`-k/--key`、`-c/--cookie`、`-i/--interactive`、`--daemon`、`-d/--dir`、`-o/--download-ori`、`-j/--jpn-title`、`-r/--rename-ori`、`-p/--proxy`、`--proxy-image`、`--proxy-image-only`、四个 RPC 选项、`--delete-task-files`、`-a/--archive`、`--download-range`、`-t/--thread`、`--timeout`、`--low-speed-threshold`、`-f/--force`、`--auto-update`、`--update-beta-channel`、`-l/--logpath`、`-v/--verbose`、`-h/--help`、`--version` 和多个 URL。
- 交互提示顺序、默认值、逗号多 URL、下载范围、daemon 优先于 interactive。
- 有 URL 时处理队列后退出；无 URL 时继续驻留；POSIX daemon、信号退出、日志 flush 和会话保存。
- 日志等级、文件/终端双输出、UTF-8、平台换行、终端标题、原版 GMT+8 时间语义纳入特征测试。

### 4.2 任务与会话

公开状态数值保留：
`paused=0, waiting=1, get_meta=2, scan_page=3, scan_img=4, scan_archive=5, download=10, make_archive=19, finished=20, failed=-1`。

- 保留 GUID 的 8 字符 UUID 前缀形式和已有会话 GUID；不依据 URL 去重——原版创建 GUID 的方式并未提供 URL 去重保证。
- 新增/删除/暂停/恢复及各状态的合法性和错误码。
- `h.json` 顶层 `tasks/cookies`，Task 的 `url/gid/sethash/guid/config/meta/failcode/state/reload_map/duplicate_map/renamed_map/page_q/img_q` 读写语义。
- JSON 对象键变为字符串、queue 为 null/空/有数据、map/set 的实际序列化异常逐项采样。
- 默认 `save_tasks=false` 仍保存 cookies；不能误改为完全不写会话。
- 自动保存、启动恢复、队列恢复及在途下载重新扫描；保留 `.ehentai.cookie` 旧格式导入。
- 同输入会话读取后保留公开字段；内部 schema 独立 version，不污染旧 RPC 结果。新增内部数据用独立 sidecar，旧 `h.json` 保持兼容。
- 新写入使用同文件系统临时文件和原子替换；损坏会话应保留原文件并清楚报错。

### 4.3 站点、解析与规则

- e-hentai/exhentai URL 识别、登录可用性、必要时从表站迁移到里站。
- 用户名/密码登录、Cookie 设置、默认 `nw=1`、`ipb_member_id/ipb_pass_hash` 登录判定、session cookie 更新。
- 元数据：日文/英文标题、总页数、tags、每页 thumbnail 数和 MPV 开关。
- 普通 gallery 分页和 MPV `imagelist` 生成单图页队列。
- 原图/普通图选择、原图链接缺失时回退、重定向后的原文件名。
- `nl` reload URL、过期 key、resampled 页面和重新解析。
- 移除/仅里站可见/IP ban/509/配额页面/小型文本错误页/404/错误大小/流中断/regex 失败。
- URL 中声明长度、响应头长度与实际流字节数的不同校验阶段。
- ignored_errors 与 `--force`、监控投票和停止条件。
- 保留公开错误码，包括当前不活跃的错误码；注释掉的 hathdl 扫描和 `reset_quota: pass` 不凭空认定为可运行功能，也不新增虚假成功路径。

### 4.4 下载、命名与归档

- 扫描和下载独立 worker 数；页面扫描可继续生产图片下载任务。
- 默认请求重试与 worker 重试、超时、重定向上限 30、退避时序均由基线固定。
- 流式临时文件、写入完成后提交、取消清理、不把短文件当已完成。
- URL 内既有图片标识用于重复图片判定；已完成直接复制，未完成等待原图结果，完成计数只增加一次。
- 下载范围含中文逗号、多个闭区间和单页；越界/重叠/反向边界按原版实际行为记录。
- 页数位数决定补零；下载阶段临时序号名、最终真实扩展名、原始命名与冲突 `(1)/(2)`、`-xeh-conflict-` 和 `.xehdone`。
- Windows 路径字符/末尾点和空白、POSIX 字符处理、Unicode 和跨文件系统移动。
- 五样本低速窗口、KB/s 单位、低速取消后重新扫描/换源；扫描/下载 worker 心跳、zombie、汇总速度、无进展退出。
- ZIP 使用 STORE、ZIP64、排序条目、UTF-8 注释，完成后可删除目录；RPC 临时取 ZIP 不删除目录。
- 源码中的监控 tick 与计数可能不等于注释声称的秒数；用注入时钟验证真实阈值，不能按注释改写。

### 4.5 代理池

- HTTP、HTTPS、SOCKS4、SOCKS4a、SOCKS5、SOCKS5h；逐种证明连接及 DNS 解析位置。
- Glype URL 编码、b 参数、Referer、站点 cookie 映射、sslagree、会话 cookie、文本中重写链接的逆变换。
- 默认使用第一个可用代理；错误累计、MAX_FAIL=5、永久禁用和 IP ban 到期恢复。
- 每次重定向按 URL 重新决定网页/图片代理策略；`proxy_image_only` 优先于 `proxy_image`。
- 池空时的错误、正在切换时的旧请求、动态增加/移除/清空代理、统计和 cookie 隔离。
- 原版直连固定地址连接、Host/SNI/TLS 域名校验保留能力；旧固定 IP 不保证仍可用，默认值与允许覆盖策略记录清楚，测试使用本地替身。
- 不默认添加轮询、健康探测或自动直连回退；这些会改变隐私和流量路径。

### 4.6 RPC 与 gallery

显式实现下列 13 个源代码公开方法对应的 wire 名称，逐个测试签名及返回形状：

```text
xeH.updateConfig(**cfg)
xeH.addTask(url, **task_cfg)
xeH.delTask(guid)
xeH.pauseTask(guid)
xeH.resumeTask(guid)
xeH.saveSession()
xeH.loadSession()
xeH.loginExhentai(name, pwd)
xeH.setCookie(cookie)
xeH.getInfo()
xeH.getConfig()
xeH.listTasks(level="download")
xeH.getImage(guid, request_range=null)
```

wire 示例（与真实 WebUI 一致）：

```json
{"jsonrpc":"2.0","id":"case-add","method":"xeH.addTask","params":[["token:example","https://e-hentai.org/g/123456/0123456789/"],{}]}
```

此 URL 只作协议形状示例，不作联网夹具或真实 gallery 数据。

- `/jsonrpc` 的 POST；OPTIONS/CORS、Content-Type、HTTP 状态、错误消息本地化。
- 无 params、位置/关键字参数、有/无密钥、冗余 token、非法方法、私有方法、缺失 id、非法 JSON、异常返回。
- 不继承 Python 反射的任意属性暴露；用显式方法表覆盖原功能。
- `getInfo`：`version, threads_zombie, threads_running, queue_pending, queue_finished, download_speed`；版本标明新版实现，兼容前端的最低版本判断，不伪装 Python 运行时。
- `listTasks`：各 state 名称、`!level` 取反、默认 download、非法 level；保留元数据和配置视图，移除原版已隐藏的内部字段。
- `getImage`：页序、相对 URI、可选尾部文件名、range 的历史行为。
- `/img/{sign}/{guid}/{fid}[/{filename}]`：目录图片/ZIP 内图片两条读取路径，MIME 和重命名映射。
- 签名算法按原 `hash_link` 定义兼容：对 `secret + "-xehentai-" + uri` 的 UTF-8 字节使用 MD5，取十六进制前 8 字符；图片 uri 为 `guid/fid`，ZIP uri 为 `guid`。此处仅记录源代码合同，本次调研不计算签名。
- `?w=&h=` 缩略图：仅 w、仅 h、两者、保持比例、不放大、保留原格式；用真实图片夹具验证，不凭 API 名推定等价。
- `/zip/{sign}/{guid}/{filename}`：现有 ZIP 或按需归档，鉴权失败/不存在/并发归档。
- `/ui/` 与 `/static/*`：原版前端、1 小时缓存、If-Modified-Since/304、更新首页时的资源失效、远端不可用时旧缓存；新缓存用安全显式格式，不反序列化旧 pickle。
- 打包本地原版静态资产，远端 UI 主机可替换；保留请求路径和浏览器连接 fragment。旧 `webui.gz` 缓存可失效后重建，不承诺 Python pickle 二进制兼容。
- 归档、rename 和 gallery read 并行时，不能返回半张图片、误删租约中的文件或形成死锁。

### 4.7 自动更新

- 原版检查的是 GitHub `master/dev` 最新 commit，不能误写为仅检查 GitHub Releases。
- 模式 `check/download/off`，正式/测试通道、当前 update_id 比较、后台检查和失败消息。
- 原版下载 Python `src.zip` 并在下次启动注入；RSS 重写保留“检查 → 下载待用 → 下次启动应用”的产品语义。
- 新版更新源必须是新版项目的可信发布源，不能下载 Python 上游包并报告更新成功。
- 将脚本包与宿主二进制更新分开；manifest 写明目标平台、host ABI、RSS 编译兼容版本、应用版本、资源清单和签名元数据。
- 更新先下载 staging，再校验来源、签名、归档路径及兼容版本；启动时原子切换版本目录，失败保留旧版本。安全校验属于明确记录的新增保障。
- Windows 文件占用由独立 launcher/helper 完成替换，POSIX 同文件系统原子切换；不在当前下载任务中途替换已加载的脚本。
- 离线、超时、限流、损坏包、降级、不兼容 ABI、权限失败、进程中断和启动失败回退均需测试。
- 真正发布仓库与签名密钥在实施发布阶段绑定；此前使用本地更新源完成全部可重复测试，绝不向生产用户推送测试包。

### 4.8 i18n

- `en_us/zh_hans/zh_hant`。
- locale 映射：`zh_CN/zh_SG → zh_hans`，`zh_TW/zh_HK/zh_MO → zh_hant`；无 locale、未知 locale 回退英语。
- 三个源码语言模块各有 98 个顶层赋值；迁移时以 AST 得到的实际 key 集合逐项验证，`err_msg` 内的错误码映射单独展开。
- 每个语言缺 key 时回退英语；英语缺错误码时使用 `ERR_NOMSG` 模板。
- `%s/%d`、format 占位符、帮助、交互、日志、RPC error 文案统一验证。
- 文案直接迁移原版资源，新增安全/兼容错误使用独立 key，不能覆盖旧 key。

## 5. 目标项目布局

实施项目建议位置：`/home/wow/rustscript/xehentai-rss`。当前不创建项目、不修改兄弟仓库。以下全部路径相对于该目标项目。

```text
Cargo.toml
Cargo.lock
LICENSE
README.md
crates/xeh-host/src/main.rs
crates/xeh-host/src/runtime.rs
crates/xeh-host/src/catalog.rs
crates/xeh-host/src/types.rs
crates/xeh-host/src/net/{mod,client,connectors,dns,stream,server}.rs
crates/xeh-host/src/os/{mod,fs,signals,daemon,browser}.rs
crates/xeh-host/src/worker/{mod,pool,events,cancellation}.rs
crates/xeh-host/src/media/{mod,archive,thumbnail}.rs
crates/xeh-host/src/update/{mod,package,launcher}.rs
app/main.rss
app/types.rss
app/config/{types,defaults,load,merge,view,snapshot}.rss
config.example.json
app/cli/{parse,interactive,lifecycle}.rss
app/domain/{task,state,queue,session,naming,archive}.rss
app/site/{metadata,pages,mpv,image,login}.rss
app/rules/{engine,metadata,page,image,download,monitor}.rss
app/download/{scheduler,retry,monitor,speed,duplicates}.rss
app/proxy/{pool,policy,glype}.rss
app/rpc/{dispatch,codec,methods,views,gallery,static}.rss
app/update/{check,channels,apply}.rss
app/i18n/{catalog,locale,format}.rss
locales/{en_us,zh_hans,zh_hant}.json
assets/webui/
compat/oracle/{loader,harness,config_runner}.py
compat/manifest.json
compat/fixtures/{http,rpc,session,config,media,updates}/
compat/golden/
crates/xeh-host/tests/{rss_smoke,host_contract,transport,concurrency,gallery,update}.rs
crates/xeh-xtask/src/{main,oracle,diff,rss_test,fixtures,verify}.rs
tests/rss/{config,state,session,site,rules,proxy,download,rpc,i18n,update}/
tests/e2e/{cli,webui,signals,migration}/
docs/{compatibility,compatibility-deltas,architecture,configuration,release}.md
.github/workflows/{test,release}.yml
```

避免创建一个全功能 `core.rss` 或把 Python 文件机械改后缀。领域状态、规则、协议视图分别有唯一拥有者。

## 6. 测试工具与统一执行约定

`xeh-xtask` 是本计划要求实施的测试工具，当前尚不存在。所有下列命令均为实施后的验收命令，未在本次规划中运行。

```bash
# 锁定上游、准备脱敏夹具、只跑本地 fixture HTTP/proxy/update server
cargo run -p xeh-xtask -- oracle capture --suite all

# 编译并执行 RSS 测试，0 个用例必须判定失败
cargo run -p xeh-xtask -- rss-test --suite config

# 同一输入分别交给固定 Python 原版与新应用
cargo run -p xeh-xtask -- diff --suite rpc

# 最终检查 manifest 中每一项均有通过证据及关闭的差异
cargo run -p xeh-xtask -- verify --all --deny-unverified
```

测试不依赖真实成人站点、账户或公网代理；HTTP 夹具来自脱敏记录和本地构造协议场景。构造场景必须标为 synthetic，并另有可追溯的原版 oracle 输出，不能把构造输出冒充抓取结果。

原版加载必须把工作目录、FILEPATH、HOME 和日志/会话目录定向到临时夹具根；mock 自动更新和远端 UI 拉取，禁止导入任何用户 config.py、h.json、cookie。原版未能运行的场景保留失败记录，使用静态分析暂列缺证，不能伪造 golden。

每个实施任务拆为独立测试场景，采用相同步骤：
1. 新增一个最小失败用例与源代码位置；验证确实失败且执行数大于零。
2. 编译/执行对应 RSS 或 host 最小实现；不得只测 Rust DTO。
3. 跑聚焦用例及该模块全套，记录命令、结果与未关闭差异。
4. 范围审查后提交明确路径；禁止顺带修改 Core 或前端业务。提交仅是未来实施动作。

## 7. 实施任务与依赖

### Task 1：建立兼容 manifest 和行为 oracle

**目标：** 把“全部功能”转换为不可跳过的机器可验收清单。

**文件：** `compat/manifest.json`、`compat/oracle/loader.py`、`compat/oracle/harness.py`、`compat/fixtures/**`、`compat/golden/**`、`docs/compatibility-deltas.md`、`crates/xeh-xtask/src/{main,oracle,fixtures}.rs`。

**步骤：**
- 固定本文两个上游 revision，记录源码归属及 GPL 信息；通过 archive/API 读取，避免默认 SSH 身份问题。
- 从 AST 枚举配置键、公开方法、state/error 常量、语言 key 和入口参数。
- 为每项记录 `id/source/inputs/expected/target/tests/status`；RPC 与 gallery 分开编号。
- 先写 `manifest_rejects_unmapped_feature` 和 `oracle_never_reads_user_home` 失败测试，再实现检查器和临时根隔离。
- 从纯工具函数开始运行原版，逐步加入本地 HTTP、任务/会话和 RPC；冻结随机数、UUID、时钟或显式标记允许变化字段。
- 将第 2.2 节每条异常变为特征测试和差异条目；不扩大标准支持范围来覆盖原版缺失功能。

**验收：** `cargo run -p xeh-xtask -- oracle capture --suite all`；原版运行错误单列；所有功能都有测试归属；没有凭空生成的 oracle 输出。

### Task 2：可运行 RSS 嵌入骨架与 typed host spike

**依赖：** Task 1 的基线定义。

**文件：** `Cargo.toml`、`crates/xeh-host/src/{main,runtime,catalog,types}.rs`、`app/{main,types}.rss`、`crates/xeh-host/tests/{rss_smoke,host_contract}.rs`。

**步骤：**
- 锁定 pd-vm revision，开发与发布均使用可复现依赖；兄弟 Core 仓库仅供只读核查 SDK，不修改或复制 Core 源码。能力补充遵循第 3.4 节，在本项目 host 层完成。
- 编写 RSS 跨模块 named type、typed host 调用、可选字段、guest callable 真实 compile/bind/run 用例。
- 验证正常退出、host 失败、取消、资源 close 和 VM 重用；编译与执行使用同一 catalog。
- 建立应用最小入口和 host descriptor 清单；先 interpreter，JIT 后续独立验证。
- 建立 Core host function 复用清单，记录每项需求的现成接口、真实 RSS 验证结果和剩余缺口；新增 descriptor 必须符合第 3.4 节。

**验收：** `cargo test -p xeh-host --test rss_smoke --test host_contract`；必须执行真实 RSS，schema 失败在启动阶段清楚报告。

### Task 3：线程、事件、流式传输的能力关卡

**依赖：** Task 2。

**文件：** `crates/xeh-host/src/worker/{mod,pool,events,cancellation}.rs`、`crates/xeh-host/src/net/{mod,client,stream}.rs`、`crates/xeh-host/tests/{transport,concurrency}.rs`。

**步骤：**
- 先测两个请求被本地 server barrier 同时阻塞，以证明并发；禁止只比较可能波动的总耗时。
- 每线程创建 VM，传 typed DTO；测试错误 VM 所属资源不能在另一 VM 使用。
- 将 binary body 分块写入目标同文件系统 staging，回报有界进度事件。
- 先写暂停/取消与完成同时发生、通道满、消费者退出、socket stall、磁盘写失败用例，再补清理实现。
- 测大于 Core 默认缓冲上限的合法图片仍能流式完成，且 VM 未持有全图。

**验收：** `cargo test -p xeh-host --test concurrency --test transport`；并发数、队列界限、取消后资源计数有断言。关卡失败时先按第 3.4 节验证现有 Core 能力及 RSS 组合；剩余真实缺口只在本项目 host 层补齐，不修改 Core，不能开始大量 RSS 迁移掩盖问题。

### Task 4：简单 JSON 配置读取、覆盖与业务视图（重做）

**依赖：** Task1、2。

**文件：** `app/config/{types,defaults,load,merge,view,snapshot}.rss`、`config.example.json`、`crates/xeh-host/tests/config.rs`、`compat/fixtures/config/**`、`docs/configuration.md`；按需修改既有 xtask config suite 和测试专用 `compat/oracle/config_runner.py`。不创建 config_bridge/process_bridge/config_arena/legacy_const，不改 Core 或 Task3 worker/net。

**步骤：**
1. 在 lease TEMP 做最小 Core JSON + 文件读取 spike，真实读 fixture 并执行 RSS。仅保留有复用价值的集成测试，一次性 probe 不写入 app/src/tests。实测缺口须回报，不用另一套 parser 绕过。
2. 写 25 个默认值及固定配置对象测试，包含 null/空数组区别；只加必要的键存在性表示，不建通用递归 schema/value graph。
3. 用有效配置、空对象、部分覆盖先 RED，再实现 load → decode → 逐字段校验 → defaults overlay。覆盖 false/0/空串/空数组和合法 null，拒绝错误类型/未知键/坏 JSON/错误根类型。
4. 在两个独立 fixture 根下分别测试默认文件缺失、显式文件缺失、有效文件和坏文件；suite 自行创建输入，不依赖先跑其他测试。目录/路径从入口传入，不写入源文件。
5. 测配置优先级、ignored_errors 去重、代理配置字段更新、task 允许项的纯字段投影、getConfig 的 rpc_*/urls 过滤。Task4不声称已验证真实Task对象的快照；该状态测试是Task5首项。代理池实际副作用在Task8验证。业务在RSS，不在此实现完整RPC server、CLI argv或Python容器内部算法。
6. 路径转换仅放原版实际阶段：目录创建能力由Task10的os/fs owner承接并真实验证缺失/已存在/失败路径，完成前不开放公开配置更新；CLI abspath和参数处理留Task13，不提前把所有相对路径改绝对路径。
7. 原版差分使用同份数据，在固定上游临时实例注入配置键并执行原函数；新实现读取JSON。Task4可将RSS纯task_projection与原版真实Task配置字段比较，但不得用同一个before值投影两次来证明状态独立。真正创建Task→更新同app→读取原Task的完整状态证据交Task5首项；RPC公开合同由Task11实际调用原方法验收。
8. 配置源格式和 ignored_errors 顺序作明确差异单独验收，其余字段严格比较；保留原始输出，不做全局排序/去 null。更新 manifest 范围说明，不删除未完成项来让整产品通过。
9. 提供简短config.example.json和字段文档；发行物无Python运行时或运行期Python脚本。数组校验使用迭代而非每元素递归，并通过真实入口验证超过Core默认调用深度的合法数组与末项类型错误；不升栈上限或引通用schema。

**验收：** `cargo test -p xeh-host --test config`、真实 `rss-test --suite config` / `diff --suite config`，以及相关既有测试、fmt/strict Clippy。故意改一个默认值应被差分拒绝；提交后准确 HEAD 记录命令和真实退出码。两个全新 fixture 根可分别独立执行 config suite；Task1–3 回归不变，whole-app verify 继续 fail-closed。不要求旧 AST/动态桥/CPython 布局测试，不为已删除目标增补实现。

### Task 5：任务状态机和公开 API

**依赖：** Task 2、4。

**文件：** `app/domain/{task,state,queue}.rss`、`tests/rss/state/**`。

**步骤：**
- 首项先实现真正Task记录及配置快照回归：在同一控制app创建Task，随后更新global config，再读取原Task；对照固定上游实际对象，包含14个基准键、任务覆盖及额外task字段。纯字段投影测试不替代该生命周期证明。
- 逐个 state 转换和错误码测试，覆盖 add/del/pause/resume。
- 增加GUID注入、未知任务、运行中禁止删除等行为；配置快照沿用本Task首项真实对象测试。
- 给异步命令引入 epoch；暂停与旧成功回报竞态不得重开任务。
- 区分逻辑任务状态与在途 worker 生命周期；公开数字保持原值。

**验收：** `cargo run -p xeh-xtask -- rss-test --suite state` 与 `diff --suite task-api`。

### Task 6：会话、cookies 和旧数据迁移

**依赖：** Task 5。

**文件：** `app/domain/session.rss`、`app/site/login.rss`、`crates/xeh-host/src/os/fs.rs`、`tests/rss/session/**`、`compat/fixtures/session/**`。

**步骤：**
- 先测 save_tasks=false 仅持久化 cookies，再测任务和队列恢复。
- 覆盖数字字符串键、renamed_map、duplicate_map、部分下载、损坏 JSON、旧 cookie。
- 应用原子写入和目录权限，测试写一半退出不会破坏最近可用会话。
- 实际从原版输出启动新版，下载剩余页，再读回公开数据；禁止只测 parse/serialize 自循环。

**验收：** `cargo run -p xeh-xtask -- diff --suite session` 与 `rss-test --suite session`。

### Task 7：站点解析和 RSS 规则内核

**依赖：** Task 1、2、4、5。

**文件：** `app/site/{metadata,pages,mpv,image,login}.rss`、`app/rules/{engine,metadata,page,image,download}.rss`、`tests/rss/{site,rules}/**`。

**步骤：**
- 原版每个 filter 先对应成功/失败夹具，再迁移元数据、普通分页、MPV、图片页。
- 区分原 URL、跳转 URL、最终 URL；保留 reload 与原文件名来源。
- 将配额、大小、状态码和文本判断写为有序 RSS rule 决策；验证规则不会重复触发成功/失败回调。
- 测可选原图缺失回退、非图片文本页、ban 时长、错页结构及英语回退错误。
- 如启用自定义规则入口，测试加载失败、类型错误、先后顺序及默认规则不变。

**验收：** `cargo run -p xeh-xtask -- diff --suite site`、`rss-test --suite rules`；所有业务判定路径都执行 RSS。

### Task 8：代理传输和 RSS 池策略

**依赖：** Task 3、4、7。

**文件：** `crates/xeh-host/src/net/{connectors,dns,client}.rs`、`app/proxy/{pool,policy,glype}.rss`、`tests/rss/proxy/**`、`crates/xeh-host/tests/transport.rs`。

**步骤：**
- 承接Task4仅字段对照留下的代理池副作用：原版oracle记录真实pool状态，新实现经实际pool操作比对；仅proxy配置JSON相同不能证明替换/追加/失效行为。
- 为每种代理创建本地协议 server，断言实际经过代理及 DNS 端点；HTTP CONNECT、HTTPS 代理的 TLS 分层分别测试。
- 默认先首个可用，再错误计分、阈值、永久失效、封禁到期、池空处理。
- 页面/图片路由含重定向；防止隐式直连及全局环境代理影响。
- Glype 建立完整 fake server：编码、cookie/Referer、sslagree、返回文本链接和非文本内容。
- 验证配置切换期间在途请求的配置快照和资源释放；旧版异常保存在差异账本。

**验收：** `cargo run -p xeh-xtask -- diff --suite proxy`，`cargo test -p xeh-host --test transport`；协议种类缺一项均未达成全量代理兼容。

### Task 9：下载调度、重试、低速和监控

**依赖：** Task 3、5、7、8。

**文件：** `app/download/{scheduler,retry,monitor,speed,duplicates}.rss`、`app/rules/monitor.rss`、`tests/rss/download/**`、`crates/xeh-host/tests/concurrency.rs`。

**步骤：**
- 分开实现 metadata/list/scan/download 队列与 worker 槽位，保持旧公开 state。
- 重试先测连接错误、403、404、509、错误长度、断流、低速、reload 失效，再实现决策。
- 注入单调时钟测试速度窗口、zombie、无进展和错误投票，不使用漫长 sleep。
- 测重复图片先完成/后完成、同页重复回报、范围外页计数、最后一页。
- 连续暂停/恢复/退出验证无双提交、无多计数、无漏回收。

**验收：** `cargo run -p xeh-xtask -- rss-test --suite download`、`diff --suite download`、`cargo test -p xeh-host --test concurrency`。

### Task 10：文件命名、恢复扫描和 ZIP64

**依赖：** Task 6、9。

**文件：** `app/domain/{naming,archive}.rss`、`crates/xeh-host/src/os/fs.rs`、`crates/xeh-host/src/media/{mod,archive}.rs`、`tests/rss/download/naming*`、`crates/xeh-host/tests/gallery.rs`。

**步骤：**
- 承接Task4目录能力：由crates/xeh-host/src/os/fs.rs owner按Core优先规则实现最小必要能力，真实覆盖目录创建、已存在目录和失败。完成前Task11不得开放配置更新；差分须观测文件系统，不能只比较路径字段。
- 从序号临时名到原文件名/扩展名，测试千页、Unicode、冲突互换和中途退出。
- 测目录已有零字节/非零文件、.xehdone、已有 ZIP 和跨文件系统移动。
- ZIP64、STORE、条目排序、注释和删除目录；大文件测试用流式构造夹具，避免将全部字节放内存。
- 归档失败不得删除原图；gallery 持有读租约时禁止危险清理。

**验收：** `cargo run -p xeh-xtask -- diff --suite files` 与 `cargo test -p xeh-host --test gallery`；ZIP 比较解包后的文件字节和合同元数据，压缩容器非合同时间戳可归一化。

### Task 11：JSON-RPC 全方法兼容

**依赖：** Task 4、5、6、9、10。

**文件：** `crates/xeh-host/src/net/server.rs`、`app/rpc/{dispatch,codec,methods,views}.rss`、`tests/rss/rpc/**`、`compat/fixtures/rpc/**`。

**步骤：**
- 先实现真实HTTP server → typed RPC envelope → RSS dispatch → wire result闭环；目录能力须先由Task10完成，代理池行为须已由Task8验证，不能将Task4字段对照当副作用完成。
- updateConfig独立对照rpc_*/urls过滤、缺proxy的no-op、含proxy但缺dir的原异常或经批准差异。内部typed helper支持部分更新不代表公开异常差异已获接受。开放可变长ignored_errors更新前，使用现成Core查找结构保序去重或明确测试域预算，避免直接暴露内部O(n²)扫描。
- 为本计划列出的 13 个方法各写成功/错误/缺参数/鉴权用例，不能用大而化之的单个 smoke 替代。
- 逐项比较 HTTP 状态、error/result、id、null、任务字段、state 名称和 !level。
- 默认只实现原版支持面，batch/notification 的任何扩展单独 version；敏感日志修复显式记录。
- malformed body、超大 body、私有方法、路径误匹配、跨域预检和断开的客户端单独测试。

**验收：** `cargo run -p xeh-xtask -- diff --suite rpc`；13 个方法全部有证据；历史异常都在差异账本有决定。

### Task 12：Gallery、缩略图、读 ZIP 和静态缓存

**依赖：** Task 10、11。

**文件：** `app/rpc/{gallery,static}.rss`、`crates/xeh-host/src/media/thumbnail.rs`、`assets/webui/**`、`crates/xeh-host/tests/gallery.rs`、`tests/e2e/webui/**`。

**步骤：**
- 保留旧链接签名合同和相对路径；使用预先审核的兼容夹具验证，不依赖真实用户 secret。
- 测目录读图、重命名后读图、归档后 ZIP 内读图、按需 ZIP、不存在项、错误签名。
- 缩略图用 jpg/png/gif/webp/bmp 夹具测尺寸、比例、输出格式和异常，不擅自承诺编码器字节相同；需要像素/字节兼容的差异逐一记录。
- UI 缓存 fresh/304/expired/remote failure、首页变化后资产失效、stale fallback 和应用重启缓存。
- 高并发读图与 rename/archive/delete 同时运行，断言不死锁、不越界读、不会暴露任务目录外文件。

**验收：** `cargo test -p xeh-host --test gallery`；`cargo run -p xeh-xtask -- diff --suite gallery`；真实冻结 WebUI 完成打开 gallery、双页/单页、翻页、缩略图和 ZIP 下载。

### Task 13：三语和完整 CLI/交互/daemon

**依赖：** Task 4、6、9、11。

**文件：** `app/i18n/{catalog,locale,format}.rss`、`locales/*.json`、`app/cli/{parse,interactive,lifecycle}.rss`、`crates/xeh-host/src/os/{signals,daemon,browser}.rs`、`tests/rss/i18n/**`、`tests/e2e/{cli,signals}/**`。

**步骤：**
- AST 提取语言 key 和占位符；逐语种对照帮助、错误码消息和缺 key fallback。
- CLI 全参数、别名、多 URL、范围、错误退出、bool 历史语义和路径解析。
- pty 驱动交互，校验提示顺序、默认值和命令覆盖。
- POSIX 后台运行、daemon/interactive 组合、Windows 不支持提示、打开浏览器和不打开浏览器。
- SIGINT/SIGTERM、二次中断、空队列驻留、有 URL 自动退出、日志 flush 和 session 保存。

**验收：** `cargo run -p xeh-xtask -- diff --suite cli`、`rss-test --suite i18n`；平台 E2E 测试进程退出后没有残留子进程或未关闭句柄。

### Task 14：自动更新检查、下载与启动应用

**依赖：** Task 2、4、6、13。

**文件：** `app/update/{check,channels,apply}.rss`、`crates/xeh-host/src/update/{mod,package,launcher}.rs`、`compat/fixtures/updates/**`、`crates/xeh-host/tests/update.rs`。

**步骤：**
- 先测试正式/测试通道、check/download/off、已有 update_id 和无更新。
- 本地更新 server 返回匹配/不匹配 ABI、平台、版本及签名场景；不访问生产发布源。
- 测下载 staging、中途退出、路径穿越 ZIP、不完整包、权限不足、应用失败后回退。
- 创建两个真实可执行版本，下载新包后当前任务继续旧版；重新启动确认新版生效、旧 h.json 可读。
- Windows helper 和 POSIX 切换分别在相应 CI 平台实测；更新失败不报告成功。

**验收：** `cargo test -p xeh-host --test update`、`cargo run -p xeh-xtask -- rss-test --suite update`；必须发生真实跨启动版本切换，mock metadata 不算完成。

### Task 15：冻结 WebUI 全流程回归

**依赖：** Task 11、12、13。

**文件：** `tests/e2e/webui/**`、`assets/webui/**`、`docs/compatibility.md`。

**步骤：**
- 冻结原版资产和锁定依赖；旧 Node 工具链若无法在现代环境构建，使用隔离构建容器或可验证的冻结产物，不重写前端来回避兼容问题。
- 浏览器执行：连接/有无 token → 添加多任务 → 任务级配置 → 暂停/恢复/删除 → 已完成/未完成列表 → 标题/标签检索。
- 执行缩略图、gallery 单/双页、最后一页、ZIP、设置代理/ignored_errors、速度显示、三语。
- 记录实际 HTTP trace、截图和失败用例；如旧浏览器 API 需修补，单列最小差异并复测。

**验收：** `cargo run -p xeh-xtask -- e2e --suite webui`；禁止以 curl 全通过代替浏览器流程。

### Task 16：打包、平台 CI 与发布合同

**依赖：** Task 14、15。

**文件：** `.github/workflows/{test,release}.yml`、`Cargo.lock`、`README.md`、`docs/{architecture,configuration,release}.md`、`LICENSE`。

**步骤：**
- Linux/macOS/Windows 平台测试；SOCKS、Glype、ZIP64、Unicode 路径、文件占用和更新均保留覆盖。
- 打包宿主、RSS bundle、语言资源、冻结 WebUI、配置示例、许可证；检查发行版离线可启动 UI。
- 默认不 JIT，先保证解释器一致性；启用 JIT 的发行物另跑相同 RSS 合同。AOT 不作为全功能迁移的前置条件。
- 记录资源预算和实际压力结果；不先写虚构吞吐/内存目标。大文件内存应随并发及分块界限增长，不随完整图片体积增长。
- 发布仓库/通道、签名和 ABI 绑定后演练测试发布源；生产发布为实施末端的独立动作。

**验收：** 各平台 `cargo test --workspace`、`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`，及平台 E2E；最终 `cargo run -p xeh-xtask -- verify --all --deny-unverified`。

### Task 17：全量差分、迁移演练与交付审查

**依赖：** 全部前置任务。

**文件：** `compat/manifest.json`、`docs/compatibility.md`、`docs/compatibility-deltas.md`、`tests/e2e/migration/**`、测试报告工件。

**步骤：**
- 同一固定输入集运行 Python 原版和新应用，比较 RPC、错误、任务轨迹、会话、文件字节和代理路径。
- 仅归一化明确列入白名单的 UUID、时间、临时端口、平台路径和新版本标识；不能把状态/错误码/丢字段加入归一化以隐藏问题。
- 演练旧会话半完成 → 新版继续 → 完成 → 浏览 gallery → 归档 → 重启 → 再查看。
- 审查 RSS 业务归属：不得把规则、代理选择、调度判断或 RPC 分发整体搬回 Rust 绕过 RSS 测试。
- 每个 manifest ID 都必须有通过的证据；任何缺项、未验证差异、仅 mock 的平台更新都阻止全量完成标记。

**验收：** `cargo run -p xeh-xtask -- verify --all --deny-unverified`；输出通过项、明确差异和证据链接；由最终全变更审查确认迁移合同。

## 8. 依赖和并行安排

```text
Task 1 → Task 2 → Task 3
Task 1+2 → Task 4 → Task 5 → Task 6
Task 1+2+4+5 → Task 7
Task 3+4+7 → Task 8
Task 3+5+7+8 → Task 9
Task 6+9 → Task 10
Task 4+5+6+9+10 → Task 11
Task 10+11 → Task 12
Task 4+6+9+11 → Task 13
Task 2+4+6+13 → Task 14
Task 11+12+13 → Task 15
Task 14+15 → Task 16 → Task 17
```

可提前拆出的独立范围：
- Task 1 中的 RPC/会话/配置/i18n oracle 采样。
- Task 2 catalog 类型确定后，原生网络能力与 RSS 配置/状态模型。
- Task 7 的普通分页/MPV/图片规则，前提是 typed DTO 先冻结。
- Task 10 的文件/ZIP 与 Task 11 的 RPC 编码。
- i18n 资源提取可提前；CLI 全流程须等下载与退出生命周期接通。

共享 `app/types.rss`、host catalog、会话 schema 只设一位负责人；接口改动先跑调用方编译测试。不得多人同时改 central state/codec 后依赖集成时猜测语义。

## 9. 最终完成标准

以下全部满足才可声明“全功能重写完成”：

- [ ] 25 个配置键、原 CLI、交互及 POSIX daemon 具备原版特征测试。
- [ ] 13 个公开 RPC 方法及原有 HTTP/gallery 路由具备真实请求回归。
- [ ] 任务状态数值、错误码及旧 `h.json`/cookie 迁移完整。
- [ ] 普通分页、MPV、原图/普通图、站点迁移和全部活跃规则执行于 RSS。
- [ ] 扫描/下载真实并行，有界流式写入，取消/暂停/恢复不产生重复提交。
- [ ] 命名冲突、重复图、范围、完整性、归档和 ZIP64 验证通过。
- [ ] 所有列出的代理协议与 Glype 均有实际协议测试，故障和冷却路径可复现。
- [ ] gallery 目录/ZIP 双路径、缩略图、ZIP 下载及读写竞态通过。
- [ ] 原版 WebUI 的连接、任务、配置、阅读、检索和三语流程通过浏览器测试。
- [ ] 自动更新具备真实下载、下次启动生效、平台替换和失败回退证据。
- [ ] i18n key/占位符/错误码/locale 回退无缺项。
- [ ] 简单 config.json parser、25 项默认/覆盖/错误提示通过真实 RSS 测试；发行物无 Python 配置解析/执行依赖，配置格式差异已记录。
- [ ] Linux/macOS/Windows CI、格式、严格 Clippy、全套测试通过。
- [ ] manifest 无未映射功能、无未验证条目，差异账本已逐项关闭。
- [ ] 应用业务主体为 RSS，原生宿主职责符合第 3 节边界。
- [ ] 可用 Core host function 均已优先复用；所有新增应用侧 host function 都有已验证的能力缺口证据，无重复底层实现。

## 10. 交付顺序与主要风险

**交付顺序：** 行为基线 → RSS/并行/流式能力关卡 → 配置和状态/会话 → 解析规则 → 代理与下载 → 文件归档 → RPC/gallery → CLI/i18n/WebUI → 自动更新与平台发布 → 全量迁移审查。

**主要风险及应对：**
- 旧代码异常与 README 不一致：以固定 revision 的可运行证据为依据，差异单列。
- RustScript public SDK 能力：先做真实能力关卡；失败就处理明确前置项，不能用虚构标准库 API 继续写应用。
- 配置格式迁移：简单 JSON 数据文件和字段说明承接实际设置，旧 Python 源码不执行；格式/集合顺序差异明确记录，不再为源码兼容实现解析器或运行时。
- SOCKS4/4a 与 Glype：单独连接器和本地协议替身，不能用 SOCKS5 测试代替。
- gallery/归档/rename 竞态：文件租约、epoch 和取消 quiescence 测试。
- 历史 UI 主机和固定 IP 可能失效：冻结资产、本地测试、可配置来源；不把历史地址可用性当迁移前提。
- 原版更新包与新版格式不兼容：新版独立可信更新源、明确 host ABI 和版本切换合同。
- 仅单机 Linux 验证不足：Windows 文件占用和更新流程必须在对应平台执行。

**本计划的验证范围：** 原始版本为源码调研及实施设计。当前 Task1–3 已验收，旧 Task4 已按用户要求回滚，具体历史证据见执行检查点。本次配置修订步骤属于待实施项，计划预期和撤回候选的旧测试均不能当成新 parser 已验证的结果。
