# M4.1 验证记录：持久队列基础

版本 0.7.0，schema 3，L0 离线队列与环回 SMTP 实验。被测功能提交为 `6c1d0bdb18c8f17876dfdfde413c1fdcbbc5534c`；[CI 35684324229](https://github.com/ZEPHYR65537/rustymail/actions/runs/35684324229) 的 Linux、Windows 和 Linux 存储实验三个作业全部成功。后续提交仅整理教材、验收状态和本轮原始证据，不改变被测代码、脚本或配置。

本轮实现原子入队、逐收件人责任、租约所有权、有限索引扫描、正向抖动退避及不确定结果；没有网络外发、混合收件人 SMTP 接受或 DSN。原理、升级与操作见[第 17 章](../../docs/17-m4-durable-queue.md)。

## 1. 已执行的验证

| 范围 | 结果 |
| --- | --- |
| Rust 测试 | Linux 77 项、Windows 73 项；新增 12 项队列测试 |
| 工程检查 | 两平台格式、严格 Clippy、锁定依赖构建通过 |
| 新增队列独立脚本 | 各 3 组检查；各 8 个真实进程终止位置 |
| 原有独立 SMTP | 17 项检查通过 |
| M2 身份/TLS | Linux 12 组、Windows 8 组 |
| M3.1 入口/TLS | Linux 11 组、Windows 10 组 |
| M3.2 本地交付 | 各 6 组 |
| M3.3 输入契约 | 各 8 组，包含 48 行角色/身份/收件人矩阵 |
| Linux 压力 | 4 个场景、120 封本地投递、40 个畸形连接及资源恢复 |
| Linux 存储 | 11 个 QEMU 中止位置及 2 种真实 ENOSPC；重开后 schema 3、完整性正常 |
| 教材附件 | Markdown 链接、代码块、TOML 和 SQL 草案检查通过 |

本地 Windows 同时通过适用的工程、协议及队列实验；最终证据使用 CI 的干净检出结果。Unix 管理、目录同步、Linux 资源采样仅计入 Linux 证据。

新增 Rust 测试覆盖：外域 local-part 大小写、严格幂等及本地/外发操作隔离；逐人 2xx/4xx/5xx/断连分类；活租约与读者保护实例锁/GC；过期仍活跃的任务不重复领取；旧 generation 拒绝；提交前后错误；时钟前跳/回拨及已知结果保存；131 条跨批扫描、全局/每域额度和查询计划；正文同长度损坏；schema 2→3 原子升级且保留本地 UID；正向抖动边界；多收件人入队原子性；跨表损坏检查。每条“通过”对应有限场景，不是穷尽输入或形式化证明。

## 2. 队列强杀与证据来源

| 阶段 | 提交前强杀后恢复 | 提交后强杀后恢复 |
| --- | --- | --- |
| 入队 enqueue | 无任务；允许留下可回收 orphan | pending |
| 领取 claim | pending | deferred |
| 正文边界 body | deferred | uncertain |
| 结果 finish | uncertain | delivered |

父进程等待故障标记后使用 SIGKILL/TerminateProcess；重开前直接观察 SQLite，重开后检查 API 状态和正文完整性。uncertain 不自动领取；hold/uncertain 显式 retry 要求接受重复风险。结果分类由合成探针输入，没有真实上游。

- [Linux 队列](linux-queue.json)、[Windows 队列](windows-queue.json)：CLI、分页、幂等冲突、GC 引用保护及八个强杀位置的持久/恢复状态。
- [Linux 输入契约](linux-contract.json)、[Windows 输入契约](windows-contract.json)：0.7.0 仍拒绝外域 RCPT，未因新增入队 API 开放中继。
- [Linux 本地交付](linux-delivery.json)、[Windows 本地交付](windows-delivery.json)。
- [Linux 入口/TLS](linux-protocol.json)、[Windows 入口/TLS](windows-protocol.json)。
- [Linux 身份回归](linux-m2-regression.json)、[Windows 身份回归](windows-m2-regression.json)。
- [Linux 压力](linux-pressure.json)、[存储故障](storage-regression.json)、[匿名收信基准](linux-benchmark.json)、[分帧微基准](linux-framing.json)。
- [CI 出处与摘要](ci-provenance.json)：3 个作业、11 个产物压缩包、14 份原始报告及 SHA-256。归档前校验 GitHub 产物摘要；含 revision/dirty 字段的报告均匹配本功能提交且 `working_tree_dirty=false`。仅将 Windows 换行规范化为 LF，JSON 值不变。

Linux QEMU 来宾为 512 MiB、2 CPU、ext4；故障模型是中止 QEMU、丢失来宾内存，宿主存储继续运行。此次已有迁移实验覆盖 schema 1→3 的提交前后；schema 2→3 使用 Rust 故障测试。**八个新增队列位置使用进程强杀，没有专门对每个位置执行 VM/物理掉电。** 不能把基础存储回归当作新增队列的完整掉电证明。

## 3. 资源与性能的实际含义

已有 SMTP 混合压力脚本在 release、WAL/FULL 下运行，配置为 8 个连接、2 个握手、2 个接收槽和 135168 字节临时预算。主进程采样如下，内存单位 KiB：

| 时刻 | RSS | 生命周期 HWM | 文件描述符 |
| --- | ---: | ---: | ---: |
| 就绪 | 8296 | 71056 | 17 |
| 八连接饱和 | 8448 | 71056 | 25 |
| 握手饱和 | 8524 | 71056 | 19 |
| DATA 饱和 | 8636 | 71056 | 21 |
| 混合投递结束并排空 | 8848 | 139768 | 17 |

峰值约 136.5 MiB，低于本实验 256 MiB 回归上限；排空约 8.6 MiB，描述符恢复为 17。HWM 不会随释放下降，包含 Argon2 分配高峰，不能把排空 RSS 当作整个运行期内存。

P04 三入口合计 120 封的有限混合阶段约 0.263 秒，约 456.8 封/秒；每封 MAIL 至成功（含重试）的 p50 3.00 ms、p95 4.93 ms、最大 14.17 ms。该阶段不含启动及前三个饱和场景。匿名收信与 framing 基准另存原始数据，不与混合 TLS/认证样本合并。

这批邮件仍是本地交付，没有后台外发。**这些数字不能当作队列吞吐或队列内存测量。** 新队列的证据是最多 128 候选的索引扫描、最多 128 活跃 guard、流式正文校验及跨批进展；大存量、公平调度、长期队列和实际发送的内存/吞吐仍待后续实验。主进程采样不包含 Python、未来 MIME worker 或 IMAP；本次宿主 CPU 与 M3.3 不同，不能据两轮短样本宣称性能翻倍。生产 2 vCPU/2 GiB 基线与 72 小时 soak 尚未验收。

## 4. 复现与依赖

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
cargo build -p rustymail-store --example m41_probe --features test-support --locked
python scripts/smoke.py
python scripts/m2_smoke.py
python scripts/m3_smoke.py
python scripts/m32_smoke.py
python scripts/m33_smoke.py
python scripts/m41_smoke.py
python scripts/check_docs.py
```

Linux 的压力、基准、静态探针及 QEMU/内核依赖和命令见[CI 工作流](../../.github/workflows/ci.yml)。全部实验使用合成内容；队列脚本不会外发邮件。

本轮没有新增第三方依赖，Cargo.lock 只变更四个工作区版本，仍为 153 个第三方包。本轮没有重新刷新 RustSec 公告；上一轮的[审计快照](../m3.3/rustsec-audit.json)仅作为依赖未变的历史资料，不能据此保证此后没有新公告。

## 5. 完成状态与下一步

M4.1 的持久队列基础已验收，可进入 M4.2。下一步是固定上游 TLS 客户端、真实逐 RCPT/DATA 响应映射、网络取消生命周期，以及接受侧本地/外发表示和原子混合责任。M4.3 再完成通知幂等、空发件人循环防护与长期失败处理。

当前仍非生产邮件服务器：没有实际外发、DSN、IMAP、域认证或反垃圾，仍为 L0；known-failed 记录不代表已通知发件人。队列内容目前由可信离线 API 导入，未作为公开提交路径。Emacs 维持原有 TLS 配置，完整 Gnus 收发互通等待 M5。
