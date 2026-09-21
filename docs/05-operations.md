# 部署、运维与故障处置

> 状态：这是生产服务的目标操作契约。当前 L0 已有存储维护、身份/TLS、本地管理和三入口 STARTTLS 实验；实际命令以 [M1 教程](11-m1-storage.md)、[M2 教程](12-m2-identity.md)、[M3.1 教程](14-m3-starttls.md)和 `--help` 为准。下文生产服务、完整备份、队列等仍待实现，不能直接照做部署。DNS / OpenSSL 检查可以在已有环境使用，但 example.com、192.0.2.10、密钥和账号均为占位符。

## 1. 部署前的现实条件

选择 Linux 主机、稳定公网地址、持久磁盘、可控制的域名和 DNS。确认云厂商允许邮件业务及所需端口；直接 MX 出站需要可用的 25 出站、可设置的 PTR 和对应 IP 信誉。买到 VPS 不代表能直接投递到任何收件箱。

参考容量为 2 vCPU / 2 GiB RAM、至少 40 GiB 持久磁盘。实际磁盘按邮箱容量、队列、暂存、WAL 和备份计算；100 个各 1 GiB 的邮箱不能装进 40 GiB 磁盘。首先给总配额设置真实上限，再开账号。

只使用本地 ext4/XFS 等经过验证的文件系统，不把 SQLite WAL 放到 NFS。加密盘保护关机后的数据，TLS 保护链路，二者都不等同于邮件内容端到端加密。

开发机可以是 Windows，本设计的生产验证在 Linux 完成。Windows 文件锁和目录同步结果不能替代 Linux 的断电实验。容器也需要真实持久卷；容器重启并不能修复丢失的卷。

## 2. 端口和网络

| 端口 | 可见范围 | 说明 |
| --- | --- | --- |
| 25/tcp | 公网 | 入站 SMTP，直接出站还需允许 egress |
| 465/tcp | 公网或 VPN | 推荐的用户提交入口 |
| 587/tcp | 可选公网 | 强制 STARTTLS 的兼容入口 |
| 993/tcp | 公网或 VPN | IMAP over TLS |
| 80/443/tcp | 按证书和政策托管方式 | ACME HTTP 验证或 MTA-STS；也可使用 DNS 验证避免 80 |
| 9090/tcp | 环回/监控私网 | 指标，不开放匿名公网 |
| Rspamd / worker socket | 本机 | 与管理接口分离，不开放公网 |

正常 DNS 查询经过受控递归解析器。出站需按模式允许上游端口或公网 SMTP、DNS 与 MTA-STS HTTPS。防火墙做基础边界，应用层仍要防 DNS 指向私网和本机。

## 3. DNS 模板与检查

最小示例，发布前替换所有占位值：

```dns
example.com.                300 IN MX 10 mail.example.com.
mail.example.com.           300 IN A 192.0.2.10
example.com.                300 IN TXT "v=spf1 ip4:192.0.2.10 -all"
s202609._domainkey.example.com. 300 IN TXT "v=DKIM1; k=rsa; p=REPLACE_WITH_PUBLIC_KEY"
_dmarc.example.com.         300 IN TXT "v=DMARC1; p=none; rua=mailto:dmarc@example.com"
```

SPF 例子只适用于该 IP 直接发信；中继模式使用上游正式提供的授权配置，并验证 envelope domain 与 From 的对齐。DKIM 记录只放公钥，不放 PEM 私钥。DNS 控制台长 TXT 的分段规则由供应商决定。

PTR 在 IP 提供商处设置为 `mail.example.com`，并确认其正向地址包含本机 IP；EHLO 使用稳定完整主机名。只有完整验证 IPv6 路由、证书和 PTR 后才发布 AAAA，避免一半收件人随机失败。

`postmaster@example.com`、`abuse@example.com`、`dmarc@example.com` 必须真实存在并有人检查。初期 DMARC `p=none` 收集报告，完成所有合法发信源梳理后再调整策略；不要把“DNS 记录存在”当成签名对齐成功。

只读检查示例（在 Linux 的运维终端）：

```sh
dig MX example.com
dig A mail.example.com
dig -x 192.0.2.10
dig TXT example.com
dig TXT s202609._domainkey.example.com
dig TXT _dmarc.example.com
```

MTA-STS 的入站政策托管与服务器的出站政策执行是两件事。入站在 `https://mta-sts.example.com/.well-known/mta-sts.txt` 提供有效证书保护的政策，并发布 `_mta-sts` TXT 的版本 ID；先 testing 再 enforce。[政策模板](../deploy/mta-sts.example.txt)。TLS 报告接收地址及记录需真实配置，首发可使用外部报告查看工具。

