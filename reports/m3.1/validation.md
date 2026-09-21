# M3.1 验证记录：SMTP 入口与 STARTTLS

版本 0.4.0，L0 环回实验。被测代码提交 `83caa0030bae27f3c77f3d2173782df8732f4f23`；[对应 CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/35607714701)。后补教材整理、注释措辞和证据归档不改变实现行为，也不冒充另一份被测代码。

## 1. 交付范围

新增 `serve-lab-smtp`，在单进程内提供收信、隐式 TLS 提交和 STARTTLS 提交三个角色；共享存储、认证与准入预算。旧实验启动命令继续可用。新增两个 Rust 测试覆盖扩展命令的 EHLO 前提和确定性预读丢弃，独立客户端验证完整网络路径。

教程同步维护：增加[已落地教材导航](../../docs/06-learning-guide.md)，明确蓝图和实现的区别；[M3.1 章节](../../docs/14-m3-starttls.md)覆盖信任边界、协议交换、状态/缓冲/许可生命周期、源码、实验、失败路径与自测答案；旧章节明确历史版本和后续修正。操作手册、README、ADR 和里程碑同步更新。

## 2. 规范、项目策略与测试映射

| 要求或策略 | 本轮行为 | 验证入口 |
| --- | --- | --- |
| RFC 3207 §4：STARTTLS 无参数，220 后进入 TLS | 多余参数 501；握手失败关闭，不回退明文 SMTP | `extensions_require_ehlo_and_a_new_tls_session_has_no_old_envelope`；独立客户端的参数/失败握手场景 |
| RFC 3207 §4.2：重置会话知识 | TLS 后没有第二个 banner；旧 EHLO、MAIL、RCPT 无效；新 EHLO 不再公布 STARTTLS | 独立客户端在匿名接收事务中升级，然后直接 MAIL/RCPT/DATA 均失败 |
| 升级边界：明文预读不可成为新会话命令 | 已预读字节丢弃，其余字节进入 TLS 解码 | `upgrade_drops_prefetched_plaintext_before_reading_new_transport_bytes`；TCP 合并 STARTTLS 与注入命令 |
| 收信/提交角色是项目策略 | 收信入口无 AUTH；提交必须 TLS、EHLO、AUTH，再检查 send-as；均不接受外域 RCPT | 独立客户端分别经过三个真实监听端口 |
| 同一服务使用全局资源额度 | 三入口共享连接、IP、握手及接收预算，握手前取得额度 | 独立客户端跨端口占用连接；一个握手阻塞时另一端口得到 454；超时后可再次握手 |
| 升级不能延长未认证生命周期 | 初始截止时间跨 TLS 保留 | 正常场景使用 30 秒预算；单独重启成 5 秒，升级后按原截止时间收到 421 |
| 撤销和最终授权不被升级绕过 | 继续使用 M2 的权限通知与事务内复查 | Linux 升级后 DATA 中在线撤销；原有存储授权竞态回归 |
| 接受责任与原文保存 | 仅四封合法邮件形成责任，重启后导出字节一致 | 独立客户端检查收件人、接收数量、导出及无秘密日志 |

RFC 链接与解释见教程。要求 EHLO 后才允许升级、拒绝未支持地址形式、收信入口不提供 AUTH，以及仅环回运行，均为此实现的明确策略；不能将这些选择一概说成所有 SMTP 服务器必须如此。

## 3. 复现与证据边界

Rust 1.94.0；Linux 55 项、Windows 51 项 Rust 测试通过，格式、严格 Clippy、构建和原有 SMTP 维护实验通过。

2026-09-21，该提交的三个 CI 作业全部通过：两平台测试与 Linux 存储实验。归档校验了五个产物压缩包的 GitHub SHA-256，并保留七份原始结果及归档摘要。

- [Linux M3.1 客户端结果](linux-protocol.json)：11 组，包括在线管理状态和升级后 DATA 中撤销。
- [Windows M3.1 客户端结果](windows-protocol.json)：10 组，不声称验证 Unix 管理。
- [Linux M2 回归](linux-m2-regression.json)及 [Windows M2 回归](windows-m2-regression.json)：原有 TLS、身份、错误证书、权限、秘密日志检查继续通过，Linux 还覆盖不同 UID、凭据大分页和证书轮换。
- [Linux 存储故障回归](storage-regression.json)：11 个 QEMU 中止位置及 SMTP/SQLite 两种真实磁盘满场景通过；已接受邮件、配额和 UID 校验一致。模型是杀死 QEMU、丢失来宾内存，宿主机及其存储仍运行，不等于物理设备掉电验证。
- [原有匿名收信基准](linux-benchmark.json)与[独立分帧基准](linux-framing.json)：保留本轮回归原始样本。匿名基准使用 `serve-lab` 明文入口，分帧基准不包含网络和磁盘；二者均不能证明三入口 TLS/认证混合负载的性能。不同 CI 宿主机的数字不用于宣称本轮加速比例。
- [出处与摘要](ci-provenance.json)：代码 revision、作业结论/测试数量、artifact ID、压缩包和归档文件 SHA-256。四份协议报告均来自 `working_tree_dirty=false` 的干净检出；Windows 仅规范化换行，JSON 值不变。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/smoke.py
python scripts/m2_smoke.py
python scripts/m3_smoke.py
python scripts/check_docs.py
```

测试仅操作一次性目录、合成邮件和临时 CA；客户端保持证书验证。不修改真实账号、系统信任根或用户的 Emacs 配置。Windows 不声称验证 Unix 管理行为。网络注入测试允许丢弃或关闭两种安全结果，确定性内存流测试单独证明预读缓冲被丢弃。

TLS 测试不是完整 RFC 条款一致性或全链路生产性能证明。存储仍按原顺序完成正文与目录同步、数据库接受事务，然后最终 250；没有数据库迁移，没有新增/升级第三方依赖。

四个工作区包升级到 0.4.0，第三方锁定项保持不变。[离线 RustSec 审计](rustsec-audit.json)使用 cargo-audit 0.22.2 和既有公告快照 `bd8037e5cbb8d8cc687c68cdd42ca53d742503fb`，读取 1256 条公告，已知漏洞/警告均为零。`--no-fetch --no-yanked` 不检查撤回版本，不代表完整依赖源码审计或不存在未知漏洞。

## 4. 已知限制与下一步

M3.1 完成入口和升级边界，并未完成整个 M3。Received/Return-Path、完整输入契约、更多状态/语法负例和阶段压力验收还需 M3.2/M3.3。IMAP literal 与 MIME worker 前置实验仍在路线图中，不能由本轮结果代替。

匿名收信仍缺域认证与反垃圾；只允许环回实验。TLS/认证混合负载 RSS、长期取消洪峰、物理存储掉电、生产备份恢复和 Emacs 全链路互通未验收。生产启动仍明确拒绝。
