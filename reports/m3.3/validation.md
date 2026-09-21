# M3.3 验证记录：输入契约与资源恢复

版本 0.6.0，L0 环回实验。被测功能提交为 `6f84af516510174649c06fb0990f573a12b6c1fe`；[CI 35617540710](https://github.com/ZEPHYR65537/rustymail/actions/runs/35617540710)的 Linux、Windows 和 Linux 存储实验三个作业全部通过。后续提交只整理教程、门槛和本报告，不改变被测代码、测试脚本或配置值。

## 交付范围

MAIL 保留 BODY 模式并检查 EHLO 协商；SIZE 验证 1–20 位十进制，大整数返回大小限制错误；独立命令长度预算；收件人上限处正确去重；HELP/VRFY 不改变信封；DATA 编码、framing、头部、超限、超时和暂存失败有明确分类。默认 7BIT 与 ASCII 头部策略比 0.5.0 严格，旧客户端示例已更新。

没有修改数据库 schema、已存储 blob 或默认部署参数，没有增加第三方运行时依赖。教程、完整支持子集契约、能力矩阵和生产兼容缺口见[第 16 章](../../docs/16-m3-input-contract.md)。

## 1. 已执行的验收

- Rust：Linux 65 项、Windows 61 项通过，包括新增五项参数、状态序列、去重和分片测试；格式、严格 Clippy 和构建通过。
- 独立客户端：原 SMTP 17 项；M2 Linux 12 组/Windows 8 组；M3.1 Linux 11 组/Windows 10 组；M3.2 各 6 组；M3.3 各 8 组全部通过。
- M3.3 契约：48 行角色/发件人/收件人矩阵；编码、长度、分隔符、事务重置与期限负例；两个接受事务重开导出后字节正确，其余输入不产生投递。
- 压力：连接与握手饱和、12 轮双 DATA 取消、三个入口共 120 封投递与 40 个畸形连接；Linux 的峰值内存与描述符恢复断言通过。
- 存储：11 个 Linux VM 中止位置及两种真实 ENOSPC 全部通过；已接受邮件、配额、UID 与恢复结果满足既有矩阵。
- 文档：本地链接、代码块、配置附件与 SQL 草案检查通过。本地 Windows 同时完成上述适用范围；Unix 管理、目录同步与 Linux 内存断言只按 Linux 证据计入。

原始证据：

- [Linux 输入契约](linux-contract.json)、[Windows 输入契约](windows-contract.json)：含完整 48 行矩阵及重开存储结果。
- [Linux 混合压力](linux-pressure.json)：逐角色延迟、容量重试、四阶段资源采样、120 个已引用文件的完整性结果。
- [Linux 本地交付回归](linux-delivery.json)、[Windows 本地交付回归](windows-delivery.json)。
- [Linux 入口/TLS 回归](linux-protocol.json)、[Windows 入口/TLS 回归](windows-protocol.json)。
- [Linux 身份回归](linux-m2-regression.json)、[Windows 身份回归](windows-m2-regression.json)。
- [Linux 存储故障](storage-regression.json)：ext4、512 MiB/2 CPU 来宾、QEMU 8.2.2；故障模型为杀死 QEMU、来宾内存丢失，宿主机和存储仍运行。
- [CI 出处与摘要](ci-provenance.json)：三个作业结论、Rust 数量、九个产物压缩包及十二份报告的 SHA-256；协议与压力报告均为 `working_tree_dirty=false`。Windows 换行只规范化为 LF，JSON 值不变。

## 2. Linux 资源恢复实测

使用 release、WAL/FULL、环回三入口和合成邮件；压力配置为 8 个连接、2 个握手、2 个接收槽、135168 字节临时预算。主进程数据如下，RSS/HWM 的单位均为 KiB：

| 时刻 | 当前 RSS | 生命周期 HWM | 文件描述符 |
| --- | ---: | ---: | ---: |
| 启动就绪 | 8256 | 71056 | 17 |
| 八连接饱和 | 8408 | 71056 | 25 |
| 握手饱和 | 8464 | 71056 | 19 |
| DATA 饱和采样 | 8556 | 71056 | 21 |
| 混合投递结束并排空 | 8804 | 139516 | 17 |

峰值约 136.2 MiB，低于本实验 256 MiB 回归上限；排空时约 8.6 MiB，描述符回到 17。HWM 记录的是全生命周期峰值，不会在释放后下降；启动本身为未知账号验证准备了一个 Argon2 占位散列，因此初始 HWM 也高于当时 RSS。不能把排空 RSS 当成认证期间内存，也不能用高水位推断尚未释放了同样多的内存。

P04 混合投递阶段耗时约 0.544 秒，含连接、两次实际认证和 120 封投递；不含启动及 P01–P03。三个客户端各接受 40 封，容量重试分别为 0、1、11 次。每封 MAIL 到最终成功的延迟包含重试：p50 2.88 ms，p95 22.46 ms，最大 183.02 ms；约 220.7 封/秒。只有 120 个小邮件样本，不能外推长期吞吐、每封新连接认证成本或 1–100 邮箱生产容量。

主进程测量不包含 Python 驱动、未来扫描/MIME worker 或 IMAP；该实验未固定两核亲和性，也未施加 2 GiB cgroup 限制。这里通过的是受控饱和、恢复和有限混合负载检查，不是已完成生产压力或长时间 soak。

## 3. 保留的匿名基准

[原始匿名收信数据](linux-benchmark.json)另使用两个 CPU 的进程亲和性，宿主为 AMD EPYC 9V74，文件系统 ext4；配置和逐封延迟在 JSON 内。不能把它与前面的混合 TLS 实验当作同一条件比较。

| 输入字节 / 并发 / 样本数 | 封/秒 | p99 毫秒 | 主进程 HWM KiB |
| --- | ---: | ---: | ---: |
| 958 / 1 / 100 | 124.5 | 149.15 | 7668 |
| 65518 / 1 / 100 | 351.4 | 17.02 | 7656 |
| 65518 / 4 / 100 | 462.0 | 81.77 | 7880 |
| 26214398 / 1 / 4 | 2.42 | 439.27 | 7536 |

首个小邮件样本出现较大长尾，不能依据这一轮短样本给邮件大小排序或断言实现加速/退化；本轮没有隔离长尾来源。CI 宿主与历史阶段不同，25 MiB 情形又仅四封，分位数不代表稳定分布。保留所有样本供后续同条件重复测量；[分帧微基准](linux-framing.json)也单独归档，不当作完整 SMTP 吞吐。

## 4. 复现与依赖

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
python scripts/m33_smoke.py
cargo build --workspace --release --locked
cargo build -p rustymail-server --example m2_certificates --release --locked
python scripts/smtp_pressure.py
python scripts/check_docs.py
```

Linux CI 另运行真实 Unix 对端拒绝检查和 [powercut.py](../../scripts/powercut.py)；其 QEMU/内核依赖与调用在[工作流](../../.github/workflows/ci.yml)。客户端保持 TLS 证书验证，不发送外部邮件，不使用真实凭据。

第三方依赖仍为 153 项，Cargo.lock 共 157 项含四个工作区包。工作区版本变化以外无锁定依赖变更。本地 [RustSec 结果](rustsec-audit.json)无已知漏洞或警告，使用 `cargo audit --no-fetch --no-yanked` 与公告快照 `bd8037e5cbb8d8cc687c68cdd42ca53d742503fb`，共 1256 条公告。它没有联网更新公告或检查撤回版本，不是未来公告、完整依赖审查或无漏洞证明。

## 5. 结论与剩余门槛

M3.3 的 L0 支持子集验收完成，可进入 M4.1 持久队列与逐收件人投递责任。quoted local-part、地址字面量、裸 Postmaster、问候/AUTH 兼容审查仍是公开的生产互通缺口，已列入发布门槛。外发、IMAP、域认证、反垃圾和生产运维仍待后续阶段。

此版本仍非生产服务器。有限分片/状态枚举不等于完整 fuzz 或形式化证明；短压力不等于长时间 soak；回归上限不是生产内存承诺；QEMU 中止不等于物理磁盘掉电。Emacs 配置保持既有 TLS 要求，完整 Gnus 收发验收等待 M5。