## 4. 构建、安装与凭据

M0 后的构建流程要求：从已审查的发布 tag 构建，锁定 Rust 工具链和 Cargo.lock，`--locked` 构建，运行测试与依赖检查，输出二进制摘要和 SBOM。CI 不使用真实邮件账号进行常规测试。

安装目录约定：

```text
/usr/local/lib/rustymail/releases/<version>/   二进制，只读
/usr/local/lib/rustymail/current               当前版本指针
/etc/rustymail/server.toml                    配置，root:rustymail 0640
/etc/rustymail/tls/                           证书与私钥，最小读取权限
/etc/rustymail/secrets/                       中继密码与 DKIM 私钥
/var/lib/rustymail/                           rustymail 用户持久数据，0700
/run/rustymail/                               管理 socket，受限目录
```

使用专用无登录用户。私钥和密码不写入命令行、版本控制、普通环境变量转储或 debug 日志；配置通过 `*_file` 指向受限文件。systemd `LoadCredential` 可在实现阶段替代静态文件，接口必须统一验证。

[配置草案](../deploy/rustymail.example.toml) 默认为环回监听的 L0，防止模板被误当作可公开服务。切换生产模式需要填写真实域、TLS、扫描端点、容量与出站模式；配置检查器拒绝占位值和冲突项。

未来管理命令契约：

```sh
# 以下命令尚未实现；M0–M7 必须使它们与文档一致。
rustymaild --config /etc/rustymail/server.toml check
rustymailctl --socket /run/rustymail/admin.sock status
rustymailctl --socket /run/rustymail/admin.sock account add alice@example.com --quota-bytes 1073741824
rustymailctl --socket /run/rustymail/admin.sock credential create alice@example.com --label emacs
rustymailctl --socket /run/rustymail/admin.sock queue list --state deferred --limit 50
```

应用密码由管理工具在本机交互终端一次性显示；不能把密码作为 `--password` 参数保存进 shell 历史。CLI 大量列表默认分页，导出需显式指定输出文件和权限。

## 5. 服务启动与证书轮换

[systemd 模板](../deploy/rustymail.service.in) 给出最小权限和资源上限。MIME worker 需要自己的受限服务/cgroup，不能把文档中的“256 MiB worker 限额”误认为在主服务内自动生效。Rspamd 也独立预算。

正式启动前先离线迁移/校验数据，检查目录权限、磁盘容量和证书；生产服务不能在启动失败时自动重置数据库。readiness 只在恢复完成、writer 可用、TLS 材料就绪、必要扫描组件健康时通过。

证书轮换流程：ACME 客户端获取新材料 → 原子写入受限位置 → 配置检查 → 管理 reload-tls → 新连接验证新证书。旧连接可以用旧 TLS 会话继续完成。失败时保留旧有效配置并报警；证书临期 14/7/2 天三级告警。

在真实域名上检查 TLS，以下不会提交邮件：

```sh
openssl s_client -connect mail.example.com:465 -servername mail.example.com -verify_hostname mail.example.com -verify_return_error -CApath /etc/ssl/certs
openssl s_client -connect mail.example.com:993 -servername mail.example.com -verify_hostname mail.example.com -verify_return_error -CApath /etc/ssl/certs
openssl s_client -starttls smtp -connect mail.example.com:587 -servername mail.example.com -verify_hostname mail.example.com -verify_return_error -CApath /etc/ssl/certs
```

不能用 `s_client` 建连成功本身证明证书有效；要检查验证结果。实验 CA 显式用 `-CAfile`，不能通过关闭验证模仿生产。

## 6. 灰度与验收

先使用专门测试子域，只有自有测试邮箱；成功后再创建少量真实账户，最后切 MX。初期可中继出站，验证后再启用 L2。降低 DNS TTL 只是加速迁移的手段，不保证所有发送方立即刷新。

完整邮件互通至少包括：

1. 两个独立外部邮件服务向本域投递，Gnus 和第二客户端读取同一封原文、附件和 flags。
2. Emacs 经 465 认证发出；收件方原始头部确认 DKIM/SPF/DMARC 结果与对齐。
3. 未认证外域→外域 RCPT 被拒；认证用户伪造另一个本地 From 被拒。
4. 暂时关闭上游，确认接受后的邮件进入队列；恢复后只对未完成收件人重试。
5. 以独立虚拟机做进程终止和断电模拟；将检查点放在文件同步、rename、COMMIT、250 前后。
6. 从加密异机备份恢复到干净机器，核验 blob 摘要、UID/flags、账号和未决队列。
7. 在配置限额附近运行慢客户端、认证洪峰和 25 MiB 邮件，观测拒绝行为与内存。

