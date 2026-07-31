# Morrow Session 恢复与一致性协议

## 1. 目的与范围

本文定义 Morrow CLI、Web Server/Web UI、Desktop/WSL 与持久化 Subagent 共用的 Session 恢复与一致性协议。协议以 append-only Session Facts 作为唯一持久化事实源，由 `agent-runtime` 的纯投影器派生：

- 模型请求上下文；
- 审计历史与稳定 Turn/Step 结构；
- Web/Desktop timeline；
- 当前连接可恢复的 Operation、审批和 Subagent 快照。

协议不覆盖 TUI、会话树或分支、Provider 流跨进程续传、durable tool 自动重试、补偿事务、逐 token 持久化以及跨连接事件 replay。

## 2. 版本

| 层 | 版本 | 不兼容行为 |
| --- | ---: | --- |
| Session fact log | v5 | 非 v5 JSONL header 拒绝作为 canonical log 打开 |
| Agent execution event | v8 | 消费者必须认识完整模型消息和 tool result checkpoint 事件 |
| Session stream | v2 | Snapshot/Event 的 schema 不匹配时重新订阅或拒绝 |
| Remote protocol | v4 | Desktop 与 Workspace Agent 握手版本不同则拒绝连接 |

实现不同时生产新旧两套实时协议。旧聚合 Session 文档只作为迁移输入或兼容缓存，不能作为模型上下文或 timeline 的事实源。

## 3. 术语

- **Session**：一个持久化会话实体，由不可变 `session_id` 标识。Reset 会创建新的 Session incarnation。
- **Fact**：已经发生并成功同步到磁盘的语义事实。
- **Revision**：Session fact log 内从 1 开始、严格连续的序号。
- **Turn**：一条用户消息触发的一次 agent 轮次。
- **Operation**：当前进程中的执行实例。现阶段一个 Operation 对应一个 Turn。
- **Projection**：仅由 header 与有序 Facts 计算得到的纯数据结构。
- **Snapshot**：订阅建立时某个原子时刻的 Session、Operation、审批与 Subagent 全量状态。
- **Stream epoch**：由 `stream_id` 标识的一次 Hub 生命周期。Reset 或 Hub 重建会产生新 epoch。
- **Sequence**：单个 stream epoch 内从 1 单调递增的实时事件序号。
- **Timeline**：UI 对 `TurnProjection` 和 `OperationProjection` 的展示投影，不是独立存储。

## 4. 必须保持的不变量

1. 磁盘上的 v5 Facts 是 Session 的唯一持久化事实源。
2. Revision 必须从 1 开始严格连续；重复、缺口和中间损坏是硬错误。
3. Fact 必须在对应可恢复事件发布前完成 append、flush 和 sync。
4. Snapshot 状态与 Snapshot cursor 必须在创建 receiver 的同一 Hub 锁内捕获。
5. 客户端只应用 `sequence == current + 1` 的事件；重复事件忽略，缺口立即重新订阅。
6. 只有已提交 `TurnCompleted` 的 Turn 才能进入模型上下文。
7. Failed、Cancelled、Interrupted Turn 只进入历史，不进入后续模型请求。
8. Tool 副作用前必须已有 `ToolCallStarted`；需要审批时，已批准的 `ApprovalResolved` 也必须已提交。
9. 已开始但没有结果 Fact 的 Tool 在恢复时为 `outcome_unknown`，不得自动重试。
10. 流式 text/reasoning delta 只存在于当前 Operation snapshot 和实时事件中，不写入 Session fact log。

## 5. v5 JSONL 格式

文件位置沿用 canonical workspace scope，主 Session 文件名为 `<session>.jsonl`。首行为 header：

```json
{"schema_version":5,"session_id":"session-...","created_at_ms":1785400000000}
```

后续每行是一个 `SessionFactEnvelope`：

```json
{
  "revision": 12,
  "timestamp_ms": 1785400000123,
  "operation_id": "operation-...",
  "turn_id": "turn-...",
  "fact": {
    "type": "model_message_committed",
    "data": {
      "model_call_id": "model-call-0",
      "message": {"role":"assistant","content":"..."}
    }
  }
}
```

`operation_id` 和 `turn_id` 只在 Fact 不属于某个 Turn 时省略，例如 compaction 或 legacy checkpoint。

### 5.1 Fact 集合

