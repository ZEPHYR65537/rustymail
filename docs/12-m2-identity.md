# M2：TLS、身份、授权与本地管理

本章基于 0.3.0，并补充 0.3.1 的资源与取消修复。代码继续运行在 L0 环回实验环境，提供隐式 TLS、AUTH PLAIN、应用密码、send-as、撤销、Unix 管理 socket 和证书重载。原始阶段验收见 [M2 报告](../reports/m2/validation.md)。完整公网 SMTP 属于 M3，IMAP 属于 M5；本阶段不开放外域投递或生产启动入口。

## 1. 用成熟组件承担它们擅长的责任

| 责任 | 当前实现 |
| --- | --- |
| TLS 记录、握手与密码学 | rustls 0.23.45 + tokio-rustls 0.26.5，显式选择 ring provider |
| 密码散列、PHC 编解码与验证 | RustCrypto argon2 0.5.3，Argon2id v19 |
| 凭据记录、权限变更和接受事务 | SQLite/rusqlite；schema 仍为 2，复用已有 credential/address/account 列 |
| 资源边界、会话状态、权限复查和故障行为 | rustymail 自己编排并测试 |

SQLite 提供原子事务，但不知道一个凭据撤销后应该关闭哪个 SMTP 会话。TLS 提供加密通道，但不知道登录者能否使用另一个人的 From。每层只解决它实际拥有的信息；不要把“数据库支持事务”或“连接已加密”理解为全部业务已经安全。

本阶段没有为了版本号增加 SQL 迁移：现有 schema 已能表示所需数据。增加应用功能和改变数据库结构是两件事。

## 2. 运行一次独立实验

先构建服务与临时证书生成器，再运行 Python 标准库客户端：

```sh
cargo build --workspace --locked
cargo build -p rustymail-server --example m2_certificates --locked
python scripts/m2_smoke.py
```

脚本只创建临时目录、临时 CA 和环回监听，结束后删除自己的实验数据。Windows 验证 TLS/认证与离线管理；Linux 另外验证在线管理、DATA 中撤销和证书轮换。CI 的 `--peer-denial-probe` 用临时放宽权限的实验 socket 验证不同 UID 仍被服务拒绝，需要测试机可执行 `sudo -n -u nobody`；不要对真实管理目录做这个实验。

原始结果记录提交、是否存在未提交改动、客户端 OpenSSL 版本和执行范围。`working_tree_dirty=true` 表示工作区测试，不是该 Git 提交的干净检出证据。

## 3. 手动启动 TLS 实验

以下为 Linux 命令，均在仓库根目录执行。先使用新的实验目录；证书生成器和账号创建都拒绝覆盖已有对象。

```sh
mkdir -p data
target/debug/examples/m2_certificates data/lab-certificates

target/debug/rustymailctl --config deploy/rustymail.tls-lab.toml account add alice@example.com
target/debug/rustymailctl --config deploy/rustymail.tls-lab.toml send-as alice@example.com alice@example.com
target/debug/rustymailctl --config deploy/rustymail.tls-lab.toml credential create alice@example.com --label emacs-lab --secret-output data/emacs-lab.secret

target/debug/rustymaild --config deploy/rustymail.tls-lab.toml serve-lab-tls
```

这个启动模式只监听 `listeners.submissions`，模板为 `127.0.0.1:2465`。`serve-lab` 则维持原来的匿名环回 SMTP；两个模式不能同时使用同一数据目录。Unix 管理 socket 为 `data/lab/admin/admin.sock`，目录 0700、socket 0600，只接受 root 或与服务相同 UID/GID 的进程。管理客户端也核对 socket 权限和服务端对端身份。

随机应用密码格式为 `selector.secret`：selector 为 16 字节随机值的十六进制编码，secret 为 32 字节随机值的十六进制编码。服务只保存 Argon2id PHC。CLI 在交互终端显示一次，或通过 `--secret-output` 创建新的 0600 文件；没有 `--password` 参数，非终端 stdout 默认拒绝生成秘密。文件已存在时在创建凭据之前失败。Windows 文件访问还依赖系统 ACL，生产权限保证以 Linux 为准。

另一终端运行这个有证书校验的客户端。实验 CA 只被这个 SSL context 信任：

