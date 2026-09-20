# rustymail：用 Rust 构建邮件服务器，也构建一套网络编程教材

本项目的目标是：自己实现 SMTP 收信与投递、IMAP 邮箱访问、持久队列和运维工具，使用成熟库处理 TLS、密码散列、MIME 和邮件域认证；在单台 Linux 服务器上，以可测量的资源开销提供可靠的邮件服务。

项目仓库：[ZEPHYR65537/rustymail](https://github.com/ZEPHYR65537/rustymail)。

**当前交付是完整落地设计与 Emacs 客户端配置，尚未实现 Rust 服务端，不可把本仓库直接部署成生产邮件服务器。** 按“先设计、再实现”的顺序，后续代码、教程和验证证据必须一起推进。本文档中的配置、服务命令和性能数字，分别标注为设计契约、未来接口和待测目标。

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

## 配套文件

| 文件 | 当前性质 |
| --- | --- |
| [emacs/rustymail.el](emacs/rustymail.el) | 可加载的客户端配置；需填写自己的账号；服务端互通尚未验证 |
| [emacs/authinfo.example](emacs/authinfo.example) | 凭据格式示例，只有占位值 |
| [deploy/rustymail.example.toml](deploy/rustymail.example.toml) | 未来服务端配置契约，当前没有读取它的程序 |
| [deploy/rustymail.service.in](deploy/rustymail.service.in) | 未来 Linux 服务模板；当前不可直接启动 |
| [docs/examples/schema.sql](docs/examples/schema.sql) | 存储模型草案，供设计评审和后续迁移实现使用 |
| [验证记录](docs/validation.md) | 本次实际运行的检查，以及仍未执行的验证 |

默认基线为单台 Linux VPS、1–100 个邮箱、2 vCPU / 2 GiB RAM、独立持久磁盘。它是项目的容量设计起点，不是测量结论。第一种生产部署先采用固定上游中继；完整目标还包括自研直接 MX 投递。生产发布前必须通过[发布门槛](docs/07-implementation-plan.md)，不能用完成阶段一代替整个目标。

设计基线日期：2026-09-21。所有设计解释使用中文；代码标识符与协议报文保留英文。
