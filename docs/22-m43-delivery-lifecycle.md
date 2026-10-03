# M4.3：失败通知、不确定结果与 SMTP 流水线

本章对应 **0.10.0、数据库 schema 4**。前置阅读是[持久队列](17-m4-durable-queue.md)、[真实中继](18-m4-smtp-relay.md)和[统一 CLI](21-unified-cli.md)。本轮补上 M4 的失败生命周期：永久失败与到期通知、通知循环保护、人工处理记录、收发两端 PIPELINING。服务端仍是环回 L0 实验；IMAP、公网安全策略和生产运维尚未完成。[验证报告](../reports/m4.3/validation.md)区分本地实验与 CI 证据。

学习目标：能画出文件发布与数据库提交的时间线；解释为什么“已通知”还需要另一项投递责任；解释未知结果为何必须跨重试保留；能够用独立对端证明流水线没有越过 DATA 的确认边界。

## 1. 接受原信之后，服务器还欠谁什么

Alice 提交一封信给 Bob 和 Carol。Bob 接收成功，Carol 的服务器返回 550。Alice 先前得到的最终 250 只说明本机承担了责任；此时要保留 Bob 的成功，再为 Carol 建立失败通知，不能把整封信重新发送。

本轮每个失败收件人产生一份报告。报告只包含该收件人的结果，既不等待其他任务结束，也不泄露其他信封收件人。`notification(delivery_id, kind)` 的唯一键把“应通知一次”约束在持久层，而不是依赖进程中的布尔变量。

三个事实必须分开：原投递 `failed` 表示失败已经确定；`notification_state=created` 表示通知及其投递责任已经提交；只有报告自己的 delivery 到达 `delivered`，才表示本地邮箱已收到或远端上游已接受。后者依然不保证人类读过邮件。

## 2. 状态转移：知道什么，就记录什么

下面是当前队列的运行规则。`possibly_delivered` 表示历史中存在一次没有排除远端接受的尝试；它不是当前 socket 的状态。

| 当前状态／事件 | 后续状态 | 后续动作 |
| --- | --- | --- |
| pending/deferred，尚未到期且有并发额度 | leased | 持久记录 token/generation，然后执行一次网络尝试 |
| leased，最终 DATA 2xx | delivered | 记录已接受；不再重发该责任 |
| leased，已知 4xx、连接前故障或配置故障，历史无未知结果 | deferred | 退避；到期扫描仍独立运行 |
| leased，已知 5xx，历史无未知结果 | failed | 待生成失败通知 |
| leased，本地内容或能力不匹配 | hold | 留给管理员修复；不能解释为对方永久拒绝 |
| leased，正文阶段丢失最终结果 | uncertain | 不自动重发，也不生成断言失败的通知 |
| leased，进程失去所有者 | deferred 或 uncertain | ready 阶段且无历史未知结果才安全重试；body 阶段保守记未知 |
| pending/deferred 到期，历史无未知结果 | failed | 到期报告，状态码 5.4.7 |
| pending/deferred 到期，历史有未知结果 | uncertain | 保留未知事实；不发送失败报告 |
| 尝试计数无法递增 | hold | `lifecycle_reason=counter`，不能溢出或伪造失败 |
| pending/deferred/uncertain/hold，人工暂停 | hold | 原子写入管理记录；保留历史未知标记 |
| deferred，人工 retry | pending | 已到期还需 `--extend-expired` |
| hold/uncertain，人工 retry | pending | 还需 `--allow-duplicate`；保留历史未知标记 |
| uncertain，或带未知标记的 hold，人工结案 | uncertain + closed_at_ms | 必填原因；关闭待办，不改写投递事实 |
| delivered/failed/已结案 | 保持原态 | 普通 hold/retry 拒绝；原文与历史不自动删除 |

关键反例：第一次发完正文后断网，管理员允许重试，第二次收到 550。第二次的拒绝不能证明第一次没有成功。因此后续非成功结果仍归入 uncertain；只有新的最终 2xx 能证明至少一次接受。即使后来成功，重复风险的管理记录也不删除。

`closed_at_ms` 只表示管理员处理了待办。若重新发送是业务决定，应建立新的邮件责任并保留关联说明；当前没有自动复制原责任的命令，也没有“强制标记已送达”按钮。

## 3. 事务 outbox：先保存正文，再提交通知责任

这里的 outbox 是一种设计方法：先在本机持久记录要完成的副作用，再由可重入的工作器执行。它不是一个保证网络恰好一次的数据库功能。

当前采用两个可恢复步骤：投递结果事务先将原责任记为 failed/pending-notification；随后通知事务补齐报告。进程在两者之间退出，只会留下可扫描的未通知责任。

