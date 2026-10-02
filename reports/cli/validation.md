# 0.9.0 验证记录：统一 CLI 与 SMTP 提交客户端

版本 0.9.0，数据库 schema 仍为 3。本轮是 M4.2 之后的客户端支线；M4.3 尚未实现，生产服务入口继续拒绝。设计、源码导读、操作和取舍见[第 21 章](../../docs/21-unified-cli.md)。

## 已实现与已执行

同一个 `rustymail` 提供 send、serve、admin、check；保留两个旧程序名。SMTP/TLS 网络引擎抽为独立 client crate，队列通过适配层保留租约与持久化语义。客户端仅需自己的配置，在连接前流式校验原文并创建有限临时快照；支持文件、stdin、两种验证证书的 TLS 和 AUTH PLAIN，按收件人输出结果且不自动重试。

| 本地 Windows 检查 | 结果／原始记录 |
| --- | --- |
| 格式与严格 Clippy | 通过；[Clippy 日志](windows-clippy.txt) |
| 锁定依赖、所有 feature 的 Rust 测试 | 84 项通过；[测试日志](windows-tests.txt) |
| debug／release 构建、证书生成器／中继探针构建 | 通过；工具链见[出处](provenance.json) |
| 独立 CLI 实验 | 20 组通过；[客户端结果](windows-cli.json) |
| 旧 CLI 与收信／导出／恢复实验 | 18 项通过；[兼容结果](windows-legacy.json) |
| 原中继实验 | 24 个单次场景、5 组集成检查通过；[队列回归](windows-relay.json) |
| 完整 cargo-audit，拒绝 warnings，启用 yanked 检查 | 158 个包（含 5 个工作区包），0 漏洞、0 警告；[审计结果](rustsec-audit.json) |

独立 Python 对端核对实际接收字节与 CLI 报告：TLS/STARTTLS、密码只在加密连接发送、错误 CA／名称／过期证书拒绝、Bcc／Resent-Bcc／Return-Path 折行移除、文件/stdin 一致、远端地址大小写与去重、4xx/5xx、最终回复丢失／超时、混合结果退出 3、畸形／超限输入在连接前拒绝。真实 rustymail 服务端互通后检查存储完整性。所有内容和密码均为实验生成，外部邮件发送数为 0。

Rust 新增 4 项配置／快照测试；原传输分片测试移入 client crate，其余队列取消、EOF 校验与持锁测试保留。测试数增加不代表形式化证明。CLI SIGINT 的独立进程实验只在 Linux 执行；Windows 本地未验证控制台取消。

## 二进制与资源边界

本地 x86_64-pc-windows-msvc、Rust 1.94.0，仓库 release 配置（thin LTO、codegen-units=1、strip=debuginfo）：

| 文件 | 字节 | MiB |
| --- | ---: | ---: |
| rustymail.exe | 6,788,096 | 6.47 |
| rustymaild.exe | 6,026,240 | 5.75 |
| rustymailctl.exe | 3,918,336 | 3.74 |

统一程序已包含三个角色；不需要把这三项加在一起作为单文件大小。精确摘要见出处记录。该数字不是 Linux 发布文件大小，也不是运行内存。客户端预检和发送使用固定上限缓冲，正文留在最多 25 MiB 的临时文件；本轮没有新增 CLI RSS／吞吐实测，不将 M4.2 单次中继探针的历史测量冒用为 CLI 性能。

逐人串行提交会重复握手和传输正文，尚无连接池、多 RCPT 或并发优化。输入要求既有 CRLF 邮件，不自动编辑 MIME／附件；没有 SMTPUTF8、OAuth、IMAP、Sent 归档或客户端持久重试队列。强杀、磁盘故障或输出管道断开仍可能丢失结果报告；accepted 只表示上游接收责任，uncertain 需要核查。

## 出处与复现

本地证据针对基于 `f0fa7eb53125ec6a398f9a096142d358ae3f53f1` 的本轮工作区；脚本里的 revision 是基线，working_tree_dirty 为 true，不能当作基线提交的测试结果。[出处记录](provenance.json)保存实际源码／脚本／锁文件的 LF 规范化 SHA-256、工具链和二进制摘要。RustSec 公告库为官方提交 `6de4455103aced2cba86e3b86e5c090b22827cf1`，含 1,279 条公告；第三方依赖版本未增加，新增的是 workspace client 包及依赖关系。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --example m42_attempt --features test-support --locked
python scripts/cli_smoke.py
python scripts/m42_smoke.py
python scripts/smoke.py
python scripts/check_docs.py
```

[CI](../../.github/workflows/ci.yml)增加两平台 CLI 实验和原始结果产物；Linux 另测 DATA 后 SIGINT：当前项 uncertain、后续项 not_attempted、退出 3、无第二次连接。独立[依赖审计工作流](../../.github/workflows/security.yml)继续检查最新公告。本地记录不替代 Linux 作业、整机故障、真实邮件服务互通或生产验收。

## 远端干净检出验证

提交 `229d2076615608147c08f31eb5cfe87307d64c01` 的 [CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/37044729859) 已通过 Linux、Windows 两个平台协议作业：分别 88、84 项 Rust 测试；全部旧 SMTP／队列实验通过，新增 CLI 实验分别 21、20 组。Linux 的新增第 21 组是正文后 SIGINT，不确定结果和后续跳过均符合约定。两份 CLI 结果的 working_tree_dirty 均为 false，见 [Linux 原始结果](ci-linux-cli.json)、[Windows 原始结果](ci-windows-cli.json)。

[安全工作流](https://github.com/ZEPHYR65537/rustymail/actions/runs/37044729782)也通过：当前官方公告库提交 `117edb3bed98e9be112f277b7615eea3252e7c43`，含 1,280 条公告，扫描 158 个包，0 漏洞、0 警告。该库比本地扫描更新，分别保存[扫描结果](ci-rustsec-audit.json)和[版本／锁文件出处](ci-security-provenance.txt)。

[CI 出处记录](ci-provenance.json)保存作业链接、测试数量、日志摘要、产物压缩包 SHA-256 与归档文件摘要。下载时核对 GitHub 提供的产物摘要，归档只规范化换行，不改 JSON 值。

同一 CI 的 Linux 存储实验作业也已成功，整条 CI 完成。原始记录包括[四组容量／恢复压力](ci-linux-pressure.json)、[QEMU 中断与实际磁盘满](ci-linux-storage.json)、[收信基准](ci-linux-benchmark.json)、[分帧基准](ci-linux-framing.json)和[中继流式采样](ci-linux-relay-stream.json)。这些是原实验的回归，不能替代物理掉电或生产持续负载验收。

抽取后的共享 SMTP 引擎在独立 Linux release 探针里发送约 1 MiB／25 MiB 时，观察到的 HWM 分别为 5,088／5,092 KiB，通过既有流式回归门槛。采样间隔目标为 5 ms，峰值可能漏采；测量对象是单次中继探针，不含 CLI 预检／快照、Python 对端、数据库、队列或 Argon2，所以不作为统一 CLI 的 RSS 声明。
