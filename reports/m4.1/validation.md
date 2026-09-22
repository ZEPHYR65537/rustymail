# M4.1 验证记录：持久队列基础

版本 0.7.0，schema 3，L0 离线实验。本轮实现入队、逐收件人责任、租约所有权、有限扫描、退避及不确定结果；没有网络外发或 DSN。原理与操作见[第 17 章](../../docs/17-m4-durable-queue.md)。

功能提交前的本地验证包括 Rust 单元/故障测试、严格 Clippy、格式、CLI 构建、既有 SMTP/TLS 回归及八个队列进程终止位置。最终数量和干净检出 CI 原始证据将在本轮验证完成后更新；本段不是已通过全部 CI 的声明。

复现新增实验：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-store --example m41_probe --features test-support --locked
python scripts/m41_smoke.py
python scripts/check_docs.py
```

本轮没有新增第三方依赖。现有 Linux QEMU 存储实验覆盖基础同步与迁移，不代表新增队列位置已经通过专门掉电测试；新八个位置使用进程强杀。逐收件人结果目前来自归一化存储 API，真实上游 2xx/4xx/5xx/断连映射属于 M4.2。本实现仍不是生产邮件服务器。
