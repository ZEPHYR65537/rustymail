# M2 验证记录

版本 0.3.0，仍为 L0 实验服务。实现与操作说明见 [M2 教程](../../docs/12-m2-identity.md)。

## 1. 证据来源与结果

日期：2026-09-21。最终代码提交 `6f852d82d8bdfa69b83f9b6656beab8b9dea9458` 的[完整 GitHub CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/35594979361)已通过：Windows、Ubuntu 24.04 两平台测试，以及 Linux 存储实验三个 job 全部成功。后补教程与归档的提交不冒充被测代码。

Rust 固定为 1.94.0。Linux 45 项、Windows 41 项 Rust 测试通过，格式、严格 Clippy、构建、原有 M1 独立客户端/维护实验均通过。Linux 额外覆盖原有三个 Unix 存储检查和新的管理请求取消后通知检查。

- [Linux TLS/身份结果](linux-protocol.json)：11 组独立客户端场景，包括在线管理与不同 UID 负例。
- [Windows TLS/身份结果](windows-protocol.json)：8 组场景；不声称覆盖 Unix 管理。
- [存储故障回归](storage-regression.json)：11 个 VM 断电位置、SMTP/SQLite 两个实际磁盘满场景通过；所有已接受邮件保留，正文/配额/UID/投递关联无不一致。断电前未提交的正文可以成为待回收孤儿。
- [证据出处与摘要](ci-provenance.json)：CI artifact ID、下载 zip SHA-256、原始成员名与归档文件摘要。两个协议报告均记录 `working_tree_dirty=false`。Windows 报告只把换行规范化为 LF，JSON 值不变；其他报告原样保存。

## 2. 独立客户端与回归覆盖

| 场景 | 实际观察 |
| --- | --- |
| 未知 CA、错误主机名、过期证书 | Python/OpenSSL 校验证书的客户端拒绝握手 |
| 未完成握手、低于配置的 TLS 版本 | 握手到期关闭；最低 TLS 1.3 配置拒绝强制 TLS 1.2 客户端 |
| 未认证的 MAIL/RCPT、错误密码/未知用户、跨账号 authzid | 分别拒绝；错误密码和未知用户使用相同通用失败码，不声称时延完全相同 |
| send-as 与 scope | 跨账号信封、跨账号授权、read_only 发信均失败；授权本地邮件成功 |
| 正文身份 | 冒充 From、重复/折叠 From、Sender 和 Resent-* 均拒绝；仅检查本阶段受限头部规则 |
| 存活会话撤销 | 在 DATA 已开始后撤销凭据，旧会话收到 421；重新使用已撤销凭据失败；账号禁用终止旧会话 |
| 最终接受权限竞态 | Rust 存储测试验证：先准备正文再撤销，最终接受事务拒绝；哈希后的认证完成也重新检查状态 |
| 异步取消 | 运行中的散列持续占用预算；管理响应等待被取消，已提交变更仍发出通知 |
| 管理 socket | 目录/socket 私有权限；客户端拒绝权限不安全的 socket；测试中即使临时放宽文件权限，不同 UID 仍被服务端拒绝 |
| 应用密码输出 | 非终端 stdout 无秘密输出目标时拒绝创建；文件一次性创建；列表不含 PHC/秘密 |
| 证书重载 | 密钥不匹配时保留原配置；成功后新连接看到新证书，原连接仍可 NOOP |
| 存储与日志 | 重启后仅有授权邮件，导出逐字节一致；实验日志不含本次 token、AUTH 载荷、PHC 或正文 |

协议场景不是 RFC 全条款一致性声明。独立客户端使用 Python 标准库 `smtplib`/`ssl`，Linux OpenSSL 3.0.13、Windows OpenSSL 3.0.16，详情保留在 JSON。M1 故障回归仍使用 ext4/QEMU、512 MiB 客体；故障模型只丢失客体内存，不证明物理主机掉电安全。

复核代码与本地实验：

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/smoke.py
python scripts/m2_smoke.py
python scripts/check_docs.py
```

Linux CI 额外使用 `python scripts/m2_smoke.py --peer-denial-probe`；它需要可以运行 `sudo -n -u nobody` 的一次性测试环境。存储 VM 的依赖和执行步骤见 [CI 工作流](../../.github/workflows/ci.yml)。

## 3. 依赖审计

[依赖清单](dependencies.json)记录 Cargo.lock SHA-256、153 个第三方包的许可证表达式与声明的 MSRV；均有许可证字段，没有声明高于 Rust 1.94 的 MSRV。依赖总数包含跨平台与测试依赖，不表示全部进入 Linux 服务端二进制。缺失的 MSRV 字段保持 null；不能据此声称那些包声明了兼容版本。

使用 cargo-audit 0.22.2 与 [RustSec 公告库提交 bd8037e](https://github.com/RustSec/advisory-db/commit/bd8037e5cbb8d8cc687c68cdd42ca53d742503fb)进行本地审计，读取 1256 条公告，已知漏洞 0、警告 0。[原始审计结果](rustsec-audit.json)

工具直接联网抓取未完成，改用 Git 获取官方公告库，再执行 `cargo audit --no-fetch --no-yanked --db reports/local/advisory-db --json`。这一证据不包含撤回版本检查，也不是对依赖源码的全面安全审计；报告内保留了这些开关。后续新增或更新依赖需要重新审计。

## 4. 验证边界

TLS/认证实验使用临时 CA、合成邮件和环回连接；未连接公网邮件账号。Unix socket 权限证据仅来自 Linux，Windows 使用离线管理。M1 性能数据没有包含 Argon2/TLS，不能用来描述 M2 登录负载的总内存或生产吞吐。没有 STARTTLS、IMAP、扫描器或远程投递；M0 的 IMAP literal/MIME worker 可行性与 M3 的完整 SMTP 接入仍待完成。
