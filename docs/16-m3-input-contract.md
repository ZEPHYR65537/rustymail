# M3.3：把协议承诺变成可执行的输入契约

本章对应 0.6.0。先读[入口与 STARTTLS](14-m3-starttls.md)及[最终本地交付](15-m3-local-delivery.md)，再运行本章实验。范围是 L0 中已支持的 SMTP 子集：说明每类输入如何解释、占用多少资源、失败后能否继续，以及用什么证据验证。它不代表完整 RFC 一致性或生产发布；最后一节保留互通缺口。

## 1. 为什么“能收到一封邮件”还不够

下面两段会话都可能通过只覆盖正常路径的测试，却暴露不同问题：

```text
EHLO client.test
MAIL FROM:<> BODY=8BITMIME
RCPT TO:<alice@example.com>
DATA
...包含高位字节的正文...

HELO client.test
MAIL FROM:<> BODY=8BITMIME
```

第一段要求解析器把 BODY 值传给接收正文的代码。只在解析时认可参数、随后丢弃它，不能证明实现了协商语义。第二段没有启用 ESMTP 扩展，应拒绝参数，不能因为同一程序支持 EHLO 就默认所有会话都协商成功。

因此，将 `Option<Body>` 保留在 MAIL 命令中：`None` 表示未提供参数，`Some(SevenBit)` 表示显式使用扩展。状态机完成协商检查后，把具体的 `Body` 放入 `Envelope`。DATA 只读取当前信封的模式，不依赖易失配的额外布尔变量。[源码](../crates/protocol/src/lib.rs)

这也是 Rust 类型设计的实际用途：保留业务需要区分的信息，再在合法转换点消除“不确定”状态。`Option<Body>` 和 `Body` 的差异对应协议事实，不只是为了通过编译。

## 2. 当前语法、编码和长度契约

下面的长度都按字节计算。命令行和物理行上限包含末尾 CRLF，不是 Unicode 字符数量。命令名和参数关键字忽略 ASCII 大小写。

| 输入 | 0.6.0 的确定行为 |
| --- | --- |
| 普通命令 | 最大 512 字节；ASCII 可打印字符，拒绝 HTAB、其他控制字符及非 ASCII；参数首尾空格容忍，命令名前空格不识别为合法命令 |
| MAIL 行 | 最大 538 字节；SIZE 扩展增加的 26 字节不扩展其他命令的上限 |
| EHLO / HELO | 接受一个非空、无空格的 ASCII token；当前不验证其完整 RFC 主机名语法，也不做 DNS 身份验证；追踪头只使用经独立验证的域名，否则只记录真实 IP |
| 信封地址 | `<ASCII dot-atom@domain>`；local-part 最多 64 字节，完整地址最多 254 字节；仅 MAIL 支持 `<>`；RCPT 必须是存在的本地账号 |
| MAIL 参数 | SIZE 和 BODY 各最多一次；重复为 501；未知参数为 555；HELO 会话带任一扩展参数返回 555 |
| SIZE | 1–20 位十进制数字；符号、空值及 21 位数字为 501。合法大数超过本机上限为 552，包括超过 `u64` 的 20 位数字 |
| BODY | 未指定与 `7BIT` 均要求 DATA 字节为 ASCII；显式 `8BITMIME` 允许正文高位字节；不允许 NUL、裸 CR/LF 或二进制无界长行 |
| DATA 物理行 | 点透明解码后最大 1000 字节；若有透明点，线上可到 1001 字节；只有整行 `.\r\n` 是结束标记 |
| 邮件头部 | 无 SMTPUTF8，因此头部为 ASCII，可使用 MIME encoded-word 表达中文；结构检查不是完整 RFC 5322/MIME 解析 |
| DATA 总量 | 配置 `message_bytes`，默认 25 MiB；按点透明解码后的客户端字节计数，包含随后被移除的旧 Return-Path；另有 `header_bytes` 上限 |
| AUTH | 仅 TLS 提交入口的 PLAIN；命令行及独立响应帧各不超过 1024 字节；解码后的登录名最多 254 字节、应用密码最多 128 字节；不提供任意 SASL 机制 |

