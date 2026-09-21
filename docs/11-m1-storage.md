# M1：把故障处理变成可以验证的行为

本章对应 0.2.0 的存储与恢复核心。首先运行 [README](../README.md) 的本地收信实验。本阶段继续只允许 L0 环回 SMTP；没有 TLS、登录、IMAP 或外发。验证完成状态以 [M1 报告](../reports/m1/validation.md)为准。

## 1. 运维前先取得唯一访问权

以下命令在仓库根目录执行，示例使用 debug 二进制。生产优化构建对应 `target/release/`。先停止 `rustymaild`；管理程序使用相同的独占实例锁，在服务仍运行时会拒绝进入。

```sh
target/debug/rustymailctl --config deploy/rustymail.lab.toml check-store
target/debug/rustymailctl --config deploy/rustymail.lab.toml migrations
```

检查命令不会因为数据目录拼错就创建一个空邮箱。缺失数据库会报错。打开已存在的 schema 1 数据库时，会校验结构并执行事务升级；`migrations` 因此不是完全不修改磁盘的探测命令。升级前保留一致副本；旧二进制会拒绝 schema 2，不支持直接回退版本号。

`check-store` 同时检查 SQLite 完整性/外键、正文长度与摘要、账户用量、UIDNEXT 与事件序号，以及本地投递引用的一致性。`healthy=false` 时命令失败；不会通过删除坏引用让检查“变绿”。完整扫描耗时随存量正文增长，当前尚未实现快速启动与后台 scrub 的分离。

## 2. 提交结果未知时，查询事实

存储提交和网络响应是两件事。数据库已经提交而结果通道失效时，回复“没有接收”会制造矛盾。服务在这一情况关闭连接，并记录只含内部 ID 的 `acceptance_outcome_unknown` 日志。

```sh
target/debug/rustymailctl --config deploy/rustymail.lab.toml operation OPERATION_ID
```

从日志复制真实 `operation_id`，不要照抄占位值。成功查到时返回 message ID、blob ID、字节数、摘要、接受时刻和收件人数。`operation=null` 表示在取得独占锁、此前在途工作已结束之后，数据库中没有该内部操作。它不证明发送方从未重试，也不消除外部 SMTP 的重复窗口。

已提交日志在尝试发送最终 250 之前写出，所以客户端断连也不会掩盖已经发生的提交。日志不记录信封地址、正文或认证负载。SMTP 的最终响应仍以数据库实际完成提交为前提。

## 3. 离线垃圾回收：默认预览，执行时重新判断

```sh
target/debug/rustymailctl --config deploy/rustymail.lab.toml gc
target/debug/rustymailctl --config deploy/rustymail.lab.toml gc --apply
target/debug/rustymailctl --config deploy/rustymail.lab.toml gc-history
```

默认宽限期为 86400 秒，每次最多处理 1000 个候选；`--limit` 可设为 1–1000，`--min-age-seconds` 可改变宽限期。较短宽限期适合合成故障实验，日常维护应保留诊断时间。输出逐项 JSON 和最终统计，不把全部候选一次载入内存。

GC 只删除两类文件：超出宽限期的 `staging/*.part`，以及数据库 `blob` 表完全没有对应记录的 `blobs/*.eml`。只要 blob 行存在，就保守保留，包括邮箱、消息历史、队列和备份 pin 的引用。M1 不删除邮箱邮件、不按年龄删除已接受消息，也不实现未来投递历史的 30 天过期策略。

开始前必须没有 staged/prepared 令牌，并且完整性检查通过。预览不是稍后删除的授权清单；执行时重新计算时间、引用和文件状态。修改时间在未来的文件被保留，时钟回拨不会使它们提前过期。未知文件名、符号链接和存储不一致导致停止，不会被当作垃圾清理。

每项执行顺序是：

```text
记录 planned 并持久提交 → 再核对引用/文件状态 → unlink
→ 同步目录 → 将动作记为 deleted
```

中断可能留下 `planned`，甚至对应文件已经不存在。下次打开存储会把未完成 run 标为 `interrupted`；重新执行 GC 扫描当前事实，不盲目重放旧删除清单。统计中的 `deleted` 是已经同步目录并记录完成的数量，不代表未完成动作一定没有删除文件。

维护记录也占空间，M1 暂不自动过期这些审计行。磁盘完全用尽、连维护记录都无法提交时，GC 会失败；空间预留是预防措施，不是文件系统配额保证。

## 4. 从精确副本恢复缺失正文

通过 operation 查询找到 blob ID，或从已保存的备份清单定位。仅当正文路径不存在时可以恢复：

```sh
target/debug/rustymailctl --config deploy/rustymail.lab.toml recover-blob BLOB_ID --source verified-copy.eml
target/debug/rustymailctl --config deploy/rustymail.lab.toml check-store
```

恢复过程以小缓冲读取副本，验证长度与 SHA-256，先同步临时文件，再用不可覆盖的同文件系统硬链接发布目标，同步目标目录，最后清除临时名字并同步 staging 目录。已有文件一律拒绝覆盖；内容错误、过大或摘要不同的副本不会成为可见正文。