- `TurnStarted { user_message, model, permissions }`
- `NoticeRecorded { message }`
- `ModelCallStarted { model_call_id }`
- `ModelMessageCommitted { model_call_id, message }`
- `ToolCallStarted { tool_call }`
- `ApprovalRequested { request }`
- `ApprovalResolved { decision }`
- `ToolCallFinished { tool_call_id, result, ok, summary }`
- `TurnCompleted`
- `TurnFailed { error }`
- `TurnCancelled { reason }`
- `TurnInterrupted { reason }`
- `ContextCompacted { summary, covered_through_turn_id }`
- `LegacyContextCheckpoint { source_schema, messages, diagnostic }`

`ModelMessageCommitted` 保存完整 assistant `Message`，包括 reasoning 和 tool calls。`ToolCallFinished` 保存完整 tool-role result message、执行状态与摘要。delta 不属于 Fact。

## 6. 投影规则

### 6.1 SessionProjection

`SessionProjection` 包含 `session_id`、当前 revision、稳定 ID 的 Turns、模型上下文和迁移诊断。相同 header 与 Facts 必须得到完全相同的投影。

Turn 状态统一为：

- `running`
- `completed`
- `failed`
- `cancelled`
- `interrupted`

Model/tool step 状态包括 `running`、`completed`、`failed`、`interrupted` 和 `outcome_unknown`。

### 6.2 模型上下文

没有 compaction 时，上下文按 Turn 顺序连接所有 completed Turn 的完整消息。

存在 `ContextCompacted` 时：

1. 插入最新 summary 对应的 system message；
2. 排除 `covered_through_turn_id` 及其之前的 Turns；
3. 只追加边界之后 completed Turns 的消息。

Compaction 只追加 Fact，不改写历史 Facts。最新 compaction Fact 决定边界。

迁移产生的 `LegacyContextCheckpoint` 优先保存旧实现实际传给模型的消息。checkpoint 之后 completed Turns 继续追加；下一次正常 compaction 会消除 legacy 特例。

### 6.3 OperationProjection

Operation 是易失、可被 Snapshot 恢复的当前运行状态，包含：

- `operation_id` 与 `turn_id`；
- 当前 phase；
- 当前 model call 的累计 text/reasoning；
- 是否可取消。

Operation 不是跨进程续跑凭据。进程重启后，未完成 Turn 会转为 Interrupted，Operation 被清空。

## 7. 保存点与副作用顺序

固定顺序如下：

```text
TurnStarted append + sync
  -> 发起第一次模型调用

ModelMessageCommitted append + sync
  -> 启动该消息声明的 tool calls

ToolCallStarted append + sync
ApprovalResolved(approved) append + sync（如需要）
  -> 执行工具副作用

ToolCallFinished append + sync
Turn terminal fact append + sync
  -> 更新投影与 Operation snapshot
  -> 分配 sequence
  -> 发布 Session event
```

前置 Fact 写入失败时，不得执行后续模型/tool 副作用。副作用后的 Fact 写入失败时，Operation 停止；恢复只能标记 `outcome_unknown`，不能推测结果或自动重试。

## 8. Snapshot 与 ordered events

### 8.1 数据结构

`SessionSnapshot` 包含：

- `session_name`、`session_id`、revision；
- `StreamCursor { stream_id, sequence }`；
- `SessionProjection`；
- `active_operation`；
- permissions、approvals、subagents。

`SessionUpdateEnvelope` 包含 stream schema、`stream_id`、sequence、`session_revision`、时间戳和 Update。

Update 仅包括：

- `TurnUpserted`
- `ContextReplaced`
- `OperationReplaced`
- `ModelStreamDelta`
- `ApprovalsReplaced`
- `SubagentUpserted` / `SubagentRemoved`
- `Notice`

### 8.2 订阅时序

```text
client                         SessionHandle / Hub
  |                                      |
  | subscribe                            |
  |------------------------------------->|
  |                         lock Hub state
  |                         create receiver
  |                         capture Snapshot + cursor
  |                         unlock
  |<-------------------------- Snapshot |
  | apply full replacement               |
  |<----------------------- Event N + 1 |
  | apply only exact next sequence        |
```

创建 receiver 早于捕获 Snapshot 返回，因此 Snapshot 建立期间发生的事件要么已经包含在 Snapshot 中，要么位于 receiver 队列中，不会落在两者之间。

### 8.3 客户端 reducer

