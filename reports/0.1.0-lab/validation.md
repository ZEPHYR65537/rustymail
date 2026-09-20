# 0.1.0 L0 实验实现验证

日期：2026-09-21。本报告对应首次 Rust 实现，范围是环回 SMTP 与本地持久化，不是生产发布。完整启动方式见[教程](../../docs/10-first-implementation.md)。

## 本机环境与已执行检查

Windows、PowerShell 7.6.5、Rust 1.94.0、Python 3.13.5。工具链固定于 `rust-toolchain.toml`，crate 固定于 `Cargo.lock`；SQLite 来自 rusqlite bundled 构建，不依赖系统安装的版本。依赖版本、许可证与声明 MSRV 见[依赖清单](dependencies.md)。

验证命令：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo build --workspace --locked
python scripts/smoke.py
python scripts/check_docs.py
```

上述本机检查全部通过；文档检查覆盖 15 个 Markdown 文件与 62 个本地链接。另已执行 `cargo build --workspace --release --locked` 与 `python scripts/smoke.py --bin-dir target/release`，优化构建的同一链路通过。这里未测量性能，不将优化构建成功解释为吞吐或内存目标已满足。

本机测试覆盖 5 个 core、6 个 protocol、7 个真实 TCP、11 个 store 测试条目，共 29 个。store 数量包括 1 个专供子进程启动的故障辅助入口；父测试实际启动六个故障子进程。Unix 另有符号链接与非私有目录的 2 个拒绝测试，共 31 个条目。独立 Python 客户端另行检查最终 250 后强制结束进程、恢复、原文字节、禁止覆盖导出和独占管理锁。

CI 已配置 Ubuntu 24.04 与 Windows 两个平台，执行相同格式、Clippy、测试、构建和独立互通检查。CI 的实际运行结果以[仓库 Actions](https://github.com/ZEPHYR65537/rustymail/actions)中对应提交为准；本文不会把“创建工作流”视为“远程已通过”。

首轮 CI 发现 Unix 存储测试 fixture 未显式设置 0700，触发预期的数据目录权限拒绝；修复为创建时设置私有权限，并补充 0755 目录必须拒绝的负例。原有服务权限校验保留，详见教程中的问题记录。

## 覆盖的关键不变量

| 范围 | 证据 |
| --- | --- |
| 不把畸形/过大输入当命令或邮件 | CRLF 跨界、每字节切分、裸 LF/NUL/溢出负例；真实 TCP DATA 超限后关闭连接 |
| 不保留前一次 MAIL 的收件人 | 纯状态转换与畸形 MAIL 的真实连接测试 |
| 不开放中继 | 实际账号查询；独立客户端外域 RCPT 被拒绝 |
| 不出现多收件人部分可见 | 任一收件人不存在、禁用或配额失败时整个接受事务回滚 |
| 不重复内部提交 | operation ID + 原文/信封一致性检查；相同重放返回同一消息 |
| 不对跨账号查询返回正文 | 导出必须同时匹配账号与 message ID |
| 不在取消后提交错误摘要 | 手动 poll 到 Pending 后取消 append，prepare 必须拒绝 |
| 限制暂存和并发实例 | staged/prepared 持有空间预留与进程锁，最后一个 token 释放后才可重开 |
| 崩溃后不出现引用缺失 | 六个进程退出点 + 重开存储/摘要验证；独立进程在最终 250 后强制结束 |
| 流式接收完整大小上限 | 使用小缓冲分块写完整 25 MiB，不创建整封正文输入数组 |
| 不把配置语法成功当生产就绪 | `check` 输出 structural_only/production_ready=false；`serve` 拒绝且不初始化数据 |

## 未覆盖与限制

- Linux VM 断电、磁盘缓存失效、ENOSPC、同步失败和 COMMIT 结果未知尚未完成故障注入；进程退出不能代替这些测试。
- Windows 不提供本实现所需的目录 fsync 保证。Unix 路径检查拒绝直接符号链接，但尚未完成针对恶意本机同 UID 进程的路径竞争加固；应使用私有目录和专用服务账号。
- 部分文件可以成为 orphan，当前只报告、不 GC。长期存储容量、WAL/事件增长、备份恢复和迁移回滚未验收。
- 无 TLS、AUTH、IMAP、外发、域认证、反垃圾、完整 SMTP trace/header 行为；Emacs 与服务端尚未互通。
- 未运行 cargo-audit/RustSec 全量公告审计、fuzz/property campaign、72 小时 soak、独立安全评审、吞吐/延迟/RSS 基准。
- 无真实 VPS、域名、证书或外部测试账号参与。本轮未向互联网收件人投递邮件。

生产门槛仍以[实现计划](../../docs/07-implementation-plan.md)的 L1/L2 清单为准，所有未验收项保留。
