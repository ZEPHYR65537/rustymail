# 设计决策、知识记录与资料

## 1. 决策记录

状态均为“设计采用，待实现验证”。重要变更需记录原因，不让代码、教程和运维文档各说各话。

| ADR | 决策 | 替代方案与代价 | 重新评估触发条件 |
| --- | --- | --- | --- |
| 001 | 自研协议/队列/邮箱业务，复用密码学与解析库 | 完整套用现成邮件服务器更快，但学习和实现目标不同 | 核心协议范围变更 |
| 002 | 单机 SQLite + 不可变 blob | PostgreSQL/对象存储利于多节点，但增大部署和一致性复杂度 | 需要多写实例或跨机 HA |
| 003 | blob 耐久后再提交元数据，250 最后发 | 先应答更快但可能丢已确认邮件 | 不允许降低耐久性；只评估有界批提交 |
| 004 | 先中继、最终直接 MX | 直接 MX 从第一天做会推迟存储和客户端闭环 | M7 完成后进入 M8 |
| 005 | IMAP4rev1 核心先完整，扩展逐项开放 | 简化私有 API 更容易，但无法达到标准 Emacs 互通 | rev2 有完整条款与互通证据 |
| 006 | Argon2id + 每设备应用密码 | OAuth 依赖身份提供方与客户端 token 流程 | 团队已有 OIDC 或合规身份需求 |
| 007 | MIME 受限工作进程 | 同进程更省 IPC，但无法靠任务超时隔离 OOM | 有经验证的分配上界解析器 |
| 008 | 首发离线 GC | 在线 GC 更高可用，但需读者/pin/世代协议 | GC 维护窗口无法满足使用需求 |
| 009 | Gnus + smtpmail 作为客户端基线 | mu4e/notmuch 本地检索更强，但增加同步和索引组件 | 需要稳定离线工作流或大量本地搜索 |
| 010 | 严格验证或延期的直接 TLS 默认策略 | 更宽松策略能提高覆盖率但降低链路身份保证 | 实际互通数据证明需要有审计的按域例外 |

## 2. 规范资料：读哪些、用在哪里

核验基线日期 2026-09-21。下列为一手资料；实现仍应检查 RFC 页面列出的更新与勘误。本项目给出的配额、模块、进程、缓存和验收策略是工程选择，不是所有邮件服务器必须照抄的规范要求。

