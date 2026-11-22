# 数据库服务、并发与运维

> 存储提交以 [storage.md](storage.md) 为准；查询结果以 [query-dsl.md](query-dsl.md) 为准。

## 1. 进程与部署

Bun（`apps/server`、`packages/service`）拥有公开 REST API、用户鉴权、产品工作流、
响应格式和 Web 静态资源。浏览器和 CLI 继续通过该入口访问知识库。Bun 使用自己的
服务凭证，通过私有数据库接口访问 x-sage-db。外部用户令牌的验证与数据库服务凭证
各自属于其所在层；产品用户声明不得冒充数据库认证主体。

x-sage-db 是专用 DBMS。Rust 数据库进程独占数据库目录，拥有持久化、事务、基础约束、
查询执行、版本、备份及恢复。一个进程管理多个 KB 是目标部署形态；当前候选运行时
每进程接入一个 KB。每 KB 独立 Snapshot、提交队列、OS 数据目录锁和配额。
查询线程池、写队列和后台队列的上限属于数据库服务设计。
不允许另一进程直接读取一个正在写的目录提供服务；离线工具先取得相应目录锁。

v1 不提供 HA、日志复制或多写者。单机可用 systemd/容器运行，Kubernetes 使用
单副本 StatefulSet。存储层 OS 锁保证独占，不能只依赖 RWO 卷声明。
保留 x-sage 的纯远程 CLI/viz 模型，客户端不携带知识数据与推理实现。

## 2. 协议

[类型化 schema](../../sage-db/proto/sage/v1/service.proto) 是私有数据库协议的唯一
消息契约，生成 Rust 和 TypeScript 类型。当前候选运行时使用 loopback TCP 上的
HTTP/1.1 JSON unary RPC，路径前缀为 `/sage.v1.SageCommandService/`。Protobuf 是
消息定义和生成依据；HTTP/2 gRPC 传输仍属后续设计。这个接口直接表达知识记录、
版本、事务及其结果，当前没有 SQL 前端、ODBC 驱动或 JDBC 驱动。

现有方法包括 Handshake、ReadTermText、ReadTermDocument、SubmitProposal、ReviewProposal、
CommitCommand、GetRequestOutcome。通用 Query、UploadBlob 和 Admin 属于设计面。
请求携带 KB 身份、协议版本、适用的 deadline 和 request_id（写必填）。不存在可供
多个客户端混用的全局写入缓冲区。

Bun 在产品边界将公开 REST 请求转换为数据库请求，并负责公开响应。部署不将数据库
监听对外暴露。此前 Rust 的 `/api/*` 投影用于验证数据库和迁移样本，不作为新公开
入口的依赖。首条接入使用 ReadTermText 返回正文及数据库版本信息，再由 Bun 生成
现有正文 Markdown 响应。ReadTermDocument 在同一数据库版本读取概念元数据、正文和
原始出向关系，由 Bun 组装公开的正文 JSON 和旧接口修订令牌。生产接入需补齐其余
读写用例。

## 3. 事务、提案与重试

TxEnvelope = { kb_uuid, principal, request_id, payload_digest, expect_version?,
read_set, operations }。principal 来自认证，不能由请求正文指定。
operations 是一次有界原子提交，不允许任意客户端直接调用底层事件写入器。

Proposal 是持久记录。SubmitProposal 提交候选，不修改活动事实；ReviewProposal
在最新状态检查权限与基线，将内容变更和提案最终状态同事务发布。
基线按实际对象修订和关系集合修订检查；仅依赖全库版本会让无关提案互相冲突。
显式 expect_version 仍可用于要求整个 KB 未变化的管理操作。

pending 提案允许变陈旧：只有被批准的那笔事务检查它自己的读写集合（data-model §5.1）。
普通知识提交不因其他 pending 提案陈旧而失败，也不在提交路径上扫描全部提案做基线
校验。历史修订存在性与已批准修改的等式检查仍然执行；放宽的是“谁的陈旧会阻塞谁”，
不是审计强度。

上传大 blob 使用临时 upload_id，绑定 KB、principal、长度、哈希和截止时间。
上传完成不代表知识已提交；引用该 blob 的事务才使它成为权威。过期/越权 upload_id
被拒绝；在飞上传受全局资源账本保护，避免 GC 删除。

幂等键为 (kb_uuid, principal_id, request_id)。相同键且相同 payload_digest 返回
首次持久结果；不同 payload 返回 IDEMPOTENCY_MISMATCH，不静默复用。
RequestOutcome 与内容同事务记录，先查 outcome 再校验已过期的基线。

v1 已提交 outcome 永久保留，不以 retain_versions 的 8 版本窗口删除。
outcome 大正文以 blob 引用，索引大小计入库配额；耗尽时拒绝新写，不能静默淘汰后
允许旧请求重放。后续磁盘索引或明确的有期限幂等协议再单独设计。
未提交的校验错误可以不记录；客户端修改 payload 后必须使用新的 request_id。

超时/断线不证明写失败，客户端用原 request_id 查询或重试。
进入提交持久化阶段后不响应客户端取消来撤销事务；取消只阻止尚未开始的操作。

