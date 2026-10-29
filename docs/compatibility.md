# 兼容清单

本文说明兼容 oracle 如何记录原版 xeHentai 的可观察行为。应用重写尚未开始，清单中的每一项都保持未实现。

## 上游

oracle 执行的源码来自：

- 应用：`fffonion/xeHentai`，revision `43fc55c0c662f195d048becf6276a26ce06b09d9`
- WebUI：`fffonion/xeHentai-webui`，revision `4e3738ef463fef344e96df613de9a81b4cda4b4e`

源码通过固定提交的 Git 对象逐字节核对后才导入。检出副本放在被忽略的 `compat/upstream/`。归属记录在同目录的 `PROVENANCE.txt`，只描述已经完成的核对；未执行的 WebUI 不标为已核对。

## 清单字段

`compat/manifest.json` 的每个功能包含：

- `id`：稳定标识
- `source`：上游文件或计划章节
- `input`：本次实际输入；未执行时只写范围说明
- `expected`：实测观察或未执行原因。`implementation` 固定为 `pending`
- `target`：重写目标。当前为 `pending`
- `tests`：对应的检查命令或测试
- `status`：`oracle_characterized`、`oracle_failed`、`partial` 或 `unverified`

`implemented`、`accepted`、`passing` 会被检查器拒绝。`verify --all --deny-unverified` 在存在未完成项时退出非零，并打印 `fail-closed` 与 `implementation_accepted=0`。

`cli.option.*` 先按 AST 枚举全部选项拼写。只有实际执行过的拼写才标为 `oracle_characterized`，未执行的保持 `unverified`。AST 中出现不等于已执行。

`behavior.sec4.rpc-methods` 与 `behavior.sec4.proxy-schemes` 的计数和状态从子项派生。登录失败或未执行的协议不能把父项标成完全覆盖。

## 上游字节核对

导入前，oracle 用租约内的 bare Git 仓库对照固定提交 `43fc55c0c662f195d048becf6276a26ce06b09d9` 的 Git 对象与检出文件。已有检出若与固定提交不一致，捕获失败，不覆盖、不修复该目录。WebUI 修订只记录，不执行，也不标为已核对。

## 易变字段

归一化 golden 只替换三类运行期值：fixture 根路径、HTTP `Date` 头、以及本次捕获实际绑定并记录的本地端口。原始 `result.json` 保留这些值。业务字段的变化必须能被 `oracle compare` 检出。

帮助文本使用固定 `COLUMNS=4096`，避免 argparse 把 fixture 根折成多行后逃过路径替换。任务 GUID 在导入前由本进程计数器代替 `uuid.uuid4` 生成，golden 保留具体值，不把 GUID 当作第四类易变字段删除。调用次数或顺序变化会改变 GUID，比较必须失败。

## 隔离

oracle 进程使用独立的 fixture 根作为 `HOME`、`USERPROFILE`、工作目录和入口脚本目录。原版把 `FILEPATH` 设为入口脚本目录，因此入口脚本放在 fixture 内，会话、日志和缓存写入不会落到用户目录。

子进程环境只保留固定的非秘密名单。收据写入租约临时目录，不记录完整环境。

每次 capture 独占创建新的输出目录。显式 `XEH_CAPTURE_OUT` 已存在时直接拒绝，不清理、不沿用、不覆盖其中的旧文件。Rust 生成本次 `run_id`，写入父子进程环境和收据。每份 audit 与 `expected-children.json` 必须携带同一个 `run_id`。

Python audit hook 在所属进程的 audit 中记录 `subprocess.Popen` 派生事实，只保留子进程 ID、父进程 ID、预期 audit 路径、预期 output 路径和 `run_id`。这是夹具协作记录，不能当作操作系统级沙箱。

发布前核对三方集合：父进程派生记录、预期子进程清单、`processes/` 下的 audit 文件。三者必须严格一致。身份图以 root 为根，父节点必须存在且与派生者一致，不允许重复 ID、重复路径、环或指向本次捕获之外的文件。空清单只在没有派生记录、也没有其他 audit 文件时成立。校验失败时保留原始诊断，不写入可信 manifest 或 golden。

网络连接只允许 `127.0.0.1`、`::1` 和 `localhost`。代理、Glype 和 HTTP 夹具都是本地合成服务，不访问真实账号、站点或代理。

## 命令

需要 `XEH_LEASE_TMP` 指向本次执行租约的临时目录。解释器由 `XEH_PYTHON` 指定；未设置时使用 `python3`，且必须能导入 `requests` 与 `socks`。

```text
cargo run -p xeh-xtask -- oracle capture --suite all
cargo run -p xeh-xtask -- oracle capture --suite isolation
cargo run -p xeh-xtask -- oracle compare --left <dir> --right <dir>
cargo run -p xeh-xtask -- manifest check
cargo run -p xeh-xtask -- verify --all --deny-unverified
```

`capture` 成功只表示原版程序已在隔离环境中运行并写下观察。它不更新应用实现状态。

差异账本见 `docs/compatibility-deltas.md`。尚未执行的路径标为 `unverified`，不补写推测输出。已执行的原版异常保留实测文本，拟定合同保持待批准。
