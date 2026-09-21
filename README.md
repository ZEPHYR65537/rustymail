# rustymail：用 Rust 构建邮件服务器，也构建一套网络编程教材

本项目的目标是：自己实现 SMTP 收信与投递、IMAP 邮箱访问、持久队列和运维工具，使用成熟库处理 TLS、密码散列、MIME 和邮件域认证；在单台 Linux 服务器上，以可测量的资源开销提供可靠的邮件服务。

项目仓库：[ZEPHYR65537/rustymail](https://github.com/ZEPHYR65537/rustymail)。

**当前是 0.4.0 / M3.1 L0 实验实现：已有存储恢复、身份与 TLS、三个 SMTP 入口角色及 STARTTLS。它还不是生产邮件服务器。** IMAP、外发、域认证和反垃圾尚未实现；程序只允许环回实验，生产启动入口明确拒绝。按[教程总目录](docs/06-learning-guide.md)学习，或直接进入[入口与 STARTTLS 实验](docs/14-m3-starttls.md)。M3 的消息头责任和完整输入契约仍待完成。

0.3.1 修复磁盘任务取消、撤销通知丢失和管理响应超限，复用 DATA 行缓冲并收紧组合资源预算。原理与保留限制见[取消与资源边界教程](docs/13-cancellation-and-bounds.md)，回归证据见[修复验证报告](reports/0.3.1/validation.md)。

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
| SIZE、8BITMIME、严格 CRLF 与点转义 | 不支持 SMTPUTF8、PIPELINING |
| 三入口角色、STARTTLS 状态重置与预读丢弃 | 只在环回启用；共享连接/握手/接收预算；握手失败关闭 |
| 隐式 TLS、AUTH PLAIN、Argon2id 应用密码 | 登录仅在 TLS 实验模式启用；散列并发/等待有界；无 LOGIN/OAuth |
| send-as、权限 scope、事务内重新授权、撤销旧会话 | 提交只接受受限的单个 ASCII From；无完整 MIME 地址解析；只投递本地邮箱 |
| Linux Unix socket 管理、对端身份校验、证书重载 | 只供本机管理员；Windows 使用离线管理；ACME 与证书临期告警待实现 |
| 25 MiB 有界流式收信、多收件人配额原子提交 | 无 MIME 语义处理；不添加 Received/Return-Path 头；仅合成实验邮件 |
| 原文文件 + WAL/FULL 元数据、内部幂等键 | Linux ext4/QEMU 断电及实际磁盘满实验通过；物理掉电与生产存储栈待验收；Windows 只用于开发 |
| 账号创建、分页列信、原文导出、完整性检查、离线 GC | GC 默认预览；仅回收无引用文件，不过期删除邮箱或投递历史 |
| schema 1→2 原子迁移、operation 查询、缺失 blob 恢复 | 迁移校验结构与摘要；恢复不覆盖现有文件，不改变 UID/配额 |

配置检查会验证未来配置字段，但不表示对应服务已实现。`serve-lab` 只绑定 `listeners.smtp`；`serve-lab-tls` 只绑定 `listeners.submissions`；新 `serve-lab-smtp` 同时绑定 smtp/submissions/submission，后两种启动模式在 Linux 启用私有管理 socket。均无指标服务和 IMAP。Emacs 配置维持 TLS 要求；完整 Gnus 收发互通等待 M5。

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
12. [M1：故障、维护与实测](docs/11-m1-storage.md)：操作手册、故障边界、迁移与回收规则、Linux 实验。
13. [M2：TLS、身份、授权与本地管理](docs/12-m2-identity.md)：独立客户端实验、密码验证预算、事务内授权和异步取消的责任边界。
14. [取消与资源边界](docs/13-cancellation-and-bounds.md)：从真实缺陷学习生命周期、通知、字节预算和性能证据。
15. [M3.1：入口职责与 STARTTLS](docs/14-m3-starttls.md)：共享准入、协议切换、预读丢弃、状态重置及独立客户端反例。

## 配套文件

| 文件 | 当前性质 |
| --- | --- |
| [emacs/rustymail.el](emacs/rustymail.el) | 可加载的客户端配置；需填写自己的账号；服务端互通尚未验证 |
| [emacs/authinfo.example](emacs/authinfo.example) | 凭据格式示例，只有占位值 |
| [deploy/rustymail.lab.toml](deploy/rustymail.lab.toml) | 可运行实验配置，显式禁用未实现的扫描和外发 |
| [deploy/rustymail.tls-lab.toml](deploy/rustymail.tls-lab.toml) | TLS/身份与三入口实验；启动命令决定活跃端口，见 M2/M3.1 教程 |
| [deploy/rustymail.example.toml](deploy/rustymail.example.toml) | 完整配置契约；`check` 可验证；扫描必需，因此不能用来启动实验接收器 |
| [deploy/rustymail.service.in](deploy/rustymail.service.in) | 未来 Linux 服务模板；当前不可直接启动 |
| [存储迁移](crates/store/migrations/0002.sql) | 当前 `user_version=2`，加入迁移与维护记录；保留所有已接受邮件 |
| [第一轮验证报告](reports/0.1.0-lab/validation.md) | 0.1.0 的历史测试、依赖和进程故障证据 |
| [M1 验证报告](reports/m1/validation.md) | 0.2.0 的迁移、维护、Linux VM 故障与性能原始数据 |
| [M2 验证报告](reports/m2/validation.md) | 0.3.0 的 TLS、身份、Unix 管理证据及依赖审计 |
| [M3.1 验证报告](reports/m3.1/validation.md) | 0.4.0 的三入口与 STARTTLS、两平台协议及 Linux 存储故障回归 |
| [验证记录](docs/validation.md) | 设计阶段的历史静态检查，以及各阶段验证报告入口 |

默认基线为单台 Linux VPS、1–100 个邮箱、2 vCPU / 2 GiB RAM、独立持久磁盘。它是项目的容量设计起点，不是测量结论。第一种生产部署先采用固定上游中继；完整目标还包括自研直接 MX 投递。生产发布前必须通过[发布门槛](docs/07-implementation-plan.md)，不能用完成阶段一代替整个目标。

设计基线日期：2026-09-21。所有设计解释使用中文；代码标识符与协议报文保留英文。