这个命令不重建数据库、不回退 UID、不重新接受邮件、不更改账户配额。损坏但仍存在的文件要先保存取证副本并由管理员进行明确的隔离处理；M1 不自动移动或删除它。没有精确副本时，摘要本身无法重建邮件。完整异机备份、恢复旧快照后的 UIDVALIDITY 策略和恢复时限仍属于 M7。

`checkpoint` 可在离线维护时截断已检查点的 WAL；它调用 SQLite 的 checkpoint 接口。不能手工删除 WAL/SHM 来“清理”数据库。

## 5. 迁移不是只改一个版本号

[migration.rs](../crates/store/src/migration.rs)对照内置 schema 校验 SQLite 对象结构，对迁移 SQL 记录版本、名称、SHA-256、时间和是否从旧版本接纳。换行格式统一后计算摘要，以便跨平台一致。

schema 1 的接纳要求结构匹配；未知表、丢失对象、未来版本或错误摘要都被拒绝。升级的 DDL、迁移记录和 `user_version` 放在同一事务中。事务前半段失败时一起回滚，提交后结果丢失则重新打开并查询实际版本。程序没有自动降低版本的路径。

当前升级不改写已有消息表，独立 CLI 实验验证升级前后的 UID、消息 ID、长度和接受时刻保持一致。SQLite 的版本字段由应用解释，无法代替结构校验与迁移业务规则。[SQLite PRAGMA](https://www.sqlite.org/pragma.html#pragma_user_version)

## 6. 三层故障实验，不能混为一谈

| 层次 | 可以证明什么 | 不能替代什么 |
| --- | --- | --- |
| 可控 I/O 边界失败 | 写入、同步、rename、SQL 及未知结果分支不会错误返回成功 | 真正的内核/磁盘故障 |
| 真正进程退出 | WAL 恢复、应用析构未执行时的责任与正文关系 | OS 页缓存丢失 |
| QEMU 客体断电与小文件系统写满 | 丢弃客体内存后重启；实际 ENOSPC；文件系统与应用共同恢复 | 物理主机断电、故障控制器或不诚实的 flush |

[StorageRuntime](../crates/store/src/runtime.rs)提供统一 UTC clock，网络超时仍使用 monotonic clock。注入 hook 只在单元测试或显式 `test-support` feature 中存在，正常二进制没有故障配置键或环境开关。每个实例持有自己的 hook，测试之间不会通过全局变量串扰。

```sh
cargo test --workspace --all-features --locked
python scripts/smoke.py
```

纯存储测试覆盖 14 个失败边界、提交前后两种未知结果、迁移中断、GC 中断、保护保留对象和时钟回拨。真实 TCP 用例检查：文件故障返回临时失败；提交结果未知时直接 EOF，不能返回 250 或声称肯定未接收的 4xx。独立 Python 客户端还检查旧 schema 升级、GC、恢复和原文逐字节一致性。

## 7. Linux 虚拟机实验与性能基线

[powercut.py](../scripts/powercut.py)创建独立 128 MiB ext4 镜像、512 MiB 内存的 QEMU 客体。在写入、文件同步、rename、目录同步、提交前后、迁移和 GC 的指定点直接杀死 QEMU；另通过客体内真实 SMTP 客户端收到最终 250 后再断电。每次用同一镜像重新启动并检查正文、责任数量和迁移版本。

磁盘满用例只允许在带专用内核标记的实验客体及受限 `/data` 文件系统内执行，分别覆盖 SMTP 正文同步和 SQLite 提交。脚本不挂载宿主磁盘、不访问物理块设备、不创建网络接口，也不针对已有邮箱运行。

客体磁盘使用 `cache=writeback` 并遵守 flush；宿主 OS 和存储保持运行。这是明确的虚拟机故障模型，不是物理掉电认证。[QEMU 磁盘缓存选项](https://www.qemu.org/docs/master/system/invocation.html)说明了不同模式的含义。

[benchmark.py](../scripts/benchmark.py)针对 native Linux release 二进制，以两颗 CPU 的 affinity 运行环回 SMTP。覆盖约 1 KiB、64 KiB、接近 25 MiB 的邮件以及四并发连接；保存逐封时延、p50/p95/p99、吞吐、daemon VmHWM、CPU 时间和环境。每个场景都创建新存储并在结束后完整校验。

测试驱动与服务内存分开计量；SQLite 在服务进程内，计入 daemon RSS。没有加入 TLS、MIME 工作进程或反垃圾依赖，也没有把 CI 主机伪装成 2 GiB VPS。数字用于定位下一轮优化，不能当作生产容量承诺。

工作流和安装依赖见 [CI](../.github/workflows/ci.yml)。完整运行需要 Linux、QEMU、匹配内核模块、静态 BusyBox 和 musl 编译器；结果与原始文件进入 `m1-linux-evidence` artifact。只有实际通过并保存的报告才计入里程碑验收。
