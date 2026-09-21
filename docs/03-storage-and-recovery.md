# 持久化、队列与崩溃恢复

## 1. 选择和物理布局

元数据使用本地 SQLite，原文使用不可变文件。没有网络共享磁盘，没有多个写实例，没有为了吞吐关闭同步落盘。WAL 提供读写并行，但写事务仍要串行；长读事务会阻碍 checkpoint。[SQLite WAL](https://www.sqlite.org/wal.html)

```text
/var/lib/rustymail/
  instance.lock
  meta.sqlite
  meta.sqlite-wal / meta.sqlite-shm  # 生命周期由 SQLite 管理
  blobs/ab/cd/<random-blob-id>.eml  # 同一文件系统
  staging/<random-ingest-id>.part
  quarantine/                     # 仅显式隔离对象，仍受配额管理
  backup-manifests/
```

使用随机 128 位 ID，原子 `create_new`，碰撞重试；不会用邮箱地址、邮件主题、Message-ID 或客户端文件名构造路径。目录不允许用户写入，拒绝符号链接逃逸。摘要用于完整性核验，首发不用于跨用户全局去重。

一封消息发给多个本地收件人时可以共享同一不可变文件，但每个邮箱都有独立 UID、flags 和配额记账。邮件原文是最终存储字节，所有 MIME offset 对应这一版本；更新 DKIM 或增加头部会产生新版本，不能沿用旧 offset。

0.5.0 在同一 staging 流里构造本地交付表示：增加本机 Received/Return-Path、清除旧 Return-Path，之后计算最终摘要和配额。客户端输入与最终大小分开计数；不保存额外整封输入副本，不改写历史 blob，详细边界见 [M3.2 教程](15-m3-local-delivery.md)。未来 MIME offset 必须从该最终版本生成，当前尚未解析 MIME。

## 2. 数据模型

[schema.sql](examples/schema.sql) 是可供 SQLite 执行的结构草案；L0 已将其转换为[首个迁移](../crates/store/migrations/0001.sql)。0.2.0 使用 `PRAGMA user_version=2`，加入迁移摘要、维护执行和 GC 动作记录，实现账号、本地接受、配额、原文检查、离线 orphan GC 和精确副本恢复，操作见 [M1 教程](11-m1-storage.md)。队列执行、IMAP 和已接受消息的历史过期尚未实现；下文继续定义完整目标。核心关系如下：

| 对象 | 唯一性 / 约束 | 用途 |
| --- | --- | --- |
| account | 登录名唯一，配额非负 | 身份与账户状态 |
| credential | selector 唯一，引用 account | 应用密码散列、撤销、scope |
| address | 规范化本地域地址唯一 | 收件路由与 send-as 授权分开存储 |
| mailbox | `(account_id, name)` 唯一 | UIDVALIDITY、UIDNEXT、事件版本 |
| blob | ID 唯一、摘要和字节数固定 | 不可变正文与 MIME 元数据版本 |
| message | 入站 operation_id 唯一 | 信封发件人、来源和接受时刻 |
| mailbox_message | `(mailbox_id, uid)` 唯一 | 消息与邮箱关系、flags、internaldate |
| delivery | `(message_id, recipient)` 唯一 | 每个实际收件人的本地/远程投递责任 |
| mailbox_event | 邮箱内事件序号唯一 | 多会话有序通知与恢复 |
| notification | 每条 delivery 的报告类型唯一 | 失败通知的内部幂等键 |

空信封发件人用明确的空值语义表示，不与“未知”混淆；外部正文 Message-ID 只是可索引字段，不是内部主键。邮箱的所有权必须经 account_id 校验，不允许仅凭 uid 读取内容。

## 3. 接收提交协议

1. 接收前预约连接、原文大小、临时磁盘和本地配额预算。SIZE 不存在时按单封上限预约，避免许多慢客户端一起用完磁盘。
2. DATA 点解码后的原文流式写 staging；累计真实大小和摘要。写入超限或磁盘不足时停止接收；未成功的消息无投递责任。
3. 扫描、头部处理、受限 MIME 解析完成，形成最终不可变字节及本地/远程收件人计划。所有永久拒绝与可预见的配额失败在这里结束。
4. 同步文件内容和所需元数据；将文件在同一文件系统内原子重命名到最终路径；同步最终父目录和 staging 父目录。新建分片目录也必须同步父链。不得用跨文件系统 copy 冒充 rename。
5. SQLite 短写事务中插入 blob/message/delivery，分配本地 UID、更新配额与 mailbox_event；所有接受的收件人共同提交。各连接显式设置 `foreign_keys=ON`，`journal_mode=WAL`，`synchronous=FULL`。
6. 数据库提交成功并且调用端取得确定结果后，发 DATA 最终 250。若响应丢失，邮件仍按已接受处理。

这是文件系统与数据库之间的**有序提交**，不是跨资源原子事务。顺序让失败倾向于留下无引用文件，避免先出现指向尚未耐久正文的数据库记录。FULL 的作用及底层同步假设见 [SQLite PRAGMA](https://www.sqlite.org/pragma.html#pragma_synchronous)；真实磁盘、文件系统、虚拟化栈仍必须接受故障实验。

程序不得先回复 250 再异步写盘。采用 group commit 时，整组全部文件与事务都耐久后才逐个回复；其批次容量和最长等待时间也必须有上限。

原始暂存、增加头部后的新版本、DKIM 出站变体都计算实际临时磁盘占用。转换前先预约新版本空间，不假定删除旧文件能立即腾出空间；预算不足时在接受前临时失败。SIZE 面向客户端原文上限，服务器新增头部有单独有界开销；最终存储大小用于配额记账。

### 崩溃点推导

| 故障时刻 | 可见状态 | 恢复动作 |
| --- | --- | --- |
| staging 写到一半 | 没有数据库记录 | 服务确认无活跃 writer 后回收临时文件 |
| 文件同步后、rename 前 | 完整 staging，无记录 | 同上；客户端未收到成功 |
| rename 后、事务前 | 最终文件无数据库引用 | 标为 orphan；宽限期后离线清理 |
| 事务进行中 | SQLite 决定事务有或无 | 无记录按 orphan；有记录按已接受 |
| COMMIT 返回结果未知 | 可能已接受 | 关闭会话，查询 operation_id；不回复明确拒收并继续投递 |
| COMMIT 后、250 前 | 已接受，客户端可能重试 | 正常投递；承认外部重复窗口 |
| 250 后崩溃 | 完整已接受消息 | 从数据库恢复责任，不能只靠内存队列 |
| 元数据引用正文但文件损坏 | 数据不变量已破坏 | 立即告警、隔离受影响读写并恢复备份；不悄悄删引用 |

## 4. 数据库访问纪律

一个写线程用有界 channel 接收命令；调用方等待时不能占用数据库连接。读池 2 个连接，给 cache_size 配置总预算，busy_timeout 是兜底而非并发设计。每个写事务只做数据库工作，不能包含文件传输、密码散列、DNS 或反垃圾请求。

对常见查询建立索引：`delivery(state, next_attempt_at, id)`、`mailbox_message(mailbox_id, uid)`、`mailbox_event(mailbox_id, event_seq)`。FETCH 先短事务拿元数据和受保护的 blob 引用，释放事务后再向慢客户端流式传输。SEARCH 分页扫描并检查取消和预算，不持有长时间读快照。

UID 分配、UIDNEXT 更新和邮箱行插入同事务。UID 是 32 位非零空间；耗尽必须触发规范允许的重建/UIDVALIDITY 变更与运维告警，不能环绕复用。删除重建邮箱必须生成新的 UIDVALIDITY；离线恢复旧备份后需要使客户端缓存失效，不能让回退的 UID 对应不同消息。

## 5. 邮箱视图的内存约束

首发选用简单、可验证的会话 UID 向量保存消息序号映射，按 4 字节 UID 计量实际 capacity。单文件夹支持上限先定为 100,000 封；全局 selected-view 预算 32 MiB，用显式 permit 控制。一个 100,000 封文件夹的基础映射约 0.38 MiB，还要计入容器和事件开销。

超出上限时 SELECT 返回明确资源错误，不偷偷截断邮箱。每会话待通知事件最多 64 KiB；不能及时同步时退出会话。这样首发具有明确容量边界，后续可优化为共享版本视图/分页结构；不能在没有实现前宣传百万邮件文件夹低内存。

mailbox_event 是持久有序日志。事件保留窗口与活跃会话游标关联；落后于保留窗口的会话必须重新 SELECT。不要依靠易丢的 broadcast channel 作为唯一变更事实来源。

## 6. 出站队列、租约与幂等

调度线程每次最多取 128 个到期任务，为每条远端收件人建立 `lease_token`、`lease_until`、`generation`。取得任务和状态迁移是数据库事务；投递完成用 token/generation 条件更新，防止旧 worker 覆盖新结果。

活跃工作必须续租；同一进程里不能仅因时间到期就并发重新派发仍在发送的任务。租约和 fencing 只能保护本地状态，不能撤回已发送到远端的 DATA。重启时不存在旧进程的正常工作，过期租约恢复为可重试；单实例锁防止两个服务同时工作。

所有 UTC 时刻由统一 clock 接口读取，间隔超时用 monotonic clock。时间回拨或超前应告警；队列恢复不能把时钟跳变当成海量永久失败。

本地 delivery 转为 delivered 与邮箱插入在同一事务完成，唯一键避免重放插入两次。生成 DSN 时以 `(delivery_id, kind)` 唯一约束与新消息创建一起提交，进程重启不能产生无限通知。

## 7. 删除与垃圾回收

EXPUNGE 先在数据库事务中移除邮箱引用、更新配额、写事件；删除最后引用的文件只能稍后进行。数据库仍由 message/delivery、隔离区或备份 pin 引用时不得清除 blob。

已完成投递与诊断历史初始保留 30 天；仍在 pending/deferred/uncertain/hold 的记录不得按年龄删除。保留期到期后，在事务中依次清理已终结通知关系与投递历史，仅当消息不再被任何邮箱、队列或保留对象引用时才能删除 message 行，最后才使 blob 成为 GC 候选。否则永久保留的 message 外键会使“删除邮件”永远无法回收空间。审计摘要与正文引用分离，保留期调整需留记录。

首发采用**维护窗口离线 GC**：暂停接收、投递与 IMAP，确认无文件读者和提交在途，在独占实例锁下扫描全部引用，分批列出候选并删除，经同步后记录结果。它牺牲短暂可用性，换取可审查的竞争模型。后台只统计候选，不能仅凭“文件老于 24 小时”在线删除。

在线 GC 后续必须引入世代标记、备份 pin 和读者生命周期协议，另写 ADR 和竞争测试后开启。启动回收 staging 同样只能处理没有活跃 writer 的阶段。

## 8. 备份一致性

首发采用短暂停写的协调备份：停止新接受与邮箱写操作、暂停出站领取，等正在提交完成，固定数据库快照和它引用的不可变 blob 集合，给这些对象加备份 pin，再恢复服务。复制的清单包括摘要、密钥版本、schema 版本和 UID 元数据。

只复制 `meta.sqlite` 而漏掉 WAL 不是备份方案。实现可选 SQLite Online Backup API，或服务完全停止后的数据库及文件一致副本；不能边运行边随意复制三份数据库文件来拼快照。跨主机恢复、解密密钥可用性和逐文件校验必须实际演练。[SQLite Backup API](https://www.sqlite.org/backup.html)

备份期间无法确定投递结果的任务恢复后按不确定处理，可能重复。恢复文档要明确这点，不能通过把所有历史任务标记成功来掩盖风险。
