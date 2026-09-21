# M3.2 验证记录：最终本地交付

版本 0.5.0，L0 环回实验。被测代码提交 `fa99159d8fadcb1b77911bd370bd86d36d51b65a`；[对应 CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/35612847184)的三个作业于 2026-09-21 全部通过。本记录的教程、配置注释与证据整理不改变被测实现或配置值，也不冒充另一份被测代码。

## 1. 交付范围与兼容性

三个 SMTP 角色及旧启动命令均在一个 staging 流中构造最终本地交付表示：生成本机 Received 和源自 MAIL FROM 的 Return-Path，移除旧 Return-Path 及其续行，保留其余字段和解码后的正文。客户端内容与最终大小独立计数，另预约最多 2 KiB，不复制整封邮件。操作 ID 和时间在 DATA 时生成一次，多收件人共享最终不可变文件。

导出语义明确为最终存储字节；恢复必须使用这一文件的精确副本。schema 仍为 2，历史 blob 不重写。默认 400 MiB 临时预算能容纳 15 个 25 MiB 加头部预约；自定义预算不得小于单次完整预约。未测试旧二进制处理新增最大存储表示的回滚兼容性。

配套[教程](../../docs/15-m3-local-delivery.md)解释表示、信任、流式过滤、预算、配额及重放；README、协议/存储契约、教材目录、历史章节和 ADR 同步维护。

## 2. 要求、策略与测试映射

| 要求或策略 | 实现与验证 |
| --- | --- |
| RFC 5321 §4.4：新增本跳追踪，保留已有 Received | 独立客户端验证此前 Received 连续行原样保留；Rust 单元检查实际 IPv6 对端、UTC 时间和字段顺序 |
| 最终交付的 Return-Path 来自反向路径 | 大小写不同、重复及折叠旧字段被移除；空 MAIL FROM 生成空路径；正文同名字符串保持原样 |
| 项目策略：不在共享字段泄露信封收件人 | 新 Received 不含 for 或账号；真实 TCP 测试两个邮箱引用相同消息，重复 RCPT 不增加交付 |
| RFC 1870：SIZE 不是帧长度 | 声明 SIZE=1、实际恰好到上限仍接受；上限只计点解码后的客户端内容；超限关闭 |
| 输入检查不能被变换绕过 | 删除前计数，重复 Return-Path 的字节仍受头部上限限制；畸形字段名、无字段续行和控制字符均拒绝 |
| 固定新增开销不能越过磁盘预算 | 最长受支持地址与 IPv6 前缀边界；配置拒绝仅容纳客户端上限的预算；6144 字节预算只允许一次 4096+2048 预约 |
| 最终大小、摘要、配额必须一致 | 独立客户端重开存储，核对导出、SHA-256、数据库字节数、各邮箱 used_bytes；新增字段导致配额不足时所有收件人均不可见 |
| 内部幂等依赖最终表示 | 重放同一 operation ID 和最终字节返回原消息；改变最终字节报冲突；外部同内容重发产生新事务 |
| 新表示仍满足持久性责任 | 保留原有 TCP 提交故障、进程强杀与精确副本恢复；Linux VM 中 SMTP 候选包含追踪，存储 API 种子仍为原字节 |

规范链接和项目取舍见教程。结构过滤不等于完整 RFC 5322/MIME 校验；保留 DKIM-Signature 字节不等于验证签名。

## 3. 已执行检查与复现

两平台干净检出：Linux 60 项、Windows 56 项 Rust 测试、格式、严格 Clippy、构建通过；原 SMTP 17 项、M2 Linux 12 组/Windows 8 组、M3.1 Linux 11 组/Windows 10 组及新 M3.2 各 6 组独立客户端检查通过。M3.2 接受 7 个 SMTP 事务，负例不留下投递记录。本地 Windows 也完成同范围回归。

- [Linux M3.2](linux-delivery.json)和[Windows M3.2](windows-delivery.json)：最终交付与大小、共享、配额、摘要检查。
- [Linux M3.1 回归](linux-protocol.json)和[Windows M3.1 回归](windows-protocol.json)：三个入口、STARTTLS、全局额度及新追踪标记。
- [Linux M2 回归](linux-m2-regression.json)和[Windows M2 回归](windows-m2-regression.json)：身份、TLS、秘密日志；Linux 额外验证 Unix 管理。
- [Linux 存储故障](storage-regression.json)：11 个 QEMU 中止位置、SMTP/SQLite 两种真实 ENOSPC。已接受邮件、配额及 UID 一致。故障模型是 QEMU 被杀、来宾内存丢失，宿主机仍运行，不等于物理设备掉电。
- [出处与摘要](ci-provenance.json)：三个作业结论、测试数量、七个产物压缩包和九份结果的 SHA-256；六份协议报告均为 `working_tree_dirty=false`。Windows 报告只规范化换行，不改变 JSON 值。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/smoke.py
python scripts/m2_smoke.py
python scripts/m3_smoke.py
python scripts/m32_smoke.py
python scripts/check_docs.py
```

所有客户端只操作临时目录、合成数据与环回端口；TLS 客户端保持证书验证。原 M3.1 回归还核对 ESMTP、ESMTPS、ESMTPSA 与各入口和信封的对应关系。Windows 不声明通过 Unix 管理实验。

本轮[匿名收信基准](linux-benchmark.json)使用 release、WAL/FULL、两个 CPU 的进程亲和性，包含新增本地追踪字段。下表的大小是客户端输入，内存是主进程 VmHWM，不含 Python 驱动；没有强制 2 GiB 内存上限。

| 客户端字节 / 并发 / 样本 | 吞吐（封/秒） | p99（毫秒） | VmHWM（KiB） |
| --- | --- | --- | --- |
| 958 / 1 / 100 | 378.1 | 43.5 | 7764 |
| 65518 / 1 / 100 | 176.4 | 74.2 | 7628 |
| 65518 / 4 / 100 | 382.7 | 49.9 | 8016 |
| 26214398 / 1 / 4 | 2.38 | 704.2 | 7648 |

这是 AMD EPYC 9V45 CI 宿主机上的短样本，复用环回明文连接，延迟计 MAIL 到最终 250，不含初始连接/EHLO。最后一项只有四封，分位数不能代表稳定分布。不同阶段 CI 宿主机不同，不能据此计算实现加速比例。另保留[独立分帧样本](linux-framing.json)，不把微基准当成完整接受吞吐。TLS/认证、扫描与长期压力仍须另测。

## 4. 依赖与证据边界

153 个第三方锁定项完全不变；已有 `time 0.3.55` 新增为直接运行时依赖并启用 formatting，避免手写日历和日期格式。四个工作区包升级为 0.5.0。以 Rust 1.94.0 构建，未提高工具链要求。

[RustSec 审计](rustsec-audit.json)使用 cargo-audit 0.22.2 与公告快照 `bd8037e5cbb8d8cc687c68cdd42ca53d742503fb`，读取 1256 条公告，已知漏洞/警告为零。`--no-fetch --no-yanked` 不检查新于该快照的公告或撤回版本，也不代替完整依赖源码审计。

本阶段没有完成 M3.3 的完整协议负例/压力门槛，也没有实现队列、IMAP、DKIM 验证或反垃圾。历史匿名明文基准不能证明 TLS/认证混合负载性能，VM 中止不能证明物理磁盘掉电可靠性。生产启动仍拒绝，Emacs 完整收发互通仍等待 M5。
