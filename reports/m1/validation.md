# M1 验证记录

版本：0.2.0。范围：本地存储与恢复核心，仍为 L0 环回实验服务。M1 的代码、操作接口、教程与以下验收已落地；TLS、认证、IMAP、外发及整机生产验收尚未完成。操作说明见 [M1 教程](../../docs/11-m1-storage.md)。

## 1. 证据来源

日期：2026-09-21。首次完整 Linux 实验通过的提交为 `0b5f0e4635599f663cb40d6c220702a76b17a6a9`，对应 [GitHub CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/35589591749)。原始结果保留每次测量的 revision，不把后补文档的提交冒充被测代码。

最终代码 `f4b67ef2b179c597f734cf6df9a78332559236a9` 的[完整 CI](https://github.com/ZEPHYR65537/rustymail/actions/runs/35590062802)再次通过，包含迁移前缀校验修正。下表和主要 JSON 来自这一轮。报告取自该运行的 artifact `10634232039`，下载 zip 的 SHA-256 为 `76ed76a98c0c95ca75bdddc587fef10ad11e29f44132fb80ffa748a09cb77f19`；两个主要 JSON 原样归档，避免临时 artifact 到期后失去证据。

- [性能原始数据](linux-benchmark.json)：环境、逐封时延、进程内存/CPU、完整性检查。
- [虚拟机故障原始数据](vm-powercut.json)：客体内核摘要、QEMU 参数、13 个结果及磁盘满观察。
- [首次完整实验的性能样本](linux-benchmark-0b5f0e4.json)：保留独立 CI 运行中的波动，不挑选最好的一轮。

Rust 固定为 1.94.0；第三方依赖解析图与[首轮清单](../0.1.0-lab/dependencies.md)相同，共 98 个第三方包。新增代码复用了已有 serde/serde_json。RustSec 审计及 M0 的候选库/worker 验证仍待完成。

## 2. 代码与独立客户端检查

Windows 37 项、Linux 40 项 Rust 测试通过，格式和严格 Clippy 通过。Linux 多出的三项为 Unix 权限/链接与锁继承检查。测试函数数量不等于故障场景数量：多个测试内部遍历失败边界和退出位置。

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --locked
python scripts/smoke.py
python scripts/check_docs.py
```

| 验收项 | 已观察到的结果 |
| --- | --- |
| 14 个存储失败边界 | 无错误的成功返回；仅实际 COMMIT 后的失败保留已提交操作 |
| 提交结果未知 | 存储可有/无操作记录；真实 TCP 均关闭连接，不回复 250 或肯定未接收的 4xx |
| 六个原有进程退出点 | 重开数据库后正文与责任符合提交矩阵 |
| schema 1→2 | 事务失败保留旧版；成功保留已有 UID、消息 ID、长度、时间；校验结构和 SQL 摘要 |
| 迁移负例 | 未来版本、篡改摘要、未知表被拒绝，包括类似系统表前缀的 sqliteXextra |
| 离线 GC | 默认预览、批量有界、blob 行/备份 pin 保留、时钟回拨保守处理；中断后重扫可继续 |
| 正文恢复 | 只接受精确副本，不覆盖已有文件，不改 UID/配额；调低收信大小上限不妨碍旧信恢复 |
| 独立 Python SMTP/CLI | 收信后强杀重启、逐字节导出、内部操作查询、旧 schema 升级、GC/历史、缺失正文恢复及 checkpoint 全部通过 |

单元 hook 是边界错误模拟，不声称改变了内核 fsync 行为；实际磁盘满由下一节独立覆盖。

## 3. Linux VM 故障矩阵

客体：Linux `6.8.0-139-generic`，QEMU 8.2.2、TCG、2 vCPU、512 MiB RAM，每个场景新建 128 MiB ext4 raw 镜像，保留块为 0%。guest 二进制为 musl release；正常服务未启用故障 hook。使用 `cache=writeback`，flush 被遵守。注入点后 SIGKILL QEMU，同一镜像重启验证。

每个场景先保存一封基准邮件。下表的“邮件数”包括这封邮件；各场景最终均通过完整性检查，所有已接受正文逐字节一致，配额/UID/投递关联无不一致。

| 位置/场景 | 恢复后的邮件数 | 结果 |
| --- | --- | --- |
| staged | 1 | 未提交的新邮件不可见 |
| file_synced | 1 | 文件同步不等于接受 |
| renamed | 1 | 无数据库引用的文件不成为邮件 |
| directories_synced | 1 | 正文耐久仍需数据库事务 |
| before_commit | 1 | 未提交事务恢复为旧状态 |
| after_commit | 2 | 已提交责任与正文保留 |
| migration_applied | 1 | 重开前版本为 1，随后可重新升级 |
| migration_committed | 1 | 重开前版本为 2，已有邮件不受损 |
| gc_planned | 1 | 已接受邮件保留，未完成维护可识别 |
| gc_unlinked | 1 | 删除孤儿后的断电不损坏已接受邮件 |
| acknowledged | 2 | 真实 SMTP 客户端收到最终 250 后断电，邮件仍在 |
| smtp-full | 1 | 实际 errno 28 后最终回复 451，无新邮件 |
| sqlite-full | 1 | 实际 errno 28，接受结果报未知；重开查询确认新操作不存在 |

两个磁盘满实验均实际写满专用文件系统，出现 `errno=28`。SQL 场景先完成正文并截断 WAL，再写满磁盘；提交不能返回成功。处理错误后移除实验填充文件，重开并查询内部 operation ID。实验填充功能被限制在专用客体标记和不超过 512 MiB 的 `/data` 文件系统中。

这个模型丢弃客体内存，但宿主 OS 和存储始终运行，不能证明物理主机断电、故障控制器或不诚实的 flush 同样安全。Windows 不提供本实现要求的目录同步保证，仍只用于开发验证。

## 4. 首轮性能基线

测量为 native Linux release 二进制，CPU affinity 为两颗逻辑 CPU；具体型号、宿主内存、文件系统与 revision 见 JSON。CI 主机约 16 GiB 内存，没有施加目标 VPS 的 2 GiB 限额。每个场景独立新建存储，复用 SMTP 连接，以 MAIL/RCPT/DATA 到最终 250 计时，不包含首次连接/EHLO。

| 原文大小 | 封数/并发 | 封/秒 | MiB/秒 | p50 / p95 / p99（ms） | daemon VmHWM（KiB） |
| --- | --- | --- | --- | --- | --- |
| 958 B | 100 / 1 | 514.99 | 0.47 | 1.68 / 1.94 / 4.37 | 6900 |
| 65518 B | 100 / 1 | 409.42 | 25.58 | 2.01 / 2.59 / 17.68 | 7008 |
| 65518 B | 100 / 4 | 762.73 | 47.66 | 4.33 / 6.01 / 23.33 | 7152 |
| 26214398 B | 4 / 1 | 3.08 | 77.02 | 303.86 / 384.40 / 384.40 | 6892 |

接近 25 MiB 的正文没有带来同样大小的进程内存增长，符合流式处理预期。SQLite 包含在 daemon RSS 内，Python 驱动、宿主文件缓存和未来 TLS/MIME/反垃圾依赖不在这项指标中。

这些是很短的闭环负载，未预热、未施加固定到达率，也未解决 coordinated omission；它们用于记录起点，不验收[生产性能目标](../../docs/04-security-and-performance.md)。尤其四封大邮件的 p95/p99 都是最大样本，不能据此推断稳定尾延迟。首次完整实验中四并发比单并发更慢，短测与共享 CI 环境存在明显波动；尚无证据将其归因到某一个瓶颈，也不能宣称增加并发必然提升吞吐。

两轮主机型号不同：首次为 Xeon Platinum 8573C，最终为 EPYC 9V45。64 KiB 四并发的测值分别为 149.50 和 762.73 封/秒；期间的代码更改是启动时 schema 名称校验，不能把这个差异解释成优化收益。需要在固定环境下按长时间、多轮负载重新测量。

## 5. 本阶段的实际边界

M1 GC 不清除已接受消息或投递历史；缺失正文恢复要求精确副本，不等同于完整备份恢复。完整性扫描随存量正文增长，审计历史也尚无过期策略。完整 SMTP、MIME worker、认证/TLS、IMAP、出站队列、生产扫描、异机恢复和 72 小时稳定性仍有独立退出条件；不能因本阶段通过就接收真实邮件的唯一副本。
