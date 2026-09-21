# M3.1：入口职责与 STARTTLS 协议切换

本章对应 0.4.0 的第一部分 SMTP 接入实现。前置阅读：[M2 身份与 TLS](12-m2-identity.md)、[取消与资源边界](13-cancellation-and-bounds.md)。学习目标是解释收信和提交的不同信任边界，并实现不会把旧明文命令带入 TLS 的协议切换。

本阶段的测试范围、条款到测试的映射与原始证据见 [M3.1 验证报告](../reports/m3.1/validation.md)。

本章记录 0.4.0 的阶段实现，当时尚不添加 Received/Return-Path；0.5.0 已在 [M3.2](15-m3-local-delivery.md)补上最终交付表示，独立实验也同步校验新头部与保留内容。M3 整体尚未完成，仍不开放公网、外域投递、PIPELINING、SMTPUTF8 或 IMAP。不得把三个实验端口可用理解为生产 SMTP 已完成。

## 1. 先确定入口职责

同样的 MAIL/RCPT/DATA，可能服务于两个不同问题：外部邮件服务器向本地邮箱投递；本地用户登录后提交要发送的邮件。前者不应要求每个外部服务器持有本地账号密码，后者必须验证用户及其发件权限。TLS 只保护这一跳传输，不证明信封或正文 From 的真实性。

新命令 `serve-lab-smtp` 在一个进程内启动三个环回入口：

| 配置 / 实验端口 | 对应生产角色 | TLS 与 AUTH 策略 | 本阶段收件范围 |
| --- | --- | --- | --- |
| smtp / 2525 | 25 收信 | 可匿名收信；提供 STARTTLS；加密前后均不提供 AUTH | 已存在的本地收件人 |
| submissions / 2465 | 465 提交 | 连接第一字节即 TLS，随后必须 AUTH | 已存在的本地收件人 |
| submission / 2587 | 587 提交 | 先 STARTTLS，再重新 EHLO 和 AUTH | 已存在的本地收件人 |

