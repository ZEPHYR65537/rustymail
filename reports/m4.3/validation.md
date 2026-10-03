# 0.10.0 / M4.3 验证记录

本轮实现失败 DSN、schema 4 通知事务、到期索引、跨重试未知标记、人工结案/操作记录，以及客户端和服务器 PIPELINING。协议、资源不变量、迁移取舍和命令说明见[第 22 章](../../docs/22-m43-delivery-lifecycle.md)。当前仍为 L0 环回实验，不是生产发布。

## 本地验证

Windows / Rust 1.94.0：严格 Clippy、格式检查、锁定依赖的全 workspace/all-features **95 项 Rust 测试通过**。新增 9 项存储测试与 2 项传输测试：通知幂等/隐私、配额双重检查与恢复、全局暂存预算、故障点、外域报告/循环抑制、导入来源限制、独立到期/管理历史、旧通知迁移、旧重试不确定性，以及容量为 1 字节的双向部分写入/取消。

独立 Python 对端完成 15 种验证 TLS 的 PIPELINING 情形；六个通知位置实际结束子进程；接收端合并/逐字节命令；认证提交后的报告 MIME/配额/重启检查；未来重试时间不阻挡到期；最终回复丢失后人工重试再遇 550 仍保留 uncertain。所有实验使用一次性账户与环回端口，外部邮件数为 0。

旧版本的基础收信、TLS/身份、STARTTLS、本地交付、输入契约、持久队列、中继和统一 CLI 继续回归。M3.3 的精确能力集合已包含 PIPELINING；M4.2 混合提交现在同时检查新增的一份本地 DSN，不再把全库“只有原信一个 blob”当作断言。

原始证据：[Rust 测试](windows-tests.txt)、[Clippy](windows-clippy.txt)、[M4.3 对端和强杀](windows-lifecycle.json)、[混合中继](windows-relay.json)、[CLI](windows-cli.json)、[输入契约](windows-contract.json)、[队列恢复](windows-queue.json)。外域报告实验先在一次性配置中授予本地域身份，再移出 local_domains，覆盖既有责任遇到域配置变化的路径；没有放宽 CLI 对外域 send-as 的限制。

最新本地 [RustSec 审计](rustsec-audit.json)检查 158 个包（含 5 个工作区包），官方公告库提交 `f8dee89e1b2f2f1eaf548312df7655fe5202a302`，1,288 条公告，0 漏洞、0 警告；启用撤回版本检查。第三方依赖版本没有增加，仅给 store 接入已有 time 依赖。

初次实现的本地 release `rustymail.exe` 为 7,003,136 字节；时钟修复后最终版本为 **7,003,648 字节 / 6.68 MiB**，见[最终本地出处](final-local-provenance.json)。这不是 Linux 大小或运行内存。初次工具链、二进制及 LF 规范化源码/证据摘要保留于[出处记录](provenance.json)，它基于 `035ef02562b6aa6e89249b95533ebe8006646ace` 上的增量工作区，dirty=true，不冒充该基线提交的结果。

## 最终干净检出 CI

最终代码提交 **`98f023338be464a10ebd48b4d332dba76004d51d`** 的 [CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/37100532726) 与[独立安全工作流](https://github.com/ZEPHYR65537/rustymail/actions/runs/37100532742)全部通过。后续提交仅归档证据并补充教材，不改变该代码结果。

| 检查 | 实际结果／原始证据 |
| --- | --- |
| Linux 全量 Rust 与协议回归 | 100 项 Rust 测试；M4.3 的 15 个 TLS 对端场景、7 组集成检查、6 个通知强杀位置通过；[Linux 通知证据](ci-linux-lifecycle.json) |
| Windows 全量 Rust 与协议回归 | 96 项 Rust 测试；同样的独立对端及强杀实验通过；[Windows 通知证据](ci-windows-lifecycle.json) |
| 严格依赖审计 | 158 个包、1,288 条公告，0 漏洞/警告，且 stderr 为空；[审计 JSON](ci-rustsec-audit.json)、[出处](ci-security-provenance.txt)、[空诊断文件](ci-audit-stderr.txt) |
| Linux 有限压力 | 三入口共 120 次持久交付，连接/握手/DATA 取消及畸形流量恢复，最终存储健康；[压力记录](ci-linux-pressure.json) |
| Linux 流式传输与分帧 | 1 MiB/25 MiB 单次 TLS 发送及分帧基准通过；[流式记录](ci-linux-relay-stream.json)、[分帧记录](ci-linux-framing.json) |
| Linux VM 存储故障 | 13 个同步/提交/迁移/GC/已确认/SMTP 满盘/SQLite 满盘场景；无缺失或损坏的被引用正文、无 UID/配额/队列不一致；[VM 记录](ci-linux-storage.json) |
| Linux 收信基线 | 4 组有限收信负载；最终完整性通过；[基线记录](ci-linux-benchmark.json) |

全部下载 ZIP 均核对 GitHub 返回的 SHA-256；归档时只规范化文本换行，不改 JSON 数值。[CI 出处](ci-provenance.json)记录 artifact ID、ZIP/文件/日志摘要、任务链接及实际测试数量。适用脚本的 working_tree_dirty 均为 false；它们与本地增量工作区记录分开保存。

内存数据必须连同测量范围阅读：单次 TLS 探针发送约 1 MiB 与 25 MiB 时，采样 RSS 分别为 5,176 / 5,140 KiB，缓冲为 16 KiB，没有出现与整信大小对应的增长；该实验使用未宣告 PIPELINING 的顺序回退路径，排除 Python 对端、存储、队列和 Argon2，5 ms 采样峰值只是下界。三入口鉴权混合压力的完整 daemon 高水位则为 **140,240 KiB**，结束后 RSS 为 **9,412 KiB**，不能把传输探针约 5 MiB 当作整台服务的总内存。

VM 使用受控 QEMU/ext4 写回缓存模型，结束的是虚拟机进程，宿主与硬件仍运行；故障前发布但未引用的孤立文件可以保留待 GC。基线主机没有施加 2 GiB RAM 限制，宿主文件系统和 CI 时延也不代表目标 VPS。本轮不承诺生产吞吐、物理掉电保证或任意负载下固定总资源消耗。

## CI 发现与修复

初次提交 `4b13f04` 的 Linux 测试发现新增迁移 fixture 直接打开了权限较宽的临时目录，触发预期的 `UnsafePermissions` 防护；`cdf80e0` 改为使用存储层创建的私有子目录，未降低权限要求。本地新增测试通过；最终跨平台证据以下述干净检出为准。

同时检查安全工作流的原始 stderr，发现 cargo-audit 0.22.2 在部分 crates.io 索引项不存在、无法完成 yanked 查询时仍返回成功。不能把这样的绿色任务当作完整审计：修复后先 `cargo fetch --locked` 填充索引，并归档/拒绝非空 audit stderr。初次安全运行只证明公告扫描返回零漏洞，不能证明完整撤回版本检查；不采用它作为最终安全证据。本地审计已有完整索引，未出现上述诊断。

初次运行链接、日志摘要与修复提交保存在[修正记录](ci-corrections.json)，没有把失败运行覆盖成通过。

后续复查增加 `98f0233`：通知准备之后的提交及错误退避同样检查壁钟/单调时钟，防止中途时钟跳变把重试期限写到遥远未来。新增可控时钟回归并验证校正/重启后恢复。最终本地 Windows [96 项 Rust 测试](windows-final-tests.txt)及[严格 Clippy](windows-final-clippy.txt)通过；上面的 95 项日志保留初次实现的范围，最终代码版本以 CI 出处为准。

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