## 4. 一致性与保留

写操作串行在最新状态上验证，读操作持有固定版本，不见部分事务。
基础图的新版本即时可查询，不等待闭包缓存预热。

| 项 | v1 默认/规则 |
|---|---|
| retain_versions | 8 个在线**整库**版本，包括当前版本 |
| 历史修订读取 | 不受 8 版本限制：按 (record, revision) 从日志重建，见 data-model §2.3 |
| max_pin_age | 300 s；可配置上限，不允许无限 |
| 单请求 deadline | 受服务端最大值限制 |
| 老版本恢复 | 在线窗口外用管理 restore，不自动重放占用普通查询线程 |
| 游标 | v1 不提供 |

“8 个在线版本”约束的是可直接按版本打开的整库快照，不是审计历史。两者不能混为一谈：
证据、提案基线与 diff 依赖按修订读取，因此不会因为版本滑出窗口而变成悬空引用。
pin、在飞请求和 backup 都登记版本引用。pin 到期拒绝新的请求并通知执行器取消；
实际文件/内存要等最后引用释放后回收，不能强制释放仍在使用的 Arc。
内存压力下先停止预热、淘汰缓存，再拒绝新 pin/查询/写入，不杀掉有效引用。

## 5. 授权

按 KB 授予 read、propose、review、write_aux、admin 能力，互不隐含继承。
普通 Agent 可 propose，不能因为具有写能力就自动具有 review 权限。
上传向量/资产和会话状态用 write_aux；导入、恢复、规则升级用 admin。

凭证使用成熟机制生成高熵不透明 token；只保存哈希，支持撤销和轮换。
执行 actor 来自已验证 principal，原文引用作者/capturedBy 作为独立来源字段保存。
部署配置与凭证不通过普通知识查询暴露；敏感参数不进入慢查询日志。

v1 不提供行级可见性与跨 KB 传播。它们需要定义“不可见支撑是否能参与证明”，
不能在目前的 KB 隔离边界上隐式加一层过滤。

## 6. 错误码与失败模式

| 错误 | 行为 |
|---|---|
| INVALID_ARGUMENT / UNKNOWN_TERM / RELATION_NOT_TRANSITIVE | 修正输入，不能当空结果 |
| CONFLICT / IDEMPOTENCY_MISMATCH | 重新审阅或使用正确请求身份 |
| REFERENCE_IN_USE / VALIDATION_FAILED | 候选事务不发布 |
| BUDGET_EXCEEDED / PROOF_BUDGET_EXCEEDED / QUOTA | 缩小请求；不返回伪完整结果 |
| DEADLINE_EXCEEDED / CANCELLED | 读结果无成功 Footer，且没有部分结果；写结果需查 outcome。两者都由调用方注入的 control hook 触发，纯推理层不读时钟 |
| VERSION_TOO_OLD / INDEX_NOT_READY | 使用可用版本或等待构建；不静默替换 |
| READ_ONLY | 拒绝写，保持已验证快照可读 |
| CORRUPT / UNSUPPORTED_FORMAT | 停止该库正常服务，显式恢复/迁移 |
| COMMIT_UNKNOWN | 按 storage §5.1：停止该库全部写入与后台维护，重试同步并校验根；验证通过才发布版本，重复失败保持只读且不重放原 request_id |
| UNAUTHENTICATED / PERMISSION_DENIED | 不泄漏无权访问对象详情 |

磁盘满可以只读降级；权威损坏不能简单归为只读继续返回不可信数据。
仅派生缓存损坏可删除重建，二者必须区分。

## 7. 资源与背压

每 KB/token 限制并发、排队长度、事务操作数、请求字节、上传暂存、查询工作区。
全局账本覆盖所有 KB，避免每库都未超限而进程整体 OOM。
网络背压之外还需有界发送队列与发送超时；传输层流控不会自动约束查询结果缓冲。
读结果遵守 Header/Row/Footer 契约。慢客户端超时释放 Snapshot 和工作区。

## 8. 运维、备份与升级

- /healthz：进程活性，不做磁盘 I/O。
- /readyz：可以提供有效读取，包括 READ_ONLY；返回每库状态。
- /write-readyz：写入可用性。磁盘满不能使只读副本从所有读流量中被摘除。
- SIGTERM：停止接收新写，排空正在持久化的提交，取消读并释放 pin。
  最终 checkpoint 是可选加速，超时可以跳过，提交耐久性不依赖停机 checkpoint。
- 指标：提交队列、commit/fsync latency、replay、cache hit、扫描步数、预算拒绝、
  当前/保留快照字节、blob/cache/staging 磁盘量、最老 pin、CORRUPT/COMMIT_UNKNOWN。
- 备份/恢复/verify 使用 storage 的协议；升级用新目录和受支持旧格式 reader。

v1 单节点停机期间不可用。正常崩溃下已 ACK 事务 RPO=0 的前提是存储同步语义被兑现；
介质损坏的 RPO 取决于已验证独立备份。RTO 包含校验、checkpoint 加载、日志重放、
基础索引构建与运维调度，必须按目标数据规模实测，不能等同于一次进程重启耗时。