- Snapshot 全量替换 canonical UI state。
- `stream_id` 不同：停止当前订阅并获取新 Snapshot。
- `sequence <= cursor.sequence`：重复事件，忽略。
- `sequence == cursor.sequence + 1`：应用 Update。
- `sequence > cursor.sequence + 1`：事件缺口，停止订阅并获取新 Snapshot。
- broadcast lag、WSL worker 断线或 Tauri/JS 转发失败不得静默跳过。

协议不支持从旧 cursor 跨连接 replay。任何重连都从新 Snapshot 收敛。

## 9. Web 与 Desktop

Web 选择 Session 时直接打开 v2 订阅，不再先通过 REST 拉聚合历史。REST 只负责 Session 管理、列表和 canonical fact log 导出。

Desktop 在发出 Remote v4 `SubscribeSession` 前生成 `subscription_id`。订阅响应返回前到达的相同 ID 事件先缓存；Snapshot 应用完成后再按到达顺序交给 reducer。Browser、Embedded Server、WSL、Tauri 和 JS 转发相同的 `SessionStreamFrame`。

命令接受/拒绝使用 `CommandResult`，`inspect_subagent` 等请求数据使用 `CommandData`。命令帧不参与 Session sequence；所有可恢复状态变化仍必须通过 ordered Update 表达。

## 10. 崩溃恢复

打开可写 `SessionHandle` 时：

1. 获取跨进程单写者文件锁；
2. 验证或迁移 v5 log；
3. 只修复最后一条未完成 JSONL 写入；
4. 验证 revision 连续性和中间行完整性；
5. 若发现无终态 Turn，追加 `TurnInterrupted`；
6. 将仍为 running 的 model step 标为 interrupted，将 tool step 标为 `outcome_unknown`；
7. 清空易失审批与 Operation。

完整但缺少结尾换行的最后一行会补换行；无法解析的最后一段会截断到上一条完整换行。中间损坏、重复 revision 或 revision 缺口不会自动修复。

## 11. 单写者租约

主 Session 和持久化 Subagent Session 都使用标准库文件锁。可写 Handle 在订阅或 Operation 存活期间持锁；同一 Server 内客户端共享 Handle。Handle 空闲后释放，使 CLI 与 Server 不会长期争用同一 workspace/session。

Archive 是例外的受控生命周期切换：Server 先阻止新命令并确认没有运行中的主 Turn、审批或 Subagent run，再使用现有 Handle 的租约移动日志。成功后 Handle 立即释放租约、切换 stream epoch 并向旧订阅发送失效事件；旧订阅必须重新获取 Snapshot，恢复后的 Session 使用新的 Handle。

Fact 只有在持有正确租约并确认 expected revision 与磁盘 revision 一致时才能追加。

## 12. 旧数据迁移

- v1/v2 Thread：转为 `LegacyContextCheckpoint`，保留原消息序列。
- v3/v4 Session：Turn 转为导入 Facts；summary 转为 `ContextCompacted`。
- 若 `active_thread` 与 `turns + context` 投影不一致：追加一次性 `LegacyContextCheckpoint` 并记录诊断。
- 迁移先在同目录生成并验证临时 v5 log，再把旧源原子移动为 `<session>.legacy-vN.bak`，最后安装 v5 log；安装失败会把 backup 原子移回旧路径。
- 迁移失败时旧源文件保持可读，不以部分聚合状态替代它。

持久化 Subagent 的 metadata 文档不再序列化聚合 Session；其模型上下文和历史来自相同 v5 fact log。旧 Subagent 聚合 Session 在首次恢复时迁移为 Facts。独立 agent event log 仅用于诊断/inspect，不参与上下文或崩溃恢复。

## 13. Reset 与生命周期操作

Reset 原子安装一个只有新 header 的 v5 log，生成新的 `session_id` 和 stream epoch，并清空 revision、Operation、审批与 Subagent stream snapshot。旧订阅看到 epoch 变化后必须重新订阅。

Rename、archive 和 delete 必须同时处理：

- canonical v5 log；
- `.legacy-vN.bak`；
- Subagent metadata、facts 和诊断事件；
- 对应模型选择等 Session 级附属数据。

`session export` 输出 canonical v5 JSONL，不输出投影缓存。

## 14. UI 状态边界

历史 Turn 与实时 Turn 使用同一个 `TurnProjection`。Timeline、当前 tools、running Turn 和 approvals 都从 `SessionSnapshot + ordered Updates` 派生，不能分别维护可漂移的业务副本。

React 折叠、选中、滚动跟随和面板开关等纯展示状态可以独立保存，但不得写入 Session Facts。
