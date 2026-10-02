# 统一 CLI 与 SMTP 提交客户端

本轮在 M4.2 基础上实现客户端支线；M4.3 的失败通知、到期处理和 PIPELINING 仍按原计划推进。

## 设计契约

- 一个 `rustymail` 文件提供 `serve`、`send`、`admin`、`check` 四个入口。旧 `rustymaild` / `rustymailctl` 保留相同命令行，作为共享入口代码的薄包装。客户端使用独立配置，只初始化本次提交需要的运行时和 TLS；不打开服务器数据库或启动监听器。
- `send` 接受已有 RFC 邮件文件，或从标准输入读取；本轮不实现主题、附件或 MIME 编辑器。要求 CRLF、ASCII 头部、有头／正文分隔、每行不超过 1000 字节（含 CRLF），正文有高位字节时协商 8BITMIME。Bcc、Resent-Bcc、Return-Path 及其折行在发送前移除，收件人只取 `--to`。
- 在连接前校验整条输入并复制到私有临时快照，默认输入上限 25 MiB、头部 64 KiB。占用固定缓冲，临时磁盘占用受大小上限约束；不是持久队列。文件和 stdin 使用同一路径，各收件人从同一份快照的独立文件句柄读取，避免修改源文件或取消读任务导致内容串位。
- 复用单收件人 SMTP/TLS 引擎；一次命令按参数顺序逐人提交，最多 100 个 `--to` 参数，随后去重；域名大小写归一而远端 local-part 大小写保留。没有自动重试、连接池或收件人头部推导。
- TLS 必须验证证书及主机名；支持隐式 TLS 和 STARTTLS，无明文回退。显式指定 PEM CA 文件，密码从私有文件读取，不接收命令行密码，不输出认证报文、正文或任意上游诊断。路径相对客户端配置文件解析。
- 标准输出是逐收件人 JSON Lines，每项立即刷新。`accepted` 只表示上游最终接受；`temporary_failure`、`permanent_failure`、`not_submitted` 表示不同的已知失败；DATA 正文边界后丢失结果为 `uncertain`。退出 0 表示全部接受，1 表示存在已知失败，2 表示参数／配置／输入等本地失败，3 表示存在未知结果；取消前没有未知结果则退出 130。已接受者不因其他收件人失败而重发。
- 操作系统强杀、输出管道中断或机器掉电仍可能使用户拿不到最终报告；CLI 不承诺端到端恰好一次。不应对整个失败命令盲目自动重试，尤其不能把 `uncertain` 当作未发送。

## 命令接口

```sh
rustymail send --config client.toml --to bob@example.com --file message.eml
rustymail send --config client.toml --from alias@example.com --to bob@example.com --to carol@example.com < message.eml
rustymail serve --config deploy/rustymail.lab.toml --mode lab
rustymail check --config deploy/rustymail.lab.toml
rustymail admin --config deploy/rustymail.lab.toml queue list
```

`serve` 默认保留 production 拒绝行为；实验需明确选择 `lab`、`lab-tls`、`lab-smtp` 或 `lab-relay`。客户端可以提交给指定的 SMTP 服务，并不解除本项目服务端的环回限制。文件是原始邮件，shell 文本管道可能改变编码／换行；跨平台优先使用 `--file`。

当前地址仅支持 ASCII dot-atom 子集；预检验证行和头部结构，不是完整 RFC 5322/MIME 校验器。From、Date、Message-ID、编码和附件由原文生成者负责。`--from` 只覆盖信封发件人，不改写头部 From；上游仍可根据账号权限拒绝提交。

## 完成条件

需要验证：旧入口兼容；仅客户端配置即可发送且不创建服务器数据；真实 TLS/STARTTLS 与 AUTH；错误 CA／主机名／过期证书不发送密码；文件和 stdin 一致；Bcc 与 Return-Path 清理；恶意／超限输入在任何网络提交前拒绝；多收件人部分接受、4xx/5xx、丢失最终回复及退出码；原队列 EOF 校验、取消及锁生命周期回归。CLI 结果必须与独立对端实际收到的字节相符，不能只测试 JSON 字段。

## 配置与结果示例

复制 [客户端配置](../deploy/rustymail.client.example.toml)，填写实际 SMTP 服务地址与认证账号。CA 文件是 PEM 信任根集合；Linux 示例路径是常见系统 CA bundle，需确认发行版实际位置；Windows 需自行指定可信 PEM 文件，本版本不自动加载系统证书库。`password_file` 保存应用密码，可带一个结尾换行；Unix 要求仅所有者可读写（通常 `chmod 600`）。密码不放在命令行或 TOML 文本里。未配置认证时同时省略 username 和 password_file；没有明文认证模式。

创建一个满足当前输入契约的原文（Python 只用于生成示例，不是运行 CLI 的依赖）：

```sh
python -c "from pathlib import Path; Path('message.eml').write_bytes(b'From: alice@example.com\r\nTo: bob@example.com\r\nDate: Sat, 03 Oct 2026 12:00:00 +0800\r\nMessage-ID: <example-1@example.com>\r\nSubject: CLI hello\r\n\r\nHello from rustymail.\r\n')"
rustymail send --config client.toml --to bob@example.com --file message.eml
```

示例输出：

```json
{"recipient":"bob@example.com","smtp_code":250,"status":"accepted"}
```

这表示 SMTP 上游接受了责任，不表示邮件已进入最终收件箱。若一个命令有三人，输出依次为 accepted、permanent_failure、uncertain，则退出 3；第一人已经接受，第二人被拒绝，第三人需要核查。每人一次独立 SMTP 事务，其他人的失败不会撤销已接受者。