SIZE 是声明，不是接下来必须读取的长度。`SIZE=1` 不应让接收器只读一个字节，也不能绕过真实输入计数。处理合法大整数时，解析器先验证十进制语法和位数，再将超出 `u64` 的值饱和为最大值；配置允许的邮件大小远小于该值，状态机必然给出 552。这样同时避免整数溢出和把资源限制误报为语法错误。[RFC 1870 §5–6](https://www.rfc-editor.org/rfc/rfc1870.html)

8BITMIME 允许保留正文中的高位字节，仍保留 SMTP 的行边界和长度限制；它不等于 BINARYMIME，也不等于允许国际化地址或 UTF-8 头部。项目选择严格拒绝未声明的高位 DATA；这是明确的接收策略，不声称所有 SMTP 实现都必须采用相同拒绝策略。[RFC 6152](https://www.rfc-editor.org/rfc/rfc6152.html)、[RFC 6531](https://www.rfc-editor.org/rfc/rfc6531.html)

兼容性变化：0.5.0 丢弃 BODY 值，会接受未声明的高位正文；0.6.0 收紧此行为。Python 客户端发送含中文的原始 8-bit 正文时使用 `mail_options=['BODY=8BITMIME']`，或先编码为 quoted-printable/base64。仓库的首次发送脚本与历史实验已同步更新。已存储的邮件不重写，schema 仍为 2。

## 3. 能力公布必须跟随会话状态

下表描述 `serve-lab-smtp` 三入口。所有行都有 `SIZE`、`8BITMIME` 和 `ENHANCEDSTATUSCODES`；SIZE 的值来自当前配置。

| 入口和状态 | 额外 EHLO 能力 | MAIL / RCPT 的权限 |
| --- | --- | --- |
| receiver 明文 | STARTTLS | 可用空或外部发件人，只投递有效本地账号 |
| receiver TLS | 无 | 与明文 receiver 相同；不接受 AUTH |
| submission 明文 | STARTTLS | MAIL / RCPT / DATA 为 530；AUTH 为 538 |
| submission TLS、未认证 | AUTH PLAIN | MAIL / RCPT / DATA 为 530 |
| implicit submission、未认证 | AUTH PLAIN | 与上行相同，从首字节即 TLS |
| 两种提交入口、已认证 | 无 | MAIL 发件人必须通过 send-as；空路径不允许；M3 仍只交付本地账号 |

旧的 `serve-lab` 没有 TLS 配置，不公布 STARTTLS。新的 EHLO/HELO 清空信封，但不撤销已完成的认证。STARTTLS 后新建会话状态，必须重新 EHLO；AUTH 仅在 EHLO 后且不在 MAIL 事务中可用。明文提交入口允许 EHLO、HELO、RSET、NOOP、HELP、VRFY、QUIT 这些不提交邮件的命令。

HELP 返回简短命令说明，不承诺当前入口或状态允许执行每个命令。VRFY 对存在和不存在的名字都返回 `252 2.5.2`，不访问账号数据库。它不能证明 RCPT 无账号枚举：收信端为了拒绝无效收件人仍会暴露可投递性，限流和公网反滥用属于后续阶段。

未公布 PIPELINING、CHUNKING、BINARYMIME、SMTPUTF8、DSN、AUTH LOGIN。一次 TCP 读取包含多条普通命令时，按顺序处理是 framing 的基本要求，不等于已经承诺 PIPELINING 的完整语义。

## 4. 状态转换和失败后的责任

| 输入或失败 | 回复 / 连接 | 当前事务 |
| --- | --- | --- |
| 无问候的 MAIL、无 MAIL 的 RCPT、无有效 RCPT 的 DATA | 503，保留连接 | 不创建接受责任 |
| 新 MAIL、语法错误的 MAIL、超限 SIZE | 成功 250 或相应错误 | 先清空旧信封，失败也不能复用旧收件人 |
| RSET、EHLO、HELO | 250，保留连接 | 清空旧信封；BODY 随新 MAIL 重建 |
| NOOP、HELP、VRFY | 250 / 214 / 252 | 不改变信封 |
| 重复本地 RCPT | 250；包括已达到人数上限时 | 不增加人数、不增加交付副本 |
| 新 RCPT 超过人数上限 | 452，保留连接 | 保留此前已接受收件人 |
| 无效本地账号、外域 RCPT | 550，保留连接 | 该收件人不进入信封 |
| 完整命令语法无效 / 不支持参数 | 501 / 555，保留连接 | MAIL 的错误额外重置信封；未知命令为 502 |
| 命令 framing 无效或超长 | 500 后关闭 | 不解释剩余字节 |
| DATA 前暂存/接收容量不足 | 452，保留连接 | 已取走信封，重试必须重新 MAIL/RCPT；没有发送 354 |
| DATA framing、行长或编码无效 | 554 后关闭 | 丢弃暂存，绝不把剩余内容切回命令态 |
| DATA 结构性坏头部 / 提交身份头不合法 | 550 后关闭 | 同上 |
| DATA 实际总量/头部超限 | 552 后关闭 | 同上 |
| DATA 行期限或总期限到期 | 421 后关闭 | 释放接收额度和暂存；逐字节发送不能延长行期限 |
| DATA 不完整、暂存 IO 失败 | 尽可能返回 451 后关闭 | 没有成功确认 |
| 提交配额不足 | 452，保留连接 | 整封拒收，不部分投递 |
| 已派发存储提交但结果不明 | 关闭，不伪造成功或失败 | 通过 operation ID 查询；不能删掉可能已提交的数据 |

回复分类影响发送方是否会重试。将永久非法输入都映射为 451，会让对端不断重试同一个坏输入；将磁盘暂时不可用映射为 5xx，则可能导致放弃一封本可稍后接收的邮件。两者都不是“只改一句错误提示”。增强状态码与主回复的类别一致；初始问候及 EHLO/HELO 回复是规范中的例外。[RFC 2034](https://www.rfc-editor.org/rfc/rfc2034.html)

去重采用本地账号的 ASCII 大小写不敏感政策，比较时直接借用字符串，不为每次检查分配两个规范化 `String`。顺序很重要：先检查是否已经存在，再检查新增收件人的容量。这里的线性比较受默认 100 个收件人的上限约束；无需为每个事务引入额外哈希表。

## 5. 资源上限不是只有一个数字

DATA 使用可复用的 1001 字节行缓冲，临时磁盘按 `message_bytes + 2048` 预约。TLS 三入口共享连接、握手和接收额度。密码散列还有独立的执行并发与等待预算；详情见[取消与资源边界](13-cancellation-and-bounds.md)。没有新增整封邮件的内存缓存或运行时第三方依赖。

本次压力配置使用 8 个连接、2 个握手、2 个 DATA 接收任务，单封输入上限 65536 字节，临时预算为 `2 × (65536 + 2048) = 135168` 字节。这些是缩小后的实验条件，不修改默认部署配置。

压力脚本验证四件可观察的事实：

1. 占满连接后，三个入口都拒绝新增连接；关闭旧连接后恢复接收。
2. 两个客户端收到 STARTTLS 的 220 后不握手，第三个升级请求在切换前得到 454；取消后能正常完成 TLS。
3. 连续 12 轮占满两个 DATA 槽并中途断连；第三个请求收到 452，每轮暂存文件最终清空。
4. 三个角色并发完成 120 封接受，其中两个提交客户端实际执行 Argon2 认证并保持 TLS 会话，同时注入 40 个 framing 错误连接。容量不足允许有期限的重试；最后重开存储核对接受数和完整性。

Linux 额外记录主服务的 VmRSS、VmHWM 和文件描述符数；VmHWM 是内核记录的进程生命周期峰值，即使采样错过散列瞬间也保留峰值。256 MiB 是本短实验的宽松回归上限，考虑两个 64 MiB 散列及其他组件；它不是默认配置或 1–100 邮箱生产负载的内存承诺。描述符恢复也不能要求 RSS 立即回到启动值：分配器和运行库可能保留内存。

## 6. 从规范到测试，再到证据

纯解析与真实网络测试互补。对 TCP 逐字节 `send`，操作系统仍可能合并数据；因此不能只靠网络脚本宣称覆盖了所有分片。纯解码测试遍历语料的全部单切分点；现有测试另覆盖逐字节输入。新增状态测试枚举七种操作的全部长度五序列，以独立的三个布尔状态核对能否开始 DATA。这是有限模型检查，不是对任意长度历史的形式化证明，也不是已完成持续 fuzz。

| 依据 / 项目约束 | 纯测试或独立实验 |
| --- | --- |
| RFC 5321：CRLF、透明点、命令顺序和长度 | `malformed_and_limit_frames_are_invariant_under_every_single_split`；C04–C06 |
| RFC 1870：SIZE 位数、MAIL 扩展长度、真实内容计数 | `mail_parameters_preserve_body_and_enforce_size_grammar`；C02/C04；M3.2 大小实验 |
| RFC 6152：BODY 值和字节保留；项目严格 7BIT 策略 | `helo_cannot_enable_extensions_or_retain_old_envelope`；C02/C05/C08 |
| 无 SMTPUTF8，头部与正文编码边界不同 | C05；旧客户端改为明确声明 BODY |
| 事务不能继承失败 MAIL 的收件人 | `generated_state_sequences_never_deliver_without_current_mail_and_recipient`；C02 |
| 去重不消耗新增容量，信息命令不改事务 | `duplicate_at_recipient_limit_and_information_commands_keep_envelope`；C02 |
| 角色、认证、send-as、路由各自生效 | C01/C03 的 48 行矩阵；M2 撤销实验与 M3.1 TLS 边界回归 |
| 绝对期限、容量拒绝、取消后恢复 | C07；P01–P04 |
| 最终 250 对应耐久责任 | C08/P04 重开检查；既有 M1 进程/VM/实际 ENOSPC 回归 |

SMTP 基础条款参见 [RFC 5321 §4](https://www.rfc-editor.org/rfc/rfc5321.html)。表中的 C 编号位于[契约客户端](../scripts/m33_smoke.py)，P 编号位于[压力脚本](../scripts/smtp_pressure.py)。共享的[临时实验夹具](../scripts/smtp_lab.py)只使用 CLI、TCP、证书文件和离线检查，不调用 Rust 解析器作为“正确答案”。

在仓库根目录运行；脚本只使用临时目录、合成邮件、随机应用密码和验证证书的环回 TLS：

```sh
cargo test --workspace --all-features --locked
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/m33_smoke.py
cargo build --workspace --release --locked
cargo build -p rustymail-server --example m2_certificates --release --locked
python scripts/smtp_pressure.py
python scripts/check_docs.py
```

原始输出写入 `reports/local`，包含代码版本、工作区是否有修改、平台、逐项结果和资源数据。Windows 可以跑压力行为，但不声明获得 Linux RSS/FD 证据；Linux CI 使用 release。归档数据及实际结论见[本阶段验证记录](../reports/m3.3/validation.md)。每封延迟包含容量重试，复用已认证的连接；P04 的 `wall_seconds` 另包含连接与两次认证，不包含启动及 P01–P03。VmHWM 覆盖整个进程生命周期。不能将每封延迟当成每封都重新认证的成本。

## 7. 仍须保留的兼容性和生产门槛

当前地址契约仍拒绝 quoted local-part、地址字面量、源路由和不带域的 `RCPT TO:<Postmaster>`。`postmaster@本地域` 需要管理员显式创建账号。裸 Postmaster 接收和合法 SMTP 地址形式是生产互通缺口，不能把它们都包装成“可选扩展”；发布前必须实现并增加路由/授权回归。EHLO/HELO 的完整语法及 AUTH 的通用协议长度兼容同样需要审查。M3.3 关闭的是现有 L0 子集的输入契约和有限压力验收。

还没有长时间 fuzz、72 小时 soak、真实 VPS 磁盘延迟下的 TLS/认证混合负载、恶意 MIME worker、域认证或公网反滥用验收。Rust 的所有权和禁止自写 unsafe 提高内存安全，但并不能自动证明第三方库、内存占用、协议逻辑或取消路径完全正确。短样本只能支撑其对应场景的结论。

下一步 M4 是持久化外发队列与固定上游中继：逐收件人结果、租约、退避、未知结果、退信循环保护。现有“任何外域 RCPT 都拒绝”的矩阵届时必须改成按认证、路由和队列持久化能力授权；不能先放行外域再补接受责任。

验收题：为什么重复 RCPT 要先于容量检查？为何 `Some(SevenBit)` 不能直接等同于 `None`？为何 20 位合法 SIZE 超出机器整数范围仍是资源错误？为何正文 8BITMIME 不能允许 UTF-8 头部？为何异常 DATA 后的 NOOP 必须被丢弃？修改其中一个答案对应的实现，先预测哪项测试失败，再运行确认。
