# rustymail：用 Rust 构建邮件服务器，也构建一套网络编程教材

本项目的目标是：自己实现 SMTP 收信与投递、IMAP 邮箱访问、持久队列和运维工具，使用成熟库处理 TLS、密码散列、MIME 和邮件域认证；在单台 Linux 服务器上，以可测量的资源开销提供可靠的邮件服务。

项目仓库：[ZEPHYR65537/rustymail](https://github.com/ZEPHYR65537/rustymail)。

**当前已有 0.1.0 L0 本地实验实现：SMTP 收信、流式落盘、SQLite 原子投递、离线管理和恢复检查。它还不是生产邮件服务器。** TLS、AUTH、IMAP、外发、域认证和反垃圾尚未实现；程序只允许环回收信，生产启动入口明确拒绝。完整落地设计、Emacs 配置和后续生产目标保留在下方文档中。

## 运行第一封邮件

需要 Rust 1.94.0（仓库已固定工具链）、本机 C 编译器（编译内置 SQLite）、Python 3.11+。Linux 可先安装 `build-essential`，Windows 使用 Rust MSVC 工具链及相应 C++ Build Tools。所有命令在仓库根目录执行；Windows 可在可执行文件名后加 `.exe`。

```sh
cargo build --workspace --locked
python scripts/smoke.py
```

上面的独立冒烟实验自动创建临时账号、收信、强制结束服务、重启并逐字节校验导出结果，不使用真实邮箱。

手动实验，在终端一执行：

```sh
target/debug/rustymaild --config deploy/rustymail.lab.toml check
target/debug/rustymailctl --config deploy/rustymail.lab.toml account add alice@example.com
target/debug/rustymaild --config deploy/rustymail.lab.toml serve-lab
```

终端二执行 `python scripts/lab_send.py`。终端一按 Ctrl+C 停止服务后，查看结果：

```sh
target/debug/rustymailctl --config deploy/rustymail.lab.toml mail list alice@example.com
target/debug/rustymailctl --config deploy/rustymail.lab.toml check-store
```

数据保存在 `data/lab`；离线管理命令与服务共用独占锁，不能同时运行。默认需保留至少 2 GiB 或磁盘 10% 的可用空间，再加活动接收的预留空间。`account add` 只执行一次；已存在时拒绝覆盖。完整导出、测试与原理见[第一轮实现教程](docs/10-first-implementation.md)。

## 当前能力

| 已运行的行为 | 边界 |
| --- | --- |
| EHLO/HELO、MAIL、RCPT、DATA、RSET、NOOP、QUIT | 简化 ASCII dot-atom 信封；非完整 SMTP 一致性声明 |
| SIZE、8BITMIME、严格 CRLF 与点转义 | 不支持 SMTPUTF8、AUTH、STARTTLS、PIPELINING |
| 25 MiB 有界流式收信、多收件人配额原子提交 | 无 MIME 语义处理；不添加 Received/Return-Path 头；仅合成实验邮件 |
| 原文文件 + WAL/FULL 元数据、内部幂等键 | Linux 目录同步；Windows 只用于开发；断电可靠性未验收 |
| 账号创建、分页列信、原文导出、完整性检查 | 离线 CLI；无在线管理、IMAP 或 GC |

配置检查会验证未来配置字段，但不表示对应服务已实现。实验服务只绑定 `listeners.smtp`，不启用其余监听、指标或管理 socket。Emacs 生产配置维持 TLS 要求，当前不能连接这个无 TLS/IMAP 的实验接收器。

## 从这里开始

1. [落地设计总纲](docs/00-project-plan.md)：交付边界、生产定义、技术路线与重要取舍。
2. [架构与 Rust 接口](docs/01-architecture.md)：模块、数据流、任务模型和错误语义。
3. [SMTP、投递与 IMAP 协议契约](docs/02-protocols.md)：协议状态机及兼容性要求。
4. [持久化、队列与崩溃恢复](docs/03-storage-and-recovery.md)：如何对一封已接收的邮件负责。
5. [安全、资源预算与性能验证](docs/04-security-and-performance.md)：威胁模型、背压、基准与限制。
6. [部署与运维手册](docs/05-operations.md)：DNS、TLS、备份恢复、灰度和上线检查。
7. [网络与 Rust 教程路线](docs/06-learning-guide.md)：15 个章节，每章有实验、反例和验收题。
8. [实现计划与验收矩阵](docs/07-implementation-plan.md)：逐阶段任务、依赖、完成定义与发布门槛。
9. [Emacs 使用手册](docs/08-emacs-client.md)：Gnus 收信、SMTP 发信、加密凭据与故障定位。
10. [设计决策与参考资料](docs/09-decisions-and-references.md)：可复核的官方资料、更新策略和知识记录模板。
11. [第一轮实现教程](docs/10-first-implementation.md)：从实际源码学习 TCP 分帧、状态机、异步取消、持久化与故障实验。

## 配套文件

| 文件 | 当前性质 |
| --- | --- |
| [emacs/rustymail.el](emacs/rustymail.el) | 可加载的客户端配置；需填写自己的账号；服务端互通尚未验证 |
| [emacs/authinfo.example](emacs/authinfo.example) | 凭据格式示例，只有占位值 |
| [deploy/rustymail.lab.toml](deploy/rustymail.lab.toml) | 可运行实验配置，显式禁用未实现的扫描和外发 |
| [deploy/rustymail.example.toml](deploy/rustymail.example.toml) | 完整配置契约；`check` 可验证；扫描必需，因此不能用来启动实验接收器 |
| [deploy/rustymail.service.in](deploy/rustymail.service.in) | 未来 Linux 服务模板；当前不可直接启动 |
| [crates/store/migrations/0001.sql](crates/store/migrations/0001.sql) | 首个事务迁移，`user_version=1`；队列等表预留，业务尚未实现 |
| [实现验证报告](reports/0.1.0-lab/validation.md) | 当前实现的测试、故障模型、依赖和已知限制 |
| [验证记录](docs/validation.md) | 本次实际运行的检查，以及仍未执行的验证 |

默认基线为单台 Linux VPS、1–100 个邮箱、2 vCPU / 2 GiB RAM、独立持久磁盘。它是项目的容量设计起点，不是测量结论。第一种生产部署先采用固定上游中继；完整目标还包括自研直接 MX 投递。生产发布前必须通过[发布门槛](docs/07-implementation-plan.md)，不能用完成阶段一代替整个目标。

设计基线日期：2026-09-21。所有设计解释使用中文；代码标识符与协议报文保留英文。