```text
原投递 failed，notification_state=pending
  → 检查资格、收件人、配额与预算
  → 生成至多 16 KiB 的报告
  → 写暂存文件 → sync_all → rename → 同步相关目录
  → BEGIN IMMEDIATE
      再检查原责任及本地配额
      插入 blob、source=dsn 的 message
      插入报告的 delivery
      若为本地邮箱：同时分配 UID、追加事件、计入配额
      插入唯一 notification 关联
      原责任 notification_state=created
    COMMIT
```

SQLite 负责这一个数据库事务中的全有或全无；它不能替外部文件执行目录同步，也不能撤销远端 SMTP 的接受。文件先发布的代价是可能留下孤立文件；数据库先引用一个没有持久化的文件则可能让已经确认的消息丢正文，因此不能交换顺序。

重复生成也可能发布两个不同的报告文件，但同一失败责任只会提交一个 report message、一次本地 UID/配额或一项远端投递责任。后来的提交先查唯一关联并返回已有结果，未引用的文件由现有离线 GC 处理。**逻辑去重不等于磁盘没有垃圾文件，更不等于远端恰好收到一次。**

返回之前发生 I/O 错误并不能证明 COMMIT 失败。`notification_failed` 只更新仍为 failed/pending 的记录；如果提交实际已完成，错误处理不能把 created 改回 pending。重启扫描与唯一关联才是恢复依据。

源码：[通知事务与生成器](../crates/store/src/notification.rs)、[存储发布顺序](../crates/store/src/blob.rs)、[后台通知工作器](../crates/server/src/relay.rs)。

## 4. DSN 不是把错误字符串发回 From

