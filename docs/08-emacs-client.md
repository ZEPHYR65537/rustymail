# Emacs 客户端：Gnus 收信、SMTP 发信

## 1. 方案与当前验证边界

采用 Emacs 内置 Gnus / nnimap 读取邮箱，smtpmail 发信，auth-source 读取加密凭据。不依赖 mu4e、notmuch、mbsync 或额外本地索引；先保持客户端安装简单，把注意力放在协议和服务端。

[rustymail.el](../emacs/rustymail.el) 是实际配置文件，加载本身不联网；显式调用 `rustymail-setup` 后才应用设置，`M-x gnus` 才开始连接。已用本机 Emacs 31.1 检查加载和变量接口，详细结果见[验证记录](validation.md)。目标兼容 Emacs 29+，旧版本尚未验证。当前 0.8.0 服务端已有 TLS/AUTH 和固定上游 SMTP 发送，但仍没有 IMAP。本轮使用独立 Python 对端验证中继，尚未完成本配置的 Emacs/Gnus 收发互通；客户端保留 TLS 验证要求，完整验收等待 M5。实验入站端口为 2465/2587，本文件仍是未来生产 465/993 配置，不能直接把实验服务当成已可用的 Gnus 邮箱。

配置面向**独立的单账户邮件 profile**，会修改全局身份、SMTP、auth-source 和 Gnus server 列表。已有多账户 Gnus 配置应在独立 Emacs 环境试用，再按账户 context 合并，不能直接覆盖生产工作配置。

## 2. 前置条件

- Emacs 含 GnuTLS；执行 `M-: (gnutls-available-p)` 应返回非 nil。
- GnuPG / EasyPG 能创建和读取 `.gpg` 文件；加密密钥或对称口令能正常解锁。
- 服务端账号和应用密码已经创建，证书匹配真实主机名，465/993 可达。
- 邮箱存在 INBOX、Sent、Drafts、Trash、Archive、Junk；首发服务端账户创建流程预建这些文件夹。

本客户端文件用标准 465/993；实验服务模板用 2465/2993 等环回高端口。进行 L0 实验时必须同步更改客户端端口和 authinfo 的 port，不能把两组模板直接拼起来宣称开箱即用。生产则在服务端显式启用标准端口。

## 3. 加载配置

将配置文件放到自己的 Emacs 配置目录，例如 `~/.emacs.d/lisp/rustymail.el`，在专用 profile 的 init 中加入：

```elisp
(add-to-list 'load-path (expand-file-name "lisp" user-emacs-directory))
(require 'rustymail)
(setq rustymail-address "alice@example.com"
      rustymail-full-name "Alice"
      rustymail-host "mail.example.com"
      rustymail-auth-file "~/.authinfo.gpg")
(rustymail-setup)
```

替换为真实地址、姓名和证书主机名，不把 URL 或端口写进 `rustymail-host`。Windows 的 `~` 是 Emacs 当前 home，未必与你的终端完全相同；可用 `M-: (expand-file-name "~/.authinfo.gpg")` 核对位置。

本任务只生成文件，没有修改你已有的 Emacs 配置或凭据。

## 4. 创建加密凭据

在 Emacs 中用 `C-x C-f` 打开 `~/.authinfo.gpg`，直接在加密文件 buffer 内输入：

```text
machine mail.example.com login alice@example.com port 993 password YOUR_APPLICATION_PASSWORD
machine mail.example.com login alice@example.com port 465 password YOUR_APPLICATION_PASSWORD
```

用正常 EasyPG 流程加密保存，重新打开验证能解密。不要先把真实密码写进仓库里的 [authinfo.example](../emacs/authinfo.example)，也不要把明文文件简单重命名为 `.gpg`，后缀本身不提供加密。Linux 下将文件权限限制为 0600，Windows 下限制相应用户 ACL。

