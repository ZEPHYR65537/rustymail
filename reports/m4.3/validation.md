# 0.10.0 / M4.3 验证记录

本轮实现失败 DSN、schema 4 通知事务、到期索引、跨重试未知标记、人工结案/操作记录，以及客户端和服务器 PIPELINING。协议、资源不变量、迁移取舍和命令说明见[第 22 章](../../docs/22-m43-delivery-lifecycle.md)。当前仍为 L0 环回实验，不是生产发布。

## 本地验证

Windows / Rust 1.94.0：严格 Clippy、格式检查、锁定依赖的全 workspace/all-features **95 项 Rust 测试通过**。新增 9 项存储测试与 2 项传输测试：通知幂等/隐私、配额双重检查与恢复、全局暂存预算、故障点、外域报告/循环抑制、导入来源限制、独立到期/管理历史、旧通知迁移、旧重试不确定性，以及容量为 1 字节的双向部分写入/取消。

独立 Python 对端完成 15 种验证 TLS 的 PIPELINING 情形；六个通知位置实际结束子进程；接收端合并/逐字节命令；认证提交后的报告 MIME/配额/重启检查；未来重试时间不阻挡到期；最终回复丢失后人工重试再遇 550 仍保留 uncertain。所有实验使用一次性账户与环回端口，外部邮件数为 0。

旧版本的基础收信、TLS/身份、STARTTLS、本地交付、输入契约、持久队列、中继和统一 CLI 继续回归。M3.3 的精确能力集合已包含 PIPELINING；M4.2 混合提交现在同时检查新增的一份本地 DSN，不再把全库“只有原信一个 blob”当作断言。

原始证据：[Rust 测试](windows-tests.txt)、[Clippy](windows-clippy.txt)、[M4.3 对端和强杀](windows-lifecycle.json)、[混合中继](windows-relay.json)、[CLI](windows-cli.json)、[输入契约](windows-contract.json)、[队列恢复](windows-queue.json)。外域报告实验先在一次性配置中授予本地域身份，再移出 local_domains，覆盖既有责任遇到域配置变化的路径；没有放宽 CLI 对外域 send-as 的限制。

最新本地 [RustSec 审计](rustsec-audit.json)检查 158 个包（含 5 个工作区包），官方公告库提交 `f8dee89e1b2f2f1eaf548312df7655fe5202a302`，1,288 条公告，0 漏洞、0 警告；启用撤回版本检查。第三方依赖版本没有增加，仅给 store 接入已有 time 依赖。

本地 release 的统一 `rustymail.exe` 为 **7,003,136 字节 / 6.68 MiB**；这不是 Linux 大小或运行内存。工具链、二进制及 LF 规范化源码/证据摘要见[出处记录](provenance.json)。本地记录基于 `035ef02562b6aa6e89249b95533ebe8006646ace` 上的增量工作区，dirty=true，不冒充该基线提交的结果。

GitHub 两平台干净检出、Linux VM 存储故障与独立安全工作流结果将在返回后记录；本地通过不代表尚未返回的 CI 已通过。

## 已知边界

- 通知生成每秒至多启动一个任务，报告至多 16 KiB；复用暂存预算。配额预检阻止持续满配额时的无效文件发布，提交前失败仍可能留下需 GC 的孤立文件。
- 进程强杀证明数据库/文件恢复边界，不能替代物理断电证明。Windows 仍缺少与 Linux 相同的目录同步保证。
- 单收件人、单连接；流水线降低命令等待，但本轮没有新增端到端吞吐提升百分比测量。无连接池、客户端持久队列或远端恰好一次保证。
- 自动报告仅限已认证提交；import/其他内部来源被抑制。不支持 RFC 3461 DSN 协商，也不宣告该扩展。
- 原文、通知及操作记录不自动清理。旧版本重试历史不完整时采用保守 unknown；旧报告无法撤回。
- IMAP/完整 Emacs 互通、域认证、反垃圾、备份/自动恢复与长期负载验收继续属于后续里程碑。

## 复现

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --example m42_attempt --features test-support --locked
cargo build -p rustymail-store --example m41_probe --example m43_probe --features test-support --locked
python scripts/m43_smoke.py
python scripts/m42_smoke.py
python scripts/cli_smoke.py
python scripts/check_docs.py
```

完整协议与 Linux 实验入口见 [CI](../../.github/workflows/ci.yml)；依赖审计由[安全工作流](../../.github/workflows/security.yml)获取最新官方 RustSec 数据库，并启用 yanked 检查、拒绝所有 warnings。