25 角色允许没有 TLS 的本地投递，587 角色要求先升级，是入口策略区别。[RFC 3207 §4、§4.3](https://www.rfc-editor.org/rfc/rfc3207.html#section-4)

三者共享同一个 SQLite owner、应用密码服务、连接/IP 额度、握手额度与接收额度。监听端口增多不应把这些上限乘三。旧 `serve-lab` 和 `serve-lab-tls` 单入口命令继续用于已有实验；不要同时对同一数据目录运行多个实例。

收信入口的 AUTH 明确返回 502，避免提示“加密后可以登录”；提交入口在明文中拒绝 AUTH，返回 538。邮件提交即使完成 TLS，未认证的 MAIL 仍返回 530，发件身份越权返回 553，外域 RCPT 返回 550。收信入口接受的 From 仍是不可信输入，域认证和反垃圾等待 M6，不能将其作为互联网安全策略直接上线。

## 2. 升级改变的不只是 socket

一次正常升级的协议交换：

```text
S: 220 ...
C: EHLO client.example.test
S: 250-...
S: 250-STARTTLS
S: 250 ENHANCEDSTATUSCODES
C: STARTTLS
S: 220 2.0.0 Ready to start TLS
   <TLS handshake>
C: EHLO client.example.test
S: 250-...
S: 250-AUTH PLAIN      # 只在提交入口提供
S: 250 ENHANCEDSTATUSCODES
C: AUTH PLAIN ...
S: 235 ...
```

TLS 握手完成后没有第二个 SMTP 欢迎 banner。新的 EHLO 不再包含 STARTTLS；客户端不能再次升级。服务端丢弃 TLS 前的 EHLO 和事务知识。项目还要求扩展命令以 EHLO 为前提，HELO 不授权 AUTH。[RFC 3207 §4.2](https://www.rfc-editor.org/rfc/rfc3207.html#section-4.2)、[RFC 4954 §4](https://www.rfc-editor.org/rfc/rfc4954.html#section-4)

协议解析器只识别无参数 STARTTLS，并产生升级 action；有多余参数返回 501。是否支持 TLS、当前是否加密、有没有握手额度属于 I/O 和入口策略。握手额度不足时在 220 之前返回 454，不启动握手。

## 3. 预读缓冲为什么是安全边界

假设一次 TCP 读取拿到：

```text
STARTTLS\r\nEHLO injected\r\nMAIL FROM:<evil@remote.test>\r\n
```

分帧器只消费第一行，BufReader 可能已把后续明文保存在内存中。如果只替换底层 socket，却保留读缓冲，TLS 成功后这些明文就可能变成可信新会话的命令。

当前实现明确执行：取得握手额度与证书配置 → 发送并 flush 220 → 丢弃旧 BufReader 的预读字节 → 合并同一连接的读写部分 → TLS 握手 → 建立新的读缓冲与 Session。尚留在内核中的字节进入 TLS 解码，错误明文会导致关闭，绝不回退成 SMTP。

所有权实现见 [transport](../crates/server/src/transport.rs) 与 [SMTP 会话](../crates/server/src/lib.rs) 的 `discard_read_buffer`、`Action::StartTls`。Transport 使用显式明文/TLS 枚举，不使用 unsafe，也不把同一个 socket 的协议模式隐含在任意布尔组合里。不同端口仍复用同一个 SMTP 会话处理器。

回归测试用内存双工流确保后续明文已进入读缓冲，再执行真正使用的丢弃函数，验证之后只能读到新写入的字节。独立 TCP 实验则允许两种安全结果：预读内容被丢弃后继续完成 TLS，或余下明文触发 TLS 错误并关闭；两种情况都不得执行夹带的 SMTP 命令。单靠一个 TCP 分包布局无法保证覆盖所有预读时机。

TLS 失败时可能发送二进制 alert。用 SMTP 文本读取器观察到字节，并不代表回退成了 SMTP；独立测试同时检查响应代码和连接关闭，不把 alert 错判为有效 SMTP 回复。

## 4. 状态和额度不能偷偷续期

握手成功会清除 EHLO、信封和收件人；即使 TLS 前已有合法 MAIL/RCPT，握手后也不能直接 DATA。认证必须在加密后的新 EHLO 之后进行。已经建立 TLS 的连接再次 STARTTLS 返回 503。

连接许可覆盖整条 TCP 生命周期；握手许可只覆盖真实握手；提交的未认证截止时间从初始连接处理开始计算，跨升级保留。不能在 STARTTLS 后重新给一整段登录等待时间。取消或超时会销毁 TLS future 并关闭连接；磁盘接收仍使用前章的独立资源生命周期规则。

证书配置在每次握手前从共享配置获取，管理重载供后续握手使用；已握手连接保持原配置。握手失败无明文 fallback。最低 TLS 版本和证书材料限制沿用 M2。

## 5. 可运行实验

先运行一次性独立实验。它不使用真实账号，不修改全局证书信任，也不需要手动清理数据：

```sh
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/m3_smoke.py
```

预期输出为 JSON：记录当前 revision、工作区是否有修改、平台、OpenSSL 版本和通过场景。失败会非零退出，不能只看报告文件是否存在。它最终重启/离线打开存储，断言只接受四封合法邮件，逐字节核对导出；非法输入不能留下接受记录。Linux 另验证管理状态和升级后 DATA 中的撤销。

实验中的正常认证/准入场景使用 30 秒未认证窗口；验证升级不续期时再重启成 5 秒窗口。在开发中曾把所有场景都压成 5 秒，导致创建多个连接时，早先连接可能在下一次准入检查前到期，混淆“共享额度”与“超时释放”两条规则。现在额度测试先逐一确认占位连接仍存活，短期限实验单独运行；测试环境也需要清楚的前置条件。

手动体验可沿 [M2 手动实验](12-m2-identity.md)创建临时证书、Alice 账号、send-as 和应用密码，再把启动命令换为：

```sh
target/debug/rustymaild --config deploy/rustymail.tls-lab.toml serve-lab-smtp
```

使用新的实验目录或复用已经初始化的实验目录；已有账号和秘密文件不要重复创建。下方客户端示例使用 POSIX shell 的 heredoc；PowerShell 用户可将 `PY` 标记之间的 Python 代码保存成临时 `.py` 文件，再从仓库根目录执行。执行时服务须已启动：

```sh
python - <<'PY'
from pathlib import Path
import smtplib, ssl
ctx = ssl.create_default_context(cafile="data/lab-certificates/ca.pem")
password = Path("data/emacs-lab.secret").read_text().strip()
with smtplib.SMTP("localhost", 2587, local_hostname="client.example.test") as smtp:
    smtp.ehlo()
    assert smtp.has_extn("starttls") and not smtp.has_extn("auth")
    smtp.starttls(context=ctx)
    smtp.ehlo()
    assert not smtp.has_extn("starttls") and smtp.has_extn("auth")
    smtp.login("alice@example.com", password)
    smtp.sendmail("alice@example.com", ["alice@example.com"],
                  b"From: alice@example.com\r\nTo: alice@example.com\r\n"
                  b"Subject: STARTTLS lab\r\n\r\nOne verified TLS hop.\r\n")
PY
```

有意去掉 `starttls` 后登录应失败；使用默认系统 CA 而不提供实验 CA 时握手应失败；把收件人改成外域应得到 550。不要通过关闭证书验证把错误变成“成功”。Emacs 默认仍使用隐式 TLS 提交，其收信闭环继续等待 M5。

## 6. 自测与后续依赖

1. 为什么 2525 不要求本地登录，2587 却必须登录？答案须区分接收外部邮件和用户委托提交，不可只回答端口不同。
2. 为什么 TLS 后不能沿用旧 BufReader？指出用户态预读与内核未读字节分别进入哪个解析器。
3. 为什么要先拿握手额度再发 220？220 之后对端会发送 TLS 字节，此时再返回明文 454 会破坏协议阶段。
4. 为什么一次握手成功不能证明发件人可靠？它验证本跳传输及服务端证书，不代替本地 send-as 或域认证。
5. 为什么 M3.1 还不等于 M3？Received/Return-Path、完整输入契约、能力矩阵与更完整接收负例仍需完成，公网还受 M6/M7 门槛约束。

M0 未完成项按真实依赖继续保留：SMTP 输入契约在 M3 细化，IMAP literal 实验是 M5 的前置，MIME worker 隔离在接入不可信 MIME 解析前完成。不会因为 STARTTLS 可用就将整个 M0 或 M3 标为完成。
