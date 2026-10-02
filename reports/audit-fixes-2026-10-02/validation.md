# M4.2 后审计修复验证：2026-10-02

本轮修复校验导出缺口和 CI 依赖审计缺口；版本仍为 0.8.0 / M4.2，schema 仍为 3，没有改变生产入口限制。下一阶段的 [M4.3 计划](../../docs/20-m43-delivery-lifecycle-plan.md)是待实现范围，不能算作本轮完成能力。

## 修复与边界

导出和出站队列共用 [VerifiedReader](../../crates/store/src/verified.rs)，在读取 EOF 时验证数据库记录的长度与 SHA-256。CLI 在验证与文件同步成功后才输出成功；失败则关闭并尝试删除本次创建的文件，保留原有禁止覆盖语义。通用 Write 可能已经收到部分字节，文件系统拒绝删除或进程强杀也可能留下产物，不能以文件存在代替成功确认。

新增[安全工作流](../../.github/workflows/security.yml)，覆盖 push、PR、每日与手动运行。工具固定为 cargo-audit 0.22.2，每次克隆最新 RustSec 公告并记录来源，漏洞／警告使检查失败，扫描错误不放行。归档审计结果；无漏洞忽略项，启用撤回版本检查。没有改仓库分支保护或通知偏好。

教程解释根因、失败输出契约与安全知识的更新，见[校验导出与依赖审计](../../docs/19-verified-export-and-dependency-audit.md)。

## 本地验证

| 命令／实验 | 结果 |
| --- | --- |
| `cargo fmt --all --check` | 通过 |
| `cargo clippy --workspace --all-targets --all-features --locked -- -D warnings` | 通过，[日志](windows-clippy.txt) |
| `cargo test --workspace --all-features --locked` | Windows 80 个测试通过，[日志](windows-tests.txt) |
| `cargo build --workspace --locked` 与 M4.2 探针构建 | 通过 |
| `python scripts/smoke.py` | 通过；包含三种损坏导出的非零退出、输出清理和良好副本保留，[结果](smtp-smoke.json) |
| `python scripts/m42_smoke.py` | 24 个单次尝试、5 组集成检查通过；校验读取抽取后未破坏队列发送／恢复，[结果](relay-smoke.json) |
| cargo-audit 完整正向扫描 | 157 个包（含 4 个工作区包），0 漏洞、0 警告；启用 yanked 检查，[结果](rustsec-audit.json) |
| cargo-audit 负向门槛实验 | 独立合成锁文件 time 0.3.46 命中 RUSTSEC-2026-0009，退出 1；未修改真实 Cargo.lock，[结果](security-negative.json) |

首次全量运行暴露旧 SMTP 测试时序问题：`serve_until` 完成后，取消产生的 blocking 暂存清理可能尚未释放实例锁，测试立即重新打开会得到 Locked。测试 harness 现在仅对 Locked 限时等待 5 秒，其他错误立即失败；没有移除服务端锁保护或修改服务退出承诺。随后完整检查通过，报告保存的是修正后的完整结果。

负向审计实验关闭 yanked 查询以单独验证漏洞门槛；真正锁文件的正向扫描和 CI 都未关闭该检查。最初选用的 time 0.3.0 不在该公告影响范围内，工具正确放行；核对公告版本范围后用 0.3.46 验证失败，不把“版本旧”当作漏洞判断条件。

## 来源与可复核性

这批本地实验针对基于 `56c2d0cf3d2c0bfa8e733c66985a9409d271bd40` 的修复工作区运行，故中继脚本记录的 revision 是基线且 working_tree_dirty 为 true。[来源记录](provenance.json)另存实际测试源码的 SHA-256（统一 LF）、锁文件摘要和工具／公告版本，避免把工作区结果冒称基线提交的结果。

公告库来自官方提交 [`6de4455103aced2cba86e3b86e5c090b22827cf1`](https://github.com/RustSec/advisory-db/commit/6de4455103aced2cba86e3b86e5c090b22827cf1)，提交时间 2026-10-01T22:25:27+02:00，含 1,279 条公告。cargo-audit JSON 没有写入提交字段，所以单独记录。

本节记录本地 Windows 验证；提交关联的 GitHub Actions 提供远端运行结果。没有在本地重跑 Linux VM 断电／磁盘满或性能实验，没有据此新增吞吐、内存峰值或生产可靠性声明。所有 SMTP 实验使用环回及合成邮件，没有投递外部邮箱。