当前这些均未执行；完成后在发布目录记录版本、时间、操作者、原始结果和限制。

## 7. 监控、告警和故障决策

| 信号 | 初始告警 | 处置 |
| --- | --- | --- |
| 队列最老待投递年龄 | >30 分钟提示；>6 小时升级 | 按固定原因分类定位 DNS/TLS/对端/账号限流 |
| 磁盘可用空间与预约 | 低于保留水位 | 暂停新写入；不删未送达邮件腾空间 |
| 接受提交失败 | 非零连续增长 | 查磁盘/DB；新请求临时失败，保留现场 |
| WAL 增长与 checkpoint 延迟 | >256 MiB 持续 10 分钟 | 查长事务和读者，不直接删除 WAL |
| 认证失败/昂贵验证队列 | 基线显著增加 | 启用限流，调查账号；不打印密码 |
| 退信/uncertain 异常增长 | 按滚动基线 | 查对端响应、超时与本地提交窗口 |
| 扫描器不可用 | 立即 | 暂缓接收，修复依赖，不能自动全放行 |
| 备份或恢复演练过期 | >24 小时备份未成功 | 检查存储、密钥、清单；告警不是删除旧备份 |

指标应包括提交延迟、队列数量/年龄、每类失败、活动连接、背压等待、磁盘预约、密码任务、DB 延迟、扫描延迟、RSS/cgroup 内存。避免按任意发件人或远端域名做高基数 label。

故障处置短册：

- **磁盘满：** 临时拒绝新接收，保留读取和诊断；先处理日志/过期备份或扩盘，再运行受控 GC。不得手删 blobs。
- **队列积压：** 查首个失败原因和目的域共享退避；修复 DNS/TLS/上游后逐步释放，禁止全队列无间隔重试。
- **账号被盗：** 撤销凭据、停止新提交、hold 未投递任务、审计收件人分布，处理后再释放或有记录地取消。
- **DB 引用丢失文件：** 停止受影响写入和 GC，保存数据库及日志，在隔离机恢复与比对，不运行自动“修复为删除”。
- **发出但 Sent 没有：** 检查客户端 APPEND/配额，先确认已接受记录；补归档，不直接再次发送。

## 8. 备份、RPO/RTO 与恢复

首发运维目标：异机加密备份至少每小时增量、每天完整清单；保留 7 个日版本、4 个周版本作为初始策略。计划磁盘彻底丢失时 RPO ≤1 小时、RTO ≤4 小时，**尚未演练证明**。进程崩溃下“已确认不丢”与整盘损毁后的备份 RPO 不是同一保证。

备份协调方式见[存储章节](03-storage-and-recovery.md)。包括：数据库一致快照、全部引用 blob、配置、DKIM/上游凭据的加密备份、schema 版本、摘要清单；解密密钥单独保管。证书一般可重新签发，但失去 DNS/ACME 凭据同样影响恢复。

恢复步骤：

1. 在隔离主机恢复相同兼容版本，保持端口关闭和出站暂停。
2. 校验备份清单与每个 blob；检查数据库完整性、外键和配额/计数一致性。
3. 恢复账号及密钥；对可能回退的邮箱增加 UIDVALIDITY，并记录客户端需重新同步。
4. 将结果未知的历史投递任务转待复核/可重试，不统一标记成功。
5. 本机读信验证、队列抽样、应用密码验证完成后，小批恢复出站和公开服务。
6. 核对缺失窗口、潜在重复、客户端状态和真实恢复耗时，更新报告。

每月在空机器恢复一次；“备份任务成功退出”并不能证明可恢复。

## 9. 升级、回滚和数据导出

升级前备份，停止写入，运行迁移，检查 invariants，再启动新版本。尽量采用兼容性扩展迁移；不可逆迁移需明确维护窗口。回滚程序版本不等于数据库自动降级，旧二进制必须拒绝不兼容 schema。

保留上一版二进制及配置。若只能恢复旧备份，必须解释恢复窗口内的新接受邮件如何单独保留和重放；不能直接覆盖新数据目录。没有验证过的回滚路径就不切正式流量。

导出目标为逐封 RFC 消息文件/Maildir 加元数据清单，包含文件夹、internaldate、flags 和映射；可以迁移到其他服务，不将用户绑在私有数据库格式上。外部导入先只在隔离目录解析和校验，不能覆盖当前 UID 空间。