按 [RFC 3464 §2](https://www.rfc-editor.org/rfc/rfc3464#section-2) 生成 `multipart/report; report-type=delivery-status`，包含可读说明和 `message/delivery-status`。省略可选的原邮件附件。

| 字段 | 当前来源与含义 |
| --- | --- |
| Reporting-MTA | 经过配置校验的 `hostname`，类型 dns |
| Final-Recipient | 该责任的原始收件人；远端 local-part 大小写保留 |
| Action | failed |
| Status | 已知 SMTP 永久失败用通用 5.0.0；本机投递期限耗尽用 5.4.7 |
| Diagnostic-Code | 已校验的 SMTP 数字码或固定本地诊断；不复制对端任意文本 |
| Final-Log-ID | 本机 delivery ID，供管理员关联 |
| To／信封目标 | 原 SMTP reverse-path，绝不取邮件头 From |
| From | `postmaster@第一个本地域`；中继启动时校验可构造合法地址 |
| MAIL FROM | 空反向路径 `<>`；本地报告也写入 `Return-Path: <>` |

状态码来自 [RFC 3463](https://www.rfc-editor.org/rfc/rfc3463)。上游只给了 550 时，本机并不知道一定是“用户不存在”，不能编造 5.1.1；通用 5.0.0 正是为了不声称更多知识。报告使用 ASCII、CRLF、随机 MIME boundary 和 Message-ID，不包含原文、Bcc、认证信息或其他收件人。

自动生成资格仅限已经接受的认证 submission。原信为空 reverse-path、来源是 dsn、离线 import 或其他内部来源时，保存 `suppressed` 及原因。头部伪装成普通邮件不能改变数据库来源。外域 reverse-path 必须是在原提交中获准的 send-as；报告再进入同一固定 TLS 上游的队列。本地域目标不存在或配额满则保持 pending，不改投其他地址。

管理 CLI 当前只允许授予本地域 send-as。外域报告路径主要覆盖既有授权/已接受责任在本地域配置变化后的情况，以及可信存储调用者；测试通过一次性域配置变更建立该场景，没有开放任意外域身份授权。

通知本身被拒绝时保存其失败状态，并以 dsn 来源／空信封发件人阻断下一份通知。此规则遵循 [RFC 5321 §4.5.5](https://www.rfc-editor.org/rfc/rfc5321#section-4.5.5) 的循环保护原则。管理员仍能查询它；抑制退信不等于删除失败。

没有成功/延迟通知，也没有实现 RFC 3461 的 NOTIFY、RET、ENVID、ORCPT。服务器**不宣告 DSN 扩展**；能生成标准格式失败报告与支持协商扩展是不同的功能。

## 5. 有界扫描、故障恢复与容量

到期索引按 `(expires_at_ms,id)` 排列，仅包含 relay 的 pending/deferred；通知索引按 `(notification_due_ms,id)` 排列，仅包含 failed/pending-notification。因此一个很晚的 next_attempt 不会挡住到期处理，海量 hold 也不必在每轮扫描中遍历。

| 工作 | 当前上限／策略 |
| --- | --- |
| 到期扫描 | 每次最多 128；后台每秒扫描，即使网络槽已满也执行；领取前另做一次有界扫描 |
| 通知生成 | 后台最多一个真实任务；每秒最多启动一个；离线 maintain 每批 1–128 项 |
| 报告字节 | 至多 16 KiB，且不能超过配置的 message_bytes；有限报告可整体构造，原邮件仍流式处理 |
| 暂存预算 | 与普通写入共用预约计数和磁盘保留空间；不绕过全局容量限制 |
| 临时失败 | 固定 60 秒再试，保留有限诊断；不存在忙等式重建 |
| 管理读取 | queue list/history 最多 128，基于游标；单份报告关联查询有界 |

本地通知先查配额，再创建暂存文件，事务内再次检查，防止检查与提交之间的变化。持续满配额不会每秒留下一个新文件。但预检之后的 I/O/提交失败仍可能留下孤立文件，长期故障会占用磁盘；需要检查 pending 原因、修复故障并运行已有 GC。此阶段没有历史清理策略，也不承诺通知积压在无限输入下有固定总大小。

壁钟跳变沿用已有单调时钟对照保护，暂停调度而不凭异常时间批量判定邮件到期。恢复流程见队列基础章。SQL 操作次数和生成字节有限，不代表任意磁盘故障下每次系统调用都有硬实时上限。

## 6. PIPELINING：少等待两个往返，不少确认条件

按照 [RFC 2920](https://www.rfc-editor.org/rfc/rfc2920)，对端在当前会话的 EHLO 中宣告 PIPELINING 后，发送端可将一组命令一起送出：

```text
客户端：MAIL FROM:... CRLF RCPT TO:... CRLF DATA CRLF
服务器：250 ... CRLF 250 ... CRLF 354 ... CRLF
客户端：此时才持久标记 body 阶段并发送正文
服务器：读取最终点号之后给出最终结果
```

回复按命令**位置**关联，不能看到一个 250 就推断整个信封成功。MAIL/RCPT 任一失败，即便异常对端随后给了 354，也不发送正文。当前连接只承担一项责任，此时直接关闭；这是对 RFC 2920 在异常 354 后建议发送空终止行的有意取舍，避免向行为矛盾的对端提交空邮件。实现不复用该连接。

实现同时写命令、读回复。否则小窗口下可能出现：客户端等写完剩余命令，服务器等写完第一条回复，双方都不继续读。`tokio::io::split` 和 `try_join!` 保持两个方向前进；容量为 1 字节的 duplex 测试强制每次发生部分写入，能直接暴露先写后读的死锁。

每条回复仍限 512 字节，每组多行回复至多 64 行／16 KiB，并有分阶段超时；没有按任意对端声明预分配内存。检测到额外的预读回复、错码的多行回复或缺失回复会失败并关闭连接。TCP 上将来才到达的额外字节无法凭一次缓冲检查预知，下一阶段仍须按协议解析与超时处理。

STARTTLS、AUTH 和正文结束保留同步点；TLS 后重新 EHLO，清空此前能力。对端没有宣告时使用顺序路径。收件人仍逐人独立连接，没有连接池和多 RCPT 共享事务；流水线只减少信封阶段的等待，不消除握手与正文传输成本。CLI `send` 和队列工作器自动共用这一能力。

接收端仍按完整命令顺序执行与回复。它能够处理 TCP 将多个命令合并或者将命令逐字节拆开；DATA 模式中的 `RSET` 是正文，退出 DATA 后才重新解释命令。只有通过这些边界实验才增加 EHLO 宣告。

源码：[SMTP 传输与流水线](../crates/client/src/smtp.rs)、[部分写入与取消测试](../crates/client/src/smtp_tests.rs)、[独立 TLS 对端](../scripts/m43_smoke.py)。

## 7. 可执行实验与操作手册

先运行完全隔离的实验，使用临时账户、临时 CA 和环回端口，不发送外部邮件：

```sh
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --example m42_attempt --features test-support --locked
cargo build -p rustymail-store --example m43_probe --features test-support --locked
python scripts/m43_smoke.py
```

它验证 15 种 TLS 流水线对端行为、接收端合并/分片输入、真实认证提交后的失败报告、配额阻塞恢复、独立到期扫描、未知结果结案，并在六个通知边界实际结束子进程。配额重试与到期的集成测试会改动**自己已停止的临时数据库**以推进条件；普通管理员不要修改业务库中的时间字段。完整 60 秒门槛由可控时钟单测验证。

以下统一 CLI 命令与旧 `rustymailctl` 等价。管理队列和配额目前需**先停止服务进程**以取得独占存储锁；不支持与运行中的 daemon 同时离线打开数据库。

```sh
rustymail admin --config server.toml queue list --limit 50
rustymail admin --config server.toml queue show DELIVERY_ID
rustymail admin --config server.toml queue history DELIVERY_ID --limit 50
rustymail admin --config server.toml queue maintain --limit 16
rustymail admin --config server.toml check-store
```

`show` 返回原责任、通知 message ID、通知的 delivery 状态。`maintain` 只扫描和生成本地/入队责任，不连接 SMTP；输出 `transmitted=false`。出现 pending_errors 时退出非零，即使本批其他任务已成功；重新运行不会重复提交已完成通知。修复故障后仍需等待 notification_due_ms 到达，再运行 maintain 或启动中继工作器。

满配额时可先核实当前用量，再增加配额；新值不能小于已使用字节：

```sh
rustymail admin --config server.toml account quota alice@example.com 1073741824
```

诊断 `recipient_quota`、`recipient_unavailable`、`recipient_counter_exhausted`、`disk_or_temporary_budget`、`report_size_limit`、`storage_unavailable` 分别对应本地容量、目标状态、计数器、空间预算、大小契约和存储故障。不要通过删除原信来消除提示。永久失败的通知可能还在 pending；failed 本身不代表已通知。

人工处理示例：

```sh
rustymail admin --config server.toml queue hold DELIVERY_ID
rustymail admin --config server.toml queue retry DELIVERY_ID --allow-duplicate
rustymail admin --config server.toml queue close-unknown DELIVERY_ID --reason "Checked upstream logs; outcome remains unknown"
```

原因当前限制为 1–512 字节可打印 ASCII，避免控制字符与无界记录；不要写密码或正文。以上是不同处理选择，不应机械连续执行。已到期且仍可人工恢复的 hold/uncertain 还需显式 `--extend-expired`。failed 终态不能借此复活。history 保存原/目标状态、操作、风险标志和原因；它是本地运维记录，尚不是带远程用户签名的审计系统。

## 8. schema 4 升级与证据边界

新版本通过现有原子迁移框架执行[0004.sql](../crates/store/migrations/0004.sql)，保留已发布的 0001–0003。增加生命周期字段、到期/通知索引和管理事件表；已有通知关联被保留并标记 created。旧二进制不能打开更新后的 schema，不能通过手改 user_version 降级；升级前停止进程并备份完整存储，恢复时按已有恢复流程处理。

旧 schema 3 没有记录每次重试是否源自 unknown。迁移对 uncertain、来源不明的 hold，以及多次尝试／显式重试的非成功任务保守保留可能送达；旧多次尝试的 failed 转为 uncertain。部分其实安全的旧任务因此也需要人工核实，这是缺失历史无法恢复时的取舍。已经发出的旧报告无法撤回。

测试专用的 legacy fixture 会重建旧表形状，仅在 `test-support` 中提供，供一次性实验还原迁移前环境；它不是生产降级接口。迁移实验包含已有队列和已有通知，以及提交前后注入错误，不通过篡改已发布迁移来“修复”测试。

进程强杀验证提交边界；Linux VM 存储实验验证既有同步与满盘路径；两者都不是任意物理硬件断电的形式化证明。当前 Windows 不提供同等目录同步保证。新功能的性能论证限于有界算法和实际测量，不能把少两个命令往返直接写成某个吞吐提升百分比。

## 9. 练习与下一步

1. 在提交前强杀通知工作器，解释为何可以有孤立 blob，却不能有半个 mailbox UID。
2. 分别构造 RCPT 550、DATA 最终 550 和丢失最终回复，说明哪些情况有资格生成失败报告。
3. 将测试中的 duplex 容量设为 1，再改成先写全部命令后读回复，观察哪个依赖形成等待环。
4. 给出一条 next_attempt 在明天、expires_at 在今天的任务，检查 SQL 查询计划是否只走 expiry 索引。
5. 解释 `created` 与报告 `delivered` 为什么需要两个字段，以及外域通知结果未知时为什么不能再造一份通知。

下一里程碑是 **M5.1：IMAP 协议骨架与受保护的只读邮箱访问**。先补 M0 的增量 literal/取消/预算实验和规范矩阵，再接入验证 TLS、既有身份、SELECT/EXAMINE、UID 稳定性与有限 FETCH，让独立客户端和 Emacs 能读取已有邮件。MIME 深层解析与不可信输入隔离须先完成 spike；APPEND、标志修改、多会话通知、搜索、Sent 归档和完整 Gnus 流程按后续 M5 子步骤验收，不能用只读连接成功代替完整 IMAP。