| 状态 | 含义与操作 |
| --- | --- |
| accepted | 收到最终成功响应，保留结果，不重发 |
| temporary_failure | 明确 4xx，保留原文；客户端本身不安排重试 |
| permanent_failure | 明确 5xx，核实收件人或政策后再决定后续 |
| not_submitted | 未确认提交，可能为连接、TLS、AUTH、能力或本地读取限制；检查配置及上游 |
| uncertain | 已进入正文阶段但缺失确定结果，先向服务器查证 |
| cancelled / not_attempted | 提交阶段 Ctrl+C 取消，或因取消跳过后续收件人 |

进入提交阶段后 Ctrl+C 会关闭当前尝试并尽量输出状态；读取 stdin 和初始化阶段使用普通进程中断行为。stdin 在 EOF 前不建立 SMTP 连接，输入字节数有上限，但等待输入的时间没有自动限时。若进程强杀或临时文件删除失败，私有临时目录中可能留有原文副本，按本机临时文件维护策略处理。

## 从实现理解取舍

### 可执行入口与库不是同一层

[统一入口](../crates/server/src/bin/rustymail.rs)先解析角色，再选运行时。send/admin 使用单线程异步调度；serve 使用多线程调度。阻塞输入／磁盘工作仍交给阻塞线程池。“单线程异步”不意味着整个进程只有一个线程，也不意味着不占用磁盘。

[客户端库](../crates/client/Cargo.toml)只依赖协议、配置、TLS 和临时文件等能力，没有 SQLite、Argon2 或服务端依赖。统一二进制包含服务端代码，但客户端路径不初始化这些服务。现阶段未提供单独的精简客户端可执行文件或 feature 裁剪承诺。

只需要统一程序时，可执行 `cargo build -p rustymail-server --bin rustymail --release --locked`；不必同时分发两个兼容入口。二进制的磁盘大小、操作系统实际映射的代码页、运行时堆分配是不同指标，不能由文件大小直接推算发信内存。

队列保留自己的 `QueueLease` 和持久化状态；[适配层](../crates/server/src/relay.rs)把共享 SMTP 结果转换为队列结果，并在正文前提交阶段标记。CLI 使用内存标记来判断未知结果。这样复用的是网络交换算法，持久责任仍由各自调用方负责。

### 为什么先做快照

如果直接为每个收件人重读输入文件，用户或编辑器可能在两次尝试之间修改它；stdin 更无法重新读取。25 MiB 输入上限允许使用有限临时磁盘换取一致重放，同时保持固定内存缓冲。快照只解决本次命令内的一致性，不是服务器 WAL/FULL 队列，不执行持久化承诺。

预检使用 16 KiB 输入块、约 17 KiB 输出容量和最多 1000 字节的行缓冲；发送时使用独立的有界读取／点转义缓冲。这里描述应用缓冲，不包括 TLS、运行时、线程栈、系统页缓存，也不是进程 RSS 实测。Unix 临时文件权限由 tempfile 限为所有者访问；Windows 依赖用户临时目录的访问控制。磁盘不足时预检失败，尚未向任何收件人发起提交。

[Snapshot](../crates/client/src/message.rs)通过 `NamedTempFile::reopen` 取得独立读取位置。不能用 `File::try_clone` 后简单 seek 来假定游标彼此独立：取消后的阻塞读可能仍在运行，共享位置会干扰下一次尝试。[tempfile 官方 API](https://docs.rs/tempfile/3.27.0/tempfile/struct.NamedTempFile.html#method.reopen)解释了重新打开时的文件身份与游标保证。

本轮为复用已验证的单收件人引擎，每人独立建立连接和发送正文；一封邮件给 N 人会传输 N 次正文，也有 N 次握手成本。这适合先验证个人 CLI 的结果语义，还不是群发吞吐优化。后续可在保留逐人状态的前提下评估多 RCPT、连接复用与并发；不能把“固定缓冲”写成“所有场景速度最快”。

### 为什么已经发送正文仍不能自动重试

成功响应可能在网络上丢失。此时对端可能已经接受，客户端却无法区分“接受但回复丢失”和“没有接受”。我们在正文前标记阶段，因此某些实际上没有最终提交的中断也会保守地记作 uncertain；用更多重复风险换取一个漂亮的布尔成功值并不可靠。参考 [RFC 5321](https://www.rfc-editor.org/rfc/rfc5321) 的 DATA 完成／超时责任及 [RFC 3207](https://www.rfc-editor.org/rfc/rfc3207) 的 TLS 状态重置规则。

## 实验与自测

```sh
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/cli_smoke.py
```

[独立实验](../scripts/cli_smoke.py)只建立临时账号和环回连接，覆盖两种 TLS、stdin／文件、清理敏感头部、去重、大小／语法预检、证书拒绝、逐人结果和与 rustymail 服务端互通；Linux 额外验证正文后 SIGINT 的未知结果与后续跳过。共享传输原有分片、校验 EOF、取消持锁等回归继续运行。

本轮执行环境、原始结果与未验证范围见[验证报告](../reports/cli/validation.md)。

自测：若输出记录不全，能否重跑整个命令？若 `--from` 与头部 From 不同，究竟哪个是退信地址？若上游没有 8BITMIME，是否可以偷偷发送高位正文？答案要点分别是先核查已接受责任、SMTP 信封 sender、拒绝而非违反已协商契约；本 CLI 不自动改写 MIME。