| 资料 | 本项目用途 |
| --- | --- |
| [RFC 5321 SMTP](https://www.rfc-editor.org/rfc/rfc5321.html) | 信封、接收责任、重试和 SMTP 对端行为 |
| [RFC 5322 消息格式](https://www.rfc-editor.org/rfc/rfc5322.html) | 头部与作者字段；与信封严格区分 |
| [RFC 3207 STARTTLS](https://www.rfc-editor.org/rfc/rfc3207) | 升级后的状态重置和缓冲安全 |
| [RFC 4954 SMTP AUTH](https://www.rfc-editor.org/rfc/rfc4954) | 认证能力、会话语义与错误处理 |
| [RFC 2920 PIPELINING](https://www.rfc-editor.org/rfc/rfc2920) | 多命令批发送、响应顺序和边界 |
| [RFC 3464 DSN 格式](https://www.rfc-editor.org/rfc/rfc3464) | 失败通知 MIME 结构 |
| [RFC 7505 Null MX](https://www.rfc-editor.org/rfc/rfc7505/) | 显式不收信域与无 MX 域的区别 |
| [RFC 8314 用户侧 TLS](https://www.rfc-editor.org/rfc/rfc8314.html) | 465/993 与用户提交/读取安全 |
| [RFC 3501 IMAP4rev1](https://www.rfc-editor.org/rfc/rfc3501.html) | 首发核心兼容性矩阵 |
| [RFC 9051 IMAP4rev2](https://www.rfc-editor.org/rfc/rfc9051.html) | 后续演进与 rev1 差异，首发不宣告 rev2 |
| [RFC 4315 UIDPLUS](https://www.rfc-editor.org/rfc/rfc4315) | APPENDUID/COPYUID/UID EXPUNGE |
| [RFC 2177 IDLE](https://www.rfc-editor.org/rfc/rfc2177) | 会话等待与异步变更通知 |
| [RFC 6851 MOVE](https://www.rfc-editor.org/rfc/rfc6851) | 原子移动的协议语义 |
| [RFC 7208 SPF](https://www.rfc-editor.org/rfc/rfc7208.html) | 发信主机与域政策、DNS 限额 |
| [RFC 6376 DKIM](https://www.rfc-editor.org/rfc/rfc6376.html) | 签名字段与规范化；另核对算法更新 |
| [RFC 8301 DKIM 加密更新](https://www.rfc-editor.org/rfc/rfc8301.html) | 禁止旧弱签名策略 |
| [RFC 9989 DMARC](https://www.rfc-editor.org/rfc/rfc9989.html) | 2026 年当前核心规范；替代 7489/9091 |
| [RFC 9990 聚合报告](https://www.rfc-editor.org/rfc/rfc9990.html) | 报告实现与兼容性规划 |
| [RFC 9991 失败报告](https://www.rfc-editor.org/rfc/rfc9991.html) | 可选报告与隐私范围；首发不发送逐封失败报告 |
| [RFC 8461 MTA-STS](https://www.rfc-editor.org/rfc/rfc8461.html) | 直接投递的政策执行与缓存 |
| [RFC 7672 DANE](https://www.rfc-editor.org/rfc/rfc7672/) | 后续 DNSSEC/TLSA 扩展，首发不声称支持 |
| [RFC 9106 Argon2](https://www.rfc-editor.org/rfc/rfc9106.html) | 密码散列参数和内存成本 |

## 3. 实现资料

| 官方入口 | 阅读重点 |
| --- | --- |
| [Tokio 共享状态](https://tokio.rs/tokio/tutorial/shared-state) | 短临界区、任务间通信、锁与 async |
| [spawn_blocking](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html) | 阻塞线程、取消与资源限制 |
| [rustls](https://docs.rs/rustls/latest/rustls/) | crypto provider、验证配置和 TLS 边界 |
| [smtp-proto](https://docs.rs/smtp-proto/) | SMTP parser 能力，不等于业务状态机 |
| [imap-codec 仓库](https://github.com/duesee/imap-codec) | literal、增量解码、支持版本与许可证 |
| [mail-parser](https://docs.rs/mail-parser/latest/mail_parser/) | 输入借用、MIME 结构、可能的全量内存需求 |
| [mail-auth](https://docs.rs/mail-auth/latest/mail_auth/) | SPF/DKIM/DMARC 及新规范实际实现情况 |
| [RustCrypto argon2](https://docs.rs/argon2/latest/argon2/) | PHC 编解码、验证与参数 |
| [rusqlite](https://github.com/rusqlite/rusqlite) | bundled SQLite、线程、features 和许可证 |
| [Hickory Resolver](https://docs.rs/hickory-resolver/latest/hickory_resolver/) | 解析器策略、缓存与查询行为 |
| [SQLite WAL](https://www.sqlite.org/wal.html) | reader/writer 并发、checkpoint、网络盘限制 |
| [SQLite synchronous](https://www.sqlite.org/pragma.html#pragma_synchronous) | FULL 与 NORMAL 的持久性差异 |
| [SQLite Backup API](https://www.sqlite.org/backup.html) | 一致数据库副本 |
| [Rspamd 协议](https://docs.rspamd.com/developers/protocol/) | checkv2 请求/响应与正文传输 |
| [Gnus IMAP 配置](https://www.gnu.org/software/emacs/manual/html_node/gnus/Customizing-the-IMAP-Connection.html) | stream/user/port 的真实语义 |
| [Emacs SMTP 手册](https://www.gnu.org/software/emacs/manual/html_mono/smtpmail.html) | 提交加密、认证与客户端队列 |
| [auth-source 用户说明](https://www.gnu.org/software/emacs/manual/html_node/auth/Help-for-users.html) | 加密凭据、host/login/port 匹配 |

这里的 `latest` 仅是阅读入口，不能进入可复现构建文件。实现锁定具体版本后，在源码目录另存依赖清单、许可证与功能选择；尤其注意 optional feature 带入的密码学/压缩/系统库依赖。

设计期间还对照了本机 Emacs 31.1 的 `nnimap.el`、`smtpmail.el`、`gnus.el`、`gnus-msg.el` 和 `mm-decode.el`。这可以验证配置 API 的存在与含义，不能替代对未来 Rust 服务端的互通测试。

## 4. 知识记录模板

每次遇到跨层 bug 或重要取舍，用独立 Markdown 记录以下字段：

```text
标题：一句话说明具体问题
状态：假设 / 已复现 / 已修复 / 已验证
观察：输入、版本、环境和实际输出
最小复现：可运行命令与脱敏 fixture
预期不变量：系统本来承诺什么
根因：协议层、任务层、存储层分别发生什么
失败方案：尝试了什么，为什么不能解决
最终方案：对应源码、资源成本和边界
验证：回归用例、故障测试、性能对照
资料：支持结论的规范或官方文档
遗留：仍未验证或需要后续演进的部分
```

将“经验教训”写成可复现知识：例如说明是哪一次 `await` 取消了哪个 future，而不是只写“异步编程要小心”。可以承认初始设计错误，并说明证据怎样改变决策。
