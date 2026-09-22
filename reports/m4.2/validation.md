# M4.2 验证记录：固定上游 TLS 与原子混合提交

版本 0.8.0，schema 3。被测提交为 `899603994531269932579b01f61f7173f301bf26`；[CI 35710009454](https://github.com/ZEPHYR65537/rustymail/actions/runs/35710009454) 的 Linux、Windows、Linux 存储实验三个作业全部成功。后续提交仅整理教材、验收状态和原始证据，不改变本次被测代码、脚本或配置。教程与复现入口见[第 18 章](../../docs/18-m4-smtp-relay.md)。

本轮实现真实单次 SMTP 尝试、验证证书的隐式 TLS/STARTTLS、AUTH PLAIN、后台队列执行、本地/远端原子混合接受，以及提交 Bcc 过滤和共享 blob 的出站投影。全部网络实验只向自建环回对端发送合成内容，没有向外部邮箱发信。

## 1. 已执行的验证

| 范围 | 结果 |
| --- | --- |
| Rust 测试 | Linux 83 项、Windows 79 项；比 M4.1 新增 6 项 |
| 工程检查 | 两平台格式、严格 Clippy、锁定依赖构建、文档/配置附件检查通过 |
| M4.2 独立对端 | 两平台各 24 个 TCP/TLS 单次连接场景、5 组端到端集成检查 |
| M4.1 队列回归 | 两平台各 3 组检查、8 个真实进程强杀位置 |
| 既有协议回归 | 收信 17 项；M2 Linux 12/Windows 8 项；M3.1 Linux 11/Windows 10 项；M3.2 两平台各 6 项；M3.3 各 8 项，包含 48 行拒绝中继矩阵 |
| Linux 实验 | 大邮件 TLS 流式采样；4 组收信压力；基础存储 QEMU 11 个中断位置、2 个实际磁盘满场景 |

新增 Rust 测试覆盖混合接受的提交前后故障、配额回滚、事务内撤销授权、幂等重放；远端 local-part 大小写；正文按不同大小分块、EOF/格式失败不发结束点；实际网络取消及阻塞读取保护实例锁。它们是有限场景的回归证据，不是形式化证明。

独立对端覆盖 4xx/5xx、正文前断线、最终回复丢失/超时、畸形/超长/过多/分片回复、SIZE 和 8BITMIME 不兼容、两种 TLS、未知 CA/名称不符/过期证书、缺少 STARTTLS、升级后能力变化、AUTH 拒绝和 challenge。完整链路还验证匿名/未认证/冒充发件人不能取得远端责任，Bcc 折叠行不泄露，单 blob 的中继字节精确等于存储字节去除已验证首行。

## 2. 逐收件人结果与恢复

同一次提交包含一个本地邮箱和五个远端地址，独立数据库观察与上游记录一致：

| 远端收件人 | 首次结果 | 重启后 | 仅手动重试 defer 后的尝试次数 |
| --- | --- | --- | --- |
| Case@remote.test | delivered | 不重发 | 1 |
| case@remote.test | delivered | 不重发 | 1 |
| defer@remote.test | deferred | 等待持久退避时间 | 2，成功 |
| fail@remote.test | failed | 不重发 | 1 |
| lost@remote.test | uncertain | 不重发 | 1 |

delivered 只表示配置的上游在最终 DATA 回复中接受责任，不代表最终收件箱已收到。failed 尚未生成 DSN；到期、损坏或不兼容会保留责任并 hold。BODY 后未知结果进入 uncertain。

R05 在独立上游完整收到 DATA 后、发送最终回复前，强杀真实 rustymail 进程。重启后该任务为 uncertain，尝试次数仍为 1；没有再次发送正文。它属于进程故障，不是整机掉电。旧 QEMU 回归验证基础持久化，不能据此声称覆盖每个中继网络/磁盘组合故障。

首轮 [CI 35709257619](https://github.com/ZEPHYR65537/rustymail/actions/runs/35709257619) 的 Linux R05 暴露了恢复步骤缺口：Unix 管理 socket 遗留时，服务按既有策略拒绝覆盖路径。修正后的实验在确认子进程退出、核对 inode/所有者/私有目录后，显式清理自己创建的 socket。Linux 强杀恢复仍需管理员执行这一操作，不能称为无人值守恢复；步骤见第 18 章。Windows 没有这个 Unix 路径。

## 3. 大邮件发送的资源证据

Linux release 单次 TLS 探针使用 16 KiB 文件缓冲，比较约 1 MiB 与 25 MiB 正文；读取、点转义和对端还原后内容摘要匹配。内存单位为 KiB：

| 存储正文大小 / bytes | 采样实验耗时 / 秒 | 最大采样 RSS | 最大采样 HWM | 采样数 |
| --- | ---: | ---: | ---: | ---: |
| 1,048,056 | 0.0122 | 5,096 | 5,096 | 2 |
| 26,214,056 | 0.1763 | 5,080 | 5,080 | 28 |

采样间隔目标 5 ms；内存只包括 Rust 单次尝试进程，排除 Python 上游、队列、存储和 Argon2。实验耗时包含进程启动、DNS/TLS、独立对端处理及退出后的内容校验，不作为纯网络吞吐指标。采样峰值是观察下界，可能错过退出前的变化；HWM 是采样时内核记录的进程历史高水位。

本轮回归门槛为大邮件观察 HWM 小于 64 MiB、相对小邮件增加少于 8 MiB，均通过。这支持当前流式路径没有随消息大小全量缓冲的判断；不能证明完整服务资源上界。连接复用、大队列公平性、固定上游共享退避和长期负载仍待测。原有收信压力与存储实验单独归档，不与出站样本合并计算性能提升。

## 4. 原始证据与复现

- [Linux 中继](linux-relay.json)、[Windows 中继](windows-relay.json)、[Linux 出站流式采样](linux-relay-stream.json)。
- [Linux 队列](linux-queue.json)、[Windows 队列](windows-queue.json)。
- [Linux 输入契约](linux-contract.json)、[Windows 输入契约](windows-contract.json)。
- [Linux 本地交付](linux-delivery.json)、[Windows 本地交付](windows-delivery.json)。
- [Linux 三入口协议](linux-protocol.json)、[Windows 三入口协议](windows-protocol.json)。
- [Linux 身份回归](linux-m2-regression.json)、[Windows 身份回归](windows-m2-regression.json)。
- [Linux 收信压力](linux-pressure.json)、[存储故障](storage-regression.json)、[收信基准](linux-benchmark.json)、[分帧微基准](linux-framing.json)。
- [出处与 SHA-256](ci-provenance.json)：3 个作业、13 个产物压缩包、17 份原始报告；下载后验证产物摘要，仅规范化 JSON 换行为 LF。带 revision/dirty 字段的报告均对应上述被测提交，且 `working_tree_dirty=false`。

复现 M4.2 的命令见第 18 章；完整两平台和 Linux 专属实验命令在 [CI 工作流](../../.github/workflows/ci.yml)。报告来源对应干净检出，未用本地工作树中的临时结果代替。

本轮没有增加第三方依赖，仍为 153 个第三方包；Cargo.lock 仅更新四个工作区包版本。本轮没有重新刷新 RustSec 公告；[上一轮审计](../m3.3/rustsec-audit.json)只是历史快照，不能据此断言之后没有新公告。

## 5. 完成状态

M4.2 的 L0 固定上游中继、真实网络生命周期与原子混合接受已验收。schema 保持 3；配置上限与队列 API 对齐的兼容性收紧见教程。Emacs 配置继续保持 TLS 要求；完整 Gnus 收发仍等待 M5 的 IMAP 与客户端互通。

下一步 M4.3：失败通知唯一性、空 reverse-path 防循环、到期及 uncertain 的运维策略，再扩展 PIPELINING 和故障互通。当前仍非生产邮件服务器：没有 DSN、IMAP、域认证、反垃圾、生产自动恢复、真实外部互通或 72 小时 soak，生产启动入口继续拒绝。