```sh
python - <<'PY'
from pathlib import Path
import smtplib
import ssl
ctx = ssl.create_default_context(cafile="data/lab-certificates/ca.pem")
password = Path("data/emacs-lab.secret").read_text().strip()
with smtplib.SMTP_SSL("localhost", 2465, context=ctx) as smtp:
    smtp.login("alice@example.com", password)
    smtp.sendmail("alice@example.com", ["alice@example.com"],
                  b"From: Alice <alice@example.com>\r\nTo: alice@example.com\r\n"
                  b"Subject: M2 lab\r\n\r\nTLS and authorization.\r\n")
PY
```

实验服务器公布的 SMTP hostname 仍来自配置；证书生成器为客户端访问名 `localhost` 签发证书。真正部署时必须为实际客户端访问名配置有效证书；不要把实验 CA 安装成系统全局信任根。

Emacs 配置继续要求生产 TLS/IMAP。本阶段可以验证提交底层能力，但尚不能完成 Gnus 收取邮件；因此没有通过关闭证书检查或修改 IMAP 配置来伪造客户端闭环。

## 4. 认证和授权是不同的检查

TLS 后 EHLO 才公布 `AUTH PLAIN`。PLAIN 表示 SASL 认证载荷的格式，不表示此处使用明文网络；只有隐式 TLS 模式接收登录，明文模式的 AUTH 请求被拒绝。支持初始响应及 334 challenge，不支持 LOGIN/OAuth/STARTTLS。AUTH 行上限 1024 字节，其余 SMTP 命令仍为 512 字节；不宣称完整 RFC 4954 扩展一致性。

凭据的 account、scope 和 auth_epoch 共同形成 Principal。`mail` 可以申请发信，`read_only` 即使密码正确也不能发送；读邮件能力等待 M5。`authzid` 必须为空或指向同一个规范化账号，不能用 Alice 的密码请求 Bob 身份。

默认新账号只有接收权。send-as 需要管理员显式授予；地址已经属于另一账号时不能接管。新增发件别名默认不启用接收。提交同时验证 MAIL FROM 和正文 From；空信封发件人被拒绝，退信职责留给 M4。

当前只接受一个 ASCII From 地址，可带简单显示名或尖括号；折叠 From、重复 From、Sender、所有 Resent-* 字段均拒绝。这是明确受限的实验策略，不是完整 RFC 5322/MIME 地址解析器。接收过程只保留身份字段和计数，不把全部正文或全部头部复制到内存。

权限在最终接受的 SQLite 事务中再次检查。客户端通过 MAIL 校验后，管理员仍可能撤销权限；此前的检查不构成永久授权。已提交的合法邮件不会因为后来撤销凭据而消失。

## 5. 撤销必须覆盖存活会话和异步取消

在线管理不绕过存储锁，请求经同一个数据库 owner 执行：

```sh
target/debug/rustymailctl --config deploy/rustymail.tls-lab.toml --socket data/lab/admin/admin.sock status
target/debug/rustymailctl --config deploy/rustymail.tls-lab.toml --socket data/lab/admin/admin.sock credential list alice@example.com
```

从列表复制实际 selector 后，执行 `credential revoke SELECTOR`。也可执行 `account disable alice@example.com` 或 `send-as alice@example.com alias@example.com --disable`，均保留相同的 `--config` 和 `--socket` 前缀。没有 `--socket` 的身份命令为离线操作，需要先停止服务。

撤销、禁用和 send-as 变更递增账户 auth_epoch。当前实现保守结束该账户的所有旧会话，其他尚未撤销的凭据可重新登录。禁用账号会同时撤销其全部凭据；本版尚无重新启用命令。使用单独的实验账号演练这些动作。

数据库 owner 在变更提交后发布通知，即使管理请求的等待方已经取消，通知仍会发生。会话在等待命令或接收 DATA 时检查通知；最终接受仍独立复查数据库。通知用于及时结束会话，数据库事务用于决定是否允许接受。已进入提交队列的任务按 owner 顺序判断权限：先提交邮件再撤销是合法接受，先撤销再提交必须失败。

管理请求每连接一帧 JSON，请求上限 16 KiB、响应上限 64 KiB（0.3.1 修复），4 路并发、读取/写入各 5 秒期限、整个请求最多 60 秒。管理队列与公网连接准入分开。响应丢失不意味着创建或撤销没有发生：创建后秘密未拿到，应先列出凭据、撤销那条记录，再重新创建；不能靠重复操作猜测状态。

正常停止会清除本实例创建的 socket，启动不会盲删已有路径。强杀后若留下旧 socket，先确认服务已停止，再由管理员处理那个确切的旧 socket 路径。

## 6. 密码验证如何保持资源有界

