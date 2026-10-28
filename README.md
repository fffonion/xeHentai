# xeHentai 兼容迁移

本仓库保存 xeHentai 重写的兼容基线。当前范围只有验收清单和原版行为 oracle，应用本体尚未实现。

固定上游：

- `fffonion/xeHentai` `43fc55c0c662f195d048becf6276a26ce06b09d9`
- `fffonion/xeHentai-webui` `4e3738ef463fef344e96df613de9a81b4cda4b4e`

许可证为 GPL-3.0-or-later。原版程序的版权与许可证文本见 `LICENSE`。

## 命令

```text
cargo run -p xeh-xtask -- oracle capture --suite all
cargo run -p xeh-xtask -- manifest check
cargo run -p xeh-xtask -- verify --all --deny-unverified
```

`verify --all --deny-unverified` 在应用行为完成前必须失败。oracle 记录的是原版程序的实测输出，不表示重写已经实现。

兼容清单、差异和命令说明见 `docs/compatibility.md`。
