# 兼容清单

本文说明兼容 oracle 如何记录原版 xeHentai 的可观察行为。应用重写尚未开始，清单中的每一项都保持未实现。

## 上游

oracle 执行的源码来自：

- 应用：`fffonion/xeHentai`，revision `43fc55c0c662f195d048becf6276a26ce06b09d9`
- WebUI：`fffonion/xeHentai-webui`，revision `4e3738ef463fef344e96df613de9a81b4cda4b4e`

源码通过 GitHub archive API 取得，不使用 SSH clone。检出副本放在被忽略的 `compat/upstream/`，归属记录在同目录的 `PROVENANCE.txt`。

## 清单字段

`compat/manifest.json` 的每个功能包含：

- `id`：稳定标识
- `source`：上游文件或计划章节
- `input`：本次实际输入；未执行时只写范围说明
- `expected`：实测观察或未执行原因。`implementation` 固定为 `pending`
- `target`：重写目标。当前为 `pending`
- `tests`：对应的检查命令或测试
- `status`：`oracle_characterized`、`oracle_failed` 或 `unverified`

`implemented`、`accepted`、`passing` 会被检查器拒绝。`verify --all --deny-unverified` 在存在未完成项时退出非零，并打印 `fail-closed` 与 `implementation_accepted=0`。

## 隔离

oracle 进程使用独立的 fixture 根作为 `HOME`、`USERPROFILE`、工作目录和入口脚本目录。原版把 `FILEPATH` 设为入口脚本目录，因此入口脚本放在 fixture 内，会话、日志和缓存写入不会落到用户目录。

子进程环境只保留固定的非秘密名单。收据写入租约临时目录，不记录完整环境。

网络连接只允许 `127.0.0.1`、`::1` 和 `localhost`。代理、Glype 和 HTTP 夹具都是本地合成服务，不访问真实账号、站点或代理。

## 命令

```text
cargo run -p xeh-xtask -- oracle capture --suite all
cargo run -p xeh-xtask -- oracle capture --suite isolation
cargo run -p xeh-xtask -- manifest check
cargo run -p xeh-xtask -- verify --all --deny-unverified
```

`capture` 成功只表示原版程序已在隔离环境中运行并写下观察。它不更新应用实现状态。

差异账本见 `docs/compatibility-deltas.md`。尚未执行的路径标为 `unverified`，不补写推测输出。已执行的原版异常保留实测文本，拟定合同保持待批准。