默认 Argon2id 使用 64 MiB、3 次迭代、4 lanes、16 字节随机 salt、32 字节输出；最多 2 个散列任务，额外最多 16 个等待请求，等待上限 2 秒。未知账号/selector 也执行有界 dummy 验证并返回相同的通用认证失败码；这不声称网络响应时延完全相同。

验证 PHC 时，库会采用字符串中保存的参数，而非简单沿用当前 Argon2 实例参数。因此必须先检查算法、版本、输出长度以及内存/迭代/lanes 上界，才能调用验证器。调低配置上界后，超出新预算的旧 PHC 会拒绝验证；先制定凭据轮换计划。[RustCrypto 验证说明](https://docs.rs/argon2/0.5.3/argon2/)

`spawn_blocking` 中已经开始的任务通常不能被异步取消终止。本实现把 active 和 admission permit 一起移入阻塞闭包，让许可与真实计算一起结束。否则网络任务已取消、散列还在跑，却开始接纳更多散列，就会突破 128 MiB 的默认散列预算。128 MiB 只计算两路 Argon2 工作内存，不能代表整个服务 RSS。[Tokio spawn_blocking](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)

另外实施固定的 60 秒窗口：每 IP 30 次、每规范化账号 10 次尝试，成功尝试也计数；两张表分别最多 4096 项，满时拒绝新键，不无限分配。每连接最多 5 次失败；未认证会话总时限默认 60 秒。当前速率是实验期固定策略，尚未做团队共享 NAT 调参、动态配置、监控指标或生产压力验收。

M2 将有界 SASL 输入与预检后的 Argon2 放在进程内；MIME 的深度、解码放大和未知分配行为仍要求独立 worker 验证，不能由这里的结论替代。

## 7. TLS 配置与重载的边界

最低版本可选 TLS 1.2 或 1.3；握手并发、握手时间和总连接均有上限。证书文件上限 1 MiB，链最多 16 张，私钥文件上限 64 KiB。Linux 拒绝符号链接、非普通文件和组/其他用户可读的私钥。

`reload-tls` 只通过在线 socket 执行。新材料完整解析并匹配私钥后，替换新连接使用的配置；已有连接继续使用原配置，失败时保留旧配置。原始测试会验证新连接证书发生变化，以及旧连接仍能 NOOP。

服务加载检查覆盖 PEM、密钥匹配及协议配置。CA 信任、主机名和证书有效期由正常验证证书的客户端检查；当前 reload 还没有完整的 ACME/证书有效期预检与临期告警，这些仍属于 M7。过期或错误名称的证书可以被服务加载，但正确配置的客户端会拒绝握手，不能把“加载成功”当作部署成功。

TLS writer 内部有缓冲，写完一个 SMTP 回复还要 flush，才能把待发送的加密记录送到连接。这里的 flush 推进网络发送，不证明对端已经收到；最终 250 之前的持久提交规则保持不变。[tokio-rustls flush 说明](https://docs.rs/tokio-rustls/0.26.5/tokio_rustls/)

## 8. 验收题与源码入口

1. 为什么密码正确仍可能得到 553？沿 [身份存储](../crates/store/src/identity.rs) 查找 scope 和 send_enabled。
2. 管理请求超时但撤销已经提交时，为什么仍必须通知会话？阅读 [数据库 owner](../crates/server/src/worker.rs) 的取消回归测试。
3. 为什么 `timeout(spawn_blocking(...))` 本身不是计算内存上限？阅读 [认证模块](../crates/server/src/auth.rs) 的 permit 生命周期测试。
4. 为什么正确的证书和私钥仍可能被客户端拒绝？运行 [独立客户端实验](../scripts/m2_smoke.py)，观察未知 CA、错误名称和过期证书。
5. 读取 [本地管理实现](../crates/server/src/admin.rs)，解释目录/socket 权限、对端 UID/GID 和客户端反向核对分别解决什么问题。

日志只写受控事件和内部标识，不记录 AUTH、PHC、正文或生成的秘密。可控秘密缓冲使用 zeroize；这属于尽力清理，不承诺清除 TLS/标准库所有内部副本、内核缓冲或交换区。更完整的审计关联、生产进程限制和全链路性能仍在后续阶段验收。


0.3.1 为撤销检查增加跨取消的 pending 状态，并限制配置的 Argon2 内存乘并发不超过 256 MiB；速率表全表清理最多每秒一次。诊断日志改为有界、允许丢弃的后台输出，不是持久审计。详见[取消与资源边界教程](13-cancellation-and-bounds.md)。
