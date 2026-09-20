# 设计阶段验证记录

本页保留首个设计提交的历史验证结果。后续 Rust 实现、真实 TCP 与进程故障检查见 [0.1.0 L0 实现报告](../reports/0.1.0-lab/validation.md)；当前文档链接和附件检查可执行 `python scripts/check_docs.py`。

日期：2026-09-21。环境：Windows / PowerShell 7.6.5，Emacs 31.1，Python 3.13.5。此记录只证明设计附件的静态可用性，不证明服务端、协议互通或生产可靠性。

## 检查项

| 检查 | 当前结果 |
| --- | --- |
| Emacs 配置加载、设置单账号参数 | 已通过；没有发起网络连接 |
| Emacs 字节编译，warning 视为错误 | 已通过 |
| 文档本地链接与代码块闭合 | 已通过：12 个 Markdown 文件、32 个本地链接 |
| TOML 语法及文档关键预算对应 | 已通过：实验环回监听、禁用出站及关键容量/时限值符合契约 |
| SQL 草案建表、外键、UID 与唯一约束 | 已通过：SQLite 3.49.1 创建 12 张表；WAL/FULL/外键启用；5 项约束负例被拒绝 |
| rustymail 名称一致性 | 已通过：交付文件无旧项目名称残留 |

SQL 负例包括 UID 为零、同邮箱重复 UID、引用不存在的邮箱、重复本地投递关系、无 token 的 leased 状态。数据库完整性与外键检查通过。这不代表跨账号授权、完整状态机或耐久性已由 SQL 自动实现。

字节编译后的 Emacs 配置还在禁止网络调用的环境里执行了一次 setup；确认 IMAP 993、SMTP 465 和 `nnimap+rustymail:Sent`，未访问账号或外部网络。未在 Emacs 29/30 或 Linux Emacs 上验证。

## 可复核入口

在项目根目录执行客户端静态检查（不会连接服务器）：

```sh
emacs --batch -Q --eval '(setq byte-compile-error-on-warn t)' -f batch-byte-compile emacs/rustymail.el
emacs --batch -Q -L emacs -l rustymail.el --eval '(progn (setq rustymail-address "alice@example.com" rustymail-full-name "Rustymail Test" rustymail-host "mail.example.com") (rustymail-setup))'
```

SQL 草案可以在空 SQLite 3.37+ 数据库中执行，随后检查 `PRAGMA foreign_key_check`、`PRAGMA integrity_check`、`PRAGMA journal_mode` 和 `PRAGMA synchronous`。五项负例使用独立 savepoint 验证约束拒绝，再 rollback，避免留下失败案例的数据。配置文件使用 Python 3.11+ 的 `tomllib` 解析；设计阶段这只是语法检查，后续 L0 已增加 Rust 严格配置验证器。

## 设计提交时未执行的检查

设计提交当时尚未有 Rust 服务端，因此未执行 Rust 构建/测试、SMTP/IMAP 互通与故障注入。后续 L0 已完成其中一部分，结果以新报告为准。Linux systemd 生产启动、IMAP、断电、性能基准、72 小时稳定性、DNS/TLS 公网配置、真实外部收发和备份恢复演练仍未验收。

Emacs 的 `.gpg` 凭据没有创建，没有修改用户已有 init，也没有导入任何真实密码。客户端仅使用占位值完成本机静态验证。
