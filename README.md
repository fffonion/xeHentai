# xeHentai 兼容迁移

本仓库保存 xeHentai 重写的兼容基线。当前范围只有验收清单和原版行为 oracle，应用本体尚未实现。

固定上游：

- `fffonion/xeHentai` `43fc55c0c662f195d048becf6276a26ce06b09d9`
- `fffonion/xeHentai-webui` `4e3738ef463fef344e96df613de9a81b4cda4b4e`

许可证为 GPL-3.0-or-later。原版程序的版权与许可证文本见 `LICENSE`。

## Python 依赖

oracle 需要一个能导入 `requests` 和 `socks` 的解释器。不要把租约路径或虚拟环境路径写进源码。在租约临时目录中创建隔离环境，再把解释器路径传给 `XEH_PYTHON`：

```text
python3 -m venv "$XEH_LEASE_TMP/py/readme-venv"
"$XEH_LEASE_TMP/py/readme-venv/bin/python" -m pip install "requests[socks]"
export XEH_PYTHON="$XEH_LEASE_TMP/py/readme-venv/bin/python"
export XEH_LEASE_TMP
cargo run -p xeh-xtask -- oracle capture --suite isolation
```

`XEH_LEASE_TMP` 必须指向当前执行租约的临时目录。`pip` 只安装到该虚拟环境，不写用户级或系统级 site-packages。

## 命令

```text
cargo run -p xeh-xtask -- oracle capture --suite all
cargo run -p xeh-xtask -- manifest check
cargo run -p xeh-xtask -- verify --all --deny-unverified
```

`verify --all --deny-unverified` 在应用行为完成前必须失败。oracle 记录的是原版程序的实测输出，不表示重写已经实现。

## RustScript 嵌入

下载器的站点客户端尚未实现。当前可执行的是嵌入入口和 Task 3 transfer worker。

```text
cargo run -p xeh-host
cargo run -p xeh-xtask -- rss-test
```

两条命令都编译并执行 `app/main.rss`，打印 RSS 算出的整数。`WorkerPool` 另编译 `app/worker/transfer.rss`，每个 worker 在自己的 OS 线程上持有 VM。依赖锁定为 `pd-vm` revision `99890b68119715df9ddd19f6a6362ff8d6258ffa`，feature 为 `runtime`、`async`、`http-client`，不含默认 JIT。定时器驱动固定在创建线程；其他线程要把 drive 和 shutdown 交回该线程。复用边界见 `docs/architecture.md`。

兼容清单、差异和命令说明见 `docs/compatibility.md`。
