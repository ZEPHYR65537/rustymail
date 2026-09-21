# M1 验证记录

版本：0.2.0；目标：本地存储与恢复核心，仍为 L0 实验服务。功能和操作说明见 [M1 教程](../../docs/11-m1-storage.md)。

## 当前已执行

Windows 本机已经执行格式检查、严格 Clippy、workspace 全 feature 测试，以及独立 Python SMTP/CLI 实验。覆盖 I/O 失败边界、未知提交结果、事务迁移、离线 GC、正文恢复、原文一致性与独占锁。后续 Linux CI 结果及测量数据将在实际运行后补入本报告。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
python scripts/smoke.py
python scripts/check_docs.py
```

## 验收状态

| 项目 | 当前证据 |
| --- | --- |
| 14 个存储失败边界 | 本机测试通过；不是内核磁盘错误注入 |
| 未知提交结果的两种状态 | 本机存储和真实 TCP 测试通过：数据库可有/无记录，网络不假报成功或拒收 |
| schema 1→2 与迁移中断 | 本机测试与独立 CLI 实验通过；保留已有邮件元数据 |
| GC 预览、受保护引用、中断和重试 | 本机测试与独立 CLI 实验通过 |
| 缺失正文的精确副本恢复 | 本机测试与 CLI 实验通过；错误副本及覆盖被拒绝 |
| 六个原有进程退出点 | 本机测试通过 |
| Linux VM 断电与真实 ENOSPC | 脚本及 CI 已配置，等待实际结果 |
| Linux 性能基线 | 脚本及 CI 已配置，等待原始数据 |

依赖版本仍固定于 Cargo.lock，第三方解析图与[首轮清单](../0.1.0-lab/dependencies.md)相同；新增复用了已有 serde/serde_json。Rust 工具链仍为 1.94.0。RustSec 审计、完整协议、MIME worker 等 M0/后续任务没有因此自动完成。

## 验证边界

进程退出、边界 hook 和 QEMU 客体断电是不同故障模型。Windows 没有本实现要求的目录同步保证；生产目标仍是 Linux。GC 不清除已经接受的消息或投递历史；缺失正文恢复要求精确副本，不等同于完整备份恢复。性能必须披露主机、负载和依赖范围，不以测试通过替代性能结论。
