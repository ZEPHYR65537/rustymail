# M3.2：从传输字节到最终交付

本章对应 0.5.0。前置阅读：[存储恢复](11-m1-storage.md)、[取消与资源边界](13-cancellation-and-bounds.md)和[入口与 STARTTLS](14-m3-starttls.md)。目标是解释一封邮件在什么时刻获得 Received 和 Return-Path，以及这些改变如何影响大小、摘要、配额和重放。

当前三个 SMTP 入口及旧实验命令均使用本章流程，只交付本地邮箱。无外发、MIME 语义解析、域认证或反垃圾，仍只允许环回实验。M3.3 的完整输入契约与压力验收尚未完成。

## 1. 三种字节表示不能混用

客户端在 DATA 阶段发送的是线上表示：行以 CRLF 结束，行首的点需要转义，最后一行单独的点终止 DATA。解码得到客户端内容；它的大小包含 CRLF，不包含额外转义点和结束标记。SIZE 是估计值，不是分帧依据。[RFC 1870 §5–6](https://www.rfc-editor.org/rfc/rfc1870.html#section-5)

最终本地交付表示则是：新 Return-Path → 新 Received → 保留下来的客户端头部 → 空行 → 正文。删除客户端已有的 Return-Path 及其折叠续行，其余已有头部顺序和字节保持不变，正文经过点解码后逐字节保存。空 DATA 或只有头部而没有分隔空行的 DATA 会补一个 CRLF，使最终头部有明确终点。

因此，“原文导出”指已经接受的不可变存储表示，不再承诺与 SMTP 客户端提交的整个字节串相等。0.4.0 及更早的历史邮件不会被后台改写；同一邮箱可以同时包含旧表示和新表示。没有数据库迁移，既有 blob 的摘要、UID、配额和导出保持原样。

## 2. 一跳 Received 与最终 Return-Path 的责任

接收服务器在内容前增加本跳 Received，保留已有 Received；最终交付到邮箱时，Return-Path 记录 MAIL FROM 的反向路径。协议允许最终交付方清理旧 Return-Path。[RFC 5321 §4.4](https://www.rfc-editor.org/rfc/rfc5321.html#section-4.4)

本项目选择清除所有旧 Return-Path，再增加恰好一个新字段。`MAIL FROM:<>` 生成 `Return-Path: <>`，不会从正文 From 猜一个退信地址。正文里的同名字串属于正文，不删除。未来 M4 中继路径必须区分“接受转发”和“最终交付”，不能直接发送本章的本地交付表示。

示例输出：

```text
Return-Path: <bounce@remote.test>
Received: from client.example.test ([127.0.0.1])
        by mail.example.com with ESMTPSA
        id 0123456789abcdef0123456789abcdef; Mon, 21 Sep 2026 12:00:00 +0000
Received: from previous.example.test
        by another.example.test; ...
From: author@remote.test
Subject: example

Body stays here.
```

实际文件用 CRLF，续行以 TAB 开始。示例的 ID 和时间是说明值。实现中的 ID 是同一 DATA 事务的 operation ID，可用于离线查询；时间在进入 DATA 时生成，使用 UTC 数字偏移，不等于数据库最终提交时间。

`from` 的 IP 来自 TCP 对端，客户端无法用 EHLO 改写。EHLO/HELO 名称只有满足域名语法才写入；否则仅记录地址字面量。语法有效不代表经过 DNS 或身份认证，已有 Received 也可能由对端伪造。代码不查询反向 DNS，不把这些头部作为授权凭据。

扩展会话用 ESMTP；加密后用 ESMTPS，成功认证后用 ESMTPSA。隐式 TLS 提交也记录 ESMTPSA。HELO 会话保守记录 SMTP，不因某个端口就虚构 ESMTP 能力。协议标记反映本跳使用情况，不提供端到端真实性证明。[RFC 3848 §1、§3](https://www.rfc-editor.org/rfc/rfc3848.html#section-1)

本项目省略可选的 `for` 子句，也不写登录名、应用密码或收件人清单。同一文件供多个本地收件人引用，因此不能在共享字段里泄露 Bcc 或其他信封收件人。已有客户端头部中的 To/Cc 不由本步重写。

## 3. 只做流式结构处理，不重排整封邮件

沿 [trace.rs](../crates/server/src/trace.rs) 和 [receive_message](../crates/server/src/lib.rs) 阅读：

1. DATA 准入时获取磁盘预算，生成一次操作 ID、时间和有界前缀。
2. 按既有分帧规则解码一行，先增加客户端内容计数和客户端头部计数。
3. 头部结构过滤器决定保留或跳过这一行；一旦遇到空行，之后不再解释字段名。
4. 向同一个 staging 文件写入最终字节，同时更新最终大小和 SHA-256。
5. 同步文件与目录，提交所有本地投递、配额、UID 和操作记录，成功后才发送最终 250。

过滤器只保存“是否已有字段”和“当前字段是否正在删除”两个布尔状态。每行最多 1000 个解码字节，DATA 缓冲复用，不累计整份头部，不为每个收件人复制正文。新增前缀不超过 2048 字节的独立预算，不生成第二个整封临时文件。

为了可靠识别字段边界，当前策略拒绝无字段名的首个续行、字段名带空格、无冒号和头部控制字符等输入；结构错误返回 550 并关闭，避免把未读 DATA 当成命令。NUL、裸 CR/LF 和超长物理行由既有分帧层拒绝，当前仍使用 451 并关闭，响应分类将在 M3.3 统一。认证提交仍额外执行 M2 的受限 From 检查。

这只是结构检查，不宣称验证完整 RFC 5322/MIME 语义。未知字段按字节保留；实验里保留 DKIM-Signature 字节并不代表验证签名。M6 接入 DKIM 时必须先在未变换内容上验证，因为删除被签名的字段可能影响结果；不能对本地交付表示直接假定原签名仍有效。

## 4. 三个大小与两种失败

| 量 | 计数对象 | 用途 |
| --- | --- | --- |
| 客户端内容大小 | 点解码后所有输入，包括随后删除的旧 Return-Path | `limits.message_bytes` 与 EHLO SIZE；默认 25 MiB |
| 客户端头部大小 | 输入头部和其分隔空行，包括随后删除的字段 | `limits.header_bytes`，防止重复或折叠字段绕过预算 |
| 最终存储大小 | 新前缀、保留字段、分隔空行、正文 | blob 摘要、文件大小、邮箱配额、导出 |

声明 SIZE 超过上限在 MAIL 阶段返回 552；低估 SIZE 不决定 DATA 结束位置，实际数据仍受真实上限约束。新增字段不侵占客户端上限，但会消耗磁盘和邮箱配额。

每个接收预约“客户端上限 + 2048”，其中包含可能补上的分隔空行。配置必须至少容纳一次完整预约；准入数量为临时预算除以单次预约的整数部分，再受 ingest_concurrency 限制。默认临时预算仍为 400 MiB，因此在默认 25 MiB 上限下能容纳 15 次完整预约，而不是原来的 16 次。没有为保留并发数而偷偷提高总预算。

存储层的 `StoreOptions.max_message_bytes` 表示最终存储上限；[适配层](../crates/server/src/worker.rs)负责从客户端上限加上额外预算。协议与存储两种含义不能共用一个未经说明的计数器。

超过客户端内容或头部限制返回 552 并关闭；某个收件人的最终配额不足，则整笔本地交付返回 452，其他收件人也不可见。RCPT 的 250 只接受继续尝试该收件人，不等于已经交付。提交结果不明仍直接断开，不能谎称肯定未接收。

## 5. 共享与幂等依赖最终表示

同一 DATA 的所有本地收件人共享一个 message/blob，各邮箱分别获得 UID 和完整存储大小的配额记账。重复同一个 RCPT 地址只产生一次交付。此处不引入别名展开或外部队列。

内部重放必须同时复用 operation ID、最终字节、信封及收件人集合。若重试时重新生成 Received 时间或 ID，摘要就变了；同一 operation ID 应报告冲突，不能再接受一份。网络层当前不会自动重试接受事务，结果不明时用 operation 查询调查。

外部 SMTP 重发则是新事务，获得新 operation ID 和新 Received；即使客户端内容完全相同，也允许成为两封邮件。这不等于内部幂等失效，也不能用相同 Message-ID 或正文摘要粗暴去重。

## 6. 可复现实验与阅读检查点

在仓库根目录执行，全部使用一次性目录与合成内容：

```sh
cargo test -p rustymail-server --all-features --locked
cargo build --workspace --locked
python scripts/m32_smoke.py
```

独立 Python 客户端应输出六组通过记录和 `accepted_messages: 7`。它验证多个收件人共享文件、伪造/折叠 Return-Path 替换、旧 Received 与正文保留、大小边界、空反向路径、原子配额、临时预算，并在关闭服务后检查导出、数据库计数和文件摘要。JSON 同时记录版本与工作区状态；dirty 运行不能冒充干净提交证据。

继续运行 `scripts/smoke.py`、`scripts/m2_smoke.py`、`scripts/m3_smoke.py` 验证旧流程；TLS 实验需先按上一章构建临时证书生成器。旧冒烟脚本已改为检查本地前缀和保留内容，恢复工具仍要求整个最终存储文件的精确副本。Linux VM 回归中仅经 SMTP 交付的候选邮件新增前缀，直接调用存储 API 的历史种子保持原字节。

自测：

1. 删除了 2 KiB 的旧 Return-Path，可以多接收 2 KiB 正文吗？不能，客户端输入先计数再过滤。
2. 两个收件人是否只记一次配额？磁盘文件只存一次，各邮箱分别按最终大小记账。
3. 能否从 From 补全空 Return-Path？不能，反向路径来自信封，空值有独立语义。
4. 重放时重新生成时间戳有什么后果？最终摘要改变，同一操作键会冲突。
5. 为什么保留 DKIM-Signature 字节还不够？签名依赖被签名的字段和规范化结果，验证应在变换前完成。

下一步 M3.3：统一输入契约、能力矩阵、语法/状态负例和资源压力验收。所有实验通过也不会解除公网生产门槛。