两个条目分别匹配 IMAP 和 SMTP 端口；本机已核对 smtpmail 的账号与端口查找方式。配置只允许这一个加密 auth-source 文件，不回退到明文 `.authinfo` 或 `.netrc`。[auth-source 官方说明](https://www.gnu.org/software/emacs/manual/html_node/auth/Help-for-users.html)

应用密码可与该 Emacs 设备绑定，丢失设备后单独撤销。更改凭据后执行 `M-x auth-source-forget-all-cached` 并重新连接；客户端内存缓存时间设置为 300 秒。缓冲区、交换区、崩溃转储和备份仍要由操作系统保护，不能把 `.gpg` 等同于运行时所有副本都加密。

## 5. TLS 的具体设置

收信：`nnimap-stream` 为 `tls`，端口 993；发信：`smtpmail-stream-type` 为兼容写法 `ssl`，端口 465。这里的 `ssl` 是 Emacs API 名称，实际协商现代 TLS，不是在启用 SSLv3。新版 smtpmail 也接受 `tls`；使用兼容拼写方便之后验证较旧 Emacs。[Gnus IMAP 连接参数](https://www.gnu.org/software/emacs/manual/html_node/gnus/Customizing-the-IMAP-Connection.html)、[Emacs SMTP 手册](https://www.gnu.org/software/emacs/manual/html_mono/smtpmail.html)

开启 `gnutls-verify-error` 和较高网络安全检查。证书错误时修复主机名、证书链或受信 CA，不设置“忽略证书错误”。若改用 587，必须同时设端口 587、`smtpmail-stream-type 'starttls`，并修改 authinfo 的端口；不能只改端口。

AUTH PLAIN 的可用前提是已经建立并验证 TLS。它与明文 TCP 上传密码有本质不同，但也不代表端到端加密邮件正文。

## 6. 日常使用

| 任务 | 操作 |
| --- | --- |
| 启动收信 | `M-x gnus` |
| 刷新组列表中的邮件 | 组 buffer 按 `g` |
| 写新邮件 | Gnus 组 buffer 按 `m` |
| 回复 | 摘要 buffer 按 `r` |
| 添加附件 | 写信 buffer 中 `M-x mml-attach-file` |
| 提交邮件 | 写信 buffer 中 `C-c C-c`；先检查 From/To/Cc/Bcc |
| 把文章移到文件夹 | `M-x gnus-summary-move-article`，输入如 `nnimap+rustymail:Archive` |
| 查当前模式操作 | `C-h m`，以当前 Emacs/Gnus 显示的绑定为准 |

Gnus 可能默认隐藏没有未读邮件的文件夹，可从 server/group 浏览视图订阅相应文件夹。不要据此判断服务器丢了 Sent。第一次登录先确认文件夹名称，服务端保持 ASCII 系统文件夹名以简化跨客户端互通。

配置优先显示 text/plain，阻止远程图片加载，关闭 Gnus Agent 自动离线处理和 SMTP 本地排队；这使失败路径较直接。仍保留写信 buffer，默认不自动 expunge。标记 deleted 与永久删除不同，真正清理需显式操作，并确认保留期和备份。

关闭本地自动队列不等于离线邮件已提交；网络失败时由用户保留草稿。草稿同步到服务器属于后续 Gnus 工作流验证，当前不宣称本地草稿自动成为 IMAP Drafts。

## 7. Sent 归档和重复发送

配置把已发邮件归档到 `nnimap+rustymail:Sent`，通过 IMAP APPEND 完成；服务端不要再自动存一次 Sent。带有明确 select method 的归档组由 Gnus 支持。[Gnus 归档说明](https://www.gnu.org/software/emacs/manual/html_mono/gnus.html)

顺序是 SMTP 提交，然后归档；二者不存在共同事务。若发信成功但 APPEND 失败：

1. 保留写信 buffer 和错误信息，不直接再次按发送。
2. 查服务端接受记录或让收件方确认；仅靠 Sent 为空不能推断未发送。
3. 修复配额/IMAP 问题后，只补归档。可以让管理员从已接受消息导出标准 `.eml`，在 Sent 摘要中用 `M-x gnus-summary-import-article` 导入该文件。
4. 导入文件必须是完整 RFC/MIME 原文，不是含 MML 附件标记和编辑分隔线的 Message 草稿。

若 SMTP 最终确认本身丢失，服务端和客户端仍可能无法立即判定；参考接受日志和 message_id 调查，不能承诺自动消除重复。

## 8. 逐项验收

- [ ] 收取纯文本、中文主题、HTML alternative 和二进制附件；保存附件后摘要一致。
- [ ] 发送至本域和外域；From 授权、DKIM/DMARC 结果符合预期。
- [ ] Sent 只出现一份；APPEND 失败时邮件不会自动再次发送。
- [ ] 已读/未读在第二客户端可见，移动与删除一致。
- [ ] 大邮件按预期被接收或明确拒绝，不出现无限卡住和内存增长。
- [ ] 密码撤销后新旧会话均不能继续敏感操作。
- [ ] 错误证书被拒，日志不泄露应用密码或正文。
- [ ] 服务重启后 UID 与文件夹仍正确；恢复备份后缓存失效处理正确。

当前列表均未进行真实服务端验收。

## 9. 常见故障

| 现象 | 优先检查 |
| --- | --- |
| 持续询问密码 | authinfo host/login/port 是否完全匹配，GPG 是否能解密，凭据是否撤销 |
| TLS 失败 | 实际连接名称、SAN、系统时间、CA 链和 GnuTLS 支持 |
| 能收不能发 | 465、发信凭据条目、send-as、上游队列/策略 |
| 发信成功但 Sent 为空 | IMAP APPEND、文件夹订阅、配额；不要直接重发 |
| 客户端反复重同步 | UIDVALIDITY/UIDNEXT、服务器恢复历史、会话事件一致性 |
| 邮件正文正常但附件错误 | MIME offset、literal 字节长度、CRLF 与点解码 |

排错可临时启用相关协议 trace，但只使用临时凭据和测试邮件，关闭后清理日志。不要把真实 AUTH Base64 当作脱敏内容贴到问题报告中。
