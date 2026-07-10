# Morrow Agent 架构

本文面向刚接触 Rust 和 agent 工程的开发者，说明 Morrow 当前重构后的分层、依赖方向和主要扩展点。这里描述的是仓库正在落地的端口架构，不包含尚未实现的分布式会话协调或强制终止所有外部进程等能力。

## 1. 先理解三个概念

### 1.1 Crate 是什么

Rust workspace 由多个 crate 组成。一个 crate 通常对应一个相对独立的职责边界，并拥有自己的 `Cargo.toml`。拆分 crate 的主要目的不是让目录变多，而是让编译依赖明确：上层可以依赖下层，下层不能反过来知道上层的具体实现。

### 1.2 端口和适配器是什么

端口是核心逻辑需要的能力接口，通常使用 Rust trait 表达。例如，agent 核心只需要“可以流式调用模型”，因此在 `agent-core` 中定义 `Model` trait。

适配器是端口的具体实现。例如，`OpenAiCompatClient` 实现 `Model`，负责 HTTP 请求和 SSE 解析。核心状态机只依赖 `dyn Model`，不需要知道 OpenAI-compatible API 的 URL、鉴权方式或响应格式。

依赖关系因此从：

```text
core -> OpenAI 客户端
```

调整为：

```text
core <- Model trait 的实现 <- OpenAI 客户端
```

trait 由核心层拥有，具体适配器依赖并实现它，这就是依赖倒置。

### 1.3 Turn、Thread 和 Session 的区别

- `Turn`：一次用户输入到本次 agent 执行结束的状态，包括模型步骤、工具步骤和错误。
- `Thread`：下一次模型调用实际会看到的消息上下文。
- `Session`：可持久化的完整会话，包含活动上下文、所有 turn 的审计历史和压缩摘要状态。

## 2. 总体分层

```text
┌─────────────────────────────────────────────────────────────┐
│ 入口层                                                      │
│ agent-cli                 agent-server                      │
│ 参数、REPL、JSONL          HTTP、WebSocket、Web UI           │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 应用编排层：agent-runtime                                   │
│ 运行一次 turn、上下文压缩、事件封装、SessionStore、MCP 装配 │
└──────────────────────────────┬──────────────────────────────┘
                               │ 注入端口实现
                               ▼
┌─────────────────────────────────────────────────────────────┐
│ 核心层：agent-core                                          │
│ Agent turn 状态机、Model 端口、ToolRuntime 端口              │
└───────────────────┬───────────────────────┬─────────────────┘
                    ▲                       ▲
                    │ implements            │ implements
┌───────────────────┴──────────┐  ┌─────────┴─────────────────┐
│ agent-model                  │  │ agent-tools               │
│ OpenAI-compatible + SSE      │  │ 内置工具、ToolRegistry、MCP│
└──────────────────────────────┘  └──────────────┬────────────┘
                                                 │
                                                 ▼
                                      ┌──────────────────────┐
                                      │ agent-sandbox        │
                                      │ 路径与权限策略判断   │
                                      └──────────────────────┘

agent-protocol 位于底部，向各层提供共享数据类型。
agent-config 负责把配置文件和环境变量转换成类型安全的配置。
```

## 3. 各 crate 的职责

| Crate | 主要职责 | 不应该承担的职责 |
| --- | --- | --- |
| `agent-protocol` | `Message`、`ToolCall`、`Turn`、`Session`、审批和事件等共享类型 | HTTP、文件访问、模型调用和业务编排 |
| `agent-core` | turn 状态机，以及 `Model`、`ToolRuntime` 两个核心端口 | OpenAI HTTP、MCP transport、CLI 和持久化 |
| `agent-model` | OpenAI-compatible 请求、SSE 解析，实现 `Model` | Session 写入、工具调度和 UI 事件处理 |
| `agent-tools` | 内置工具、工具注册、MCP 工具适配，实现 `ToolRuntime` | 决定一整个 turn 的状态流转 |
| `agent-sandbox` | workspace 路径约束和权限决策 | 直接执行模型或管理 Session |
| `agent-runtime` | 编排已注入的模型端口与工具系统、上下文压缩、turn 事件封装、Session 持久化 | 解析 CLI 参数、实现模型 HTTP 协议或渲染 Web UI |
| `agent-config` | 加载并校验 `morrow.toml` 和环境变量 | 执行 agent turn |
| `agent-cli` | CLI 参数、REPL、终端输出、交互式审批 | 复制核心状态机 |
| `agent-server` | HTTP/WebSocket API、浏览器事件、远程审批和取消入口 | 实现模型协议或工具业务 |

## 4. 编译依赖方向

箭头 `A -> B` 表示 A 的代码可以导入 B：

```text
agent-cli    -> agent-runtime, agent-server, agent-model, agent-config, agent-protocol
agent-server -> agent-runtime, agent-model, agent-config, agent-protocol
agent-runtime-> agent-core, agent-tools, agent-config, agent-protocol

agent-model  -> agent-core, agent-protocol
agent-tools  -> agent-core, agent-sandbox, agent-config, agent-protocol
agent-sandbox-> agent-protocol
agent-config -> agent-protocol
agent-core   -> agent-protocol
agent-protocol -> serde / serde_json
```

最重要的约束是：

1. `agent-core` 定义端口，但不依赖 `agent-model` 或 `agent-tools` 的具体类型。
2. `agent-model` 和 `agent-tools` 依赖 `agent-core`，分别实现端口。
3. `agent-cli` 和 `agent-server` 是模型适配器的组合根；它们创建客户端，再以 `dyn Model` 注入 runtime。
4. `agent-runtime` 负责用例编排和工具系统装配，但不知道具体模型供应商。
5. `agent-protocol` 不反向依赖任何业务 crate。

以后增加模型供应商或工具运行时，通常不需要修改 turn 状态机。

## 5. 核心端口

### 5.1 模型端口

`agent-core` 中的模型边界可以简化理解为：

```rust
pub trait Model: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ModelFuture;
}
```

`ModelRequest` 包含本次对话和工具定义。模型通过流返回三种核心事件：

- `TextDelta`：一段增量文本。
- `ToolCalls`：模型请求调用一个或多个工具。
- `Completed`：本轮模型流正常结束。

`OpenAiCompatClient` 是当前适配器。它把 `ModelRequest` 转成 Chat Completions HTTP 请求，再把 SSE 数据转换成核心层认识的 `ModelEvent`。

这里使用 `BoxFuture` 和 `BoxStream`，是因为不同适配器产生的 async future 和 stream 具有不同、通常无法直接写出的具体类型。装箱后，`AgentTurnStream` 可以用统一类型持有它们，并跨多次 `poll` 推进状态机。

### 5.2 工具运行时端口

核心层看到的是较粗粒度的 `ToolRuntime`：

```rust
pub trait ToolRuntime: Send + Sync {
    fn definitions(&self) -> Vec<ToolDefinition>;
    fn execution_mode(&self, call: &ToolCall) -> ToolExecutionMode;
    fn execute(
        &self,
        call: ToolCall,
        approval: Option<ToolApproval>,
        context: ToolExecutionContext,
    ) -> ToolFuture;
}
```

它回答三个问题：有哪些工具、这个调用能否并发、如何执行调用。`ToolExecutionContext` 当前携带本次 turn 的取消信号；以后增加 trace id 或 deadline 时，也可以继续通过这个上下文传递，而不污染每个工具的业务参数。

`agent-tools::ToolRegistry` 实现该端口。Registry 内部还有更细粒度的 `Tool` trait，供单个内置工具组或 MCP 适配器实现。两层 trait 的职责不同：

- `ToolRuntime` 是 core 与整个工具系统之间的端口。
- `Tool` 是 tools crate 内部的插件接口。

读操作通常标记为 `Concurrent`，写文件和 shell 等有副作用的操作标记为 `Serial`。core 最多并发执行四个可并发调用，但会按照模型原始 tool call 顺序把结果写回对话，避免并发完成顺序改变模型语义。

## 6. 一次 turn 的完整时序

```text
CLI / Server
    │
    │ prompt + Session + 配置
    ▼
agent-runtime
    │ 1. 构建内置工具并发现 MCP 工具
    │ 2. 连同工具 schema 估算上下文并按需压缩
    │ 3. 把 Model 与 ToolRuntime 注入 Agent
    ▼
agent-core::Agent
    │ 4. system prompt + active_thread + user message
    ▼
dyn Model
    │ 5. TextDelta / ToolCalls / Completed
    ▼
AgentTurnStream
    │ 6. 如有工具调用，交给 dyn ToolRuntime
    │ 7. 如需审批，暂停并发批次，发出 ApprovalRequested
    │ 8. 工具结果写回对话，再次调用模型
    │ 9. 最多执行八轮工具调用
    ▼
TurnRecord + AgentEvent
    │ 10. Session::apply_turn
    ▼
agent-runtime / SessionStore
    │ 11. 保存 Session v3
    ▼
CLI 输出或 WebSocket 广播
```

更具体地说：

1. 入口层加载配置和 Session，并确定 workspace root 与权限档位。
2. runtime 创建 `ToolRegistry`。MCP 启动或发现失败会形成 warning，而不是让所有可用工具一起失效。
3. runtime 把工具 schema 也计入 token 估算，再决定是否执行上下文压缩。
4. core 创建 `Conversation`，其中 system prompt 不写入长期 Thread。
5. 模型文本一边到达，一边发出 `TextDelta`，入口层可以实时显示。
6. 模型请求工具时，core 先记录 assistant tool-call message，再调度工具。
7. 工具结果被转换成 `tool` role message，随后进入下一次模型调用。
8. 最终文本形成 assistant message，并生成完成的 `TurnRecord`。
9. runtime 通过 Session 的单一追加 API 提交该记录，再由入口层保存。

事件展示和 Session 持久化是两个概念。`AgentEvent` 用于实时观察执行过程，`TurnRecord` 才是会话历史的持久化事实。

事件接收方失败也不会回滚已经发生的领域事实：若投递在 turn 中途失败，runtime 会取消执行并提交一个 `Failed` record；若 turn 已经完成，则仍提交 `Completed` record，并通过 `RunAgentTurnOutcome.error` 把投递错误返回给入口层。这样 stdout/JSONL/WebSocket 的观察故障不会造成“副作用已经发生但 Session 没有审计记录”。

## 7. Session 与 Turn 不变量

### 7.1 Session 的三个部分

Session v3 保持下面的 JSON 结构：

```text
Session
├── active_thread: Thread
├── turns: Vec<TurnRecord>
└── context: SessionContext
    ├── summary: Option<String>
    └── summarized_turns: usize
```

各字段含义如下：

- `active_thread` 是下一次模型会看到的活动上下文，不是完整审计日志。
- `turns` 保存所有完成或失败的 turn，供恢复、展示和排查错误。
- `context` 记录压缩摘要，以及历史中已经被摘要覆盖的前缀长度。

当前为了保持 Session v3 JSON 兼容，这些字段仍然公开。业务代码不应分别手动 `push`，而应使用 `Session::apply_turn(record)`。

`apply_turn` 的规则是：

```text
Completed -> record.messages 追加到 active_thread，然后 record 追加到 turns
Failed    -> active_thread 不变，只把 record 追加到 turns
```

这样失败 turn 会留在审计历史中，但不会污染下一次模型上下文。`Session::try_apply_turn` 会拒绝 `Running` record；`Running` 只属于执行期，不能作为最终记录保存。

### 7.2 Turn 的状态约束

- `Running`：turn 正在执行，最后一个 step 通常也是 `Running`。
- `Completed`：存在最终 `assistant_message`，`error` 为空，最后一个模型 step 已完成。
- `Failed`：`error` 有值，活动 Thread 不应追加该 turn 的 messages。

`TurnStep` 描述一次模型调用或一次工具调用。一个整体完成的 turn 内部仍可能存在失败的工具 step，例如工具返回错误后，模型理解该错误并给出最终答复。

### 7.3 TurnRecord.messages 的含义

`TurnRecord.messages` 只保存本 turn 产生的消息链，可能包含：

```text
user
assistant(tool_calls)
tool(result)
assistant(final answer)
```

这些消息既用于审计，也用于成功 turn 的活动上下文更新。不要只保存最终 assistant 文本，否则下一轮模型会丢失工具调用和工具结果之间的对应关系。

### 7.4 上下文压缩约束

压缩不会删除 `turns`，只改变模型活动上下文：

```text
active_thread = summary system message
              + 尚未摘要的 Completed turn messages
```

`summarized_turns` 是 turn 数组中的前缀边界，不是消息数量。失败 turn 可以被摘要覆盖，但不会直接重新加入 active Thread。

## 8. 如何扩展模型

增加新模型适配器时：

1. 在 `agent-model` 或新的模型适配器 crate 中创建客户端类型。
2. 为该类型实现 `agent_core::Model`。
3. 把供应商特有响应转换为 `TextDelta`、`ToolCalls` 和 `Completed`。
4. 把供应商错误包装成 `ModelFailure`，不要让 provider-specific error 进入 core。
5. 在 CLI、server 或新的入口组合根中选择并注入该实现。
6. 为请求映射、流结束、空流、工具调用和错误流添加适配器测试。

如果供应商不使用 OpenAI 消息格式，转换逻辑仍应留在适配器中，而不是给 core 增加供应商分支。

## 9. 如何扩展工具

增加本地工具时：

1. 实现 `agent_tools::Tool`。
2. 提供稳定且唯一的 `ToolDefinition` 名称和 JSON Schema。
3. 根据副作用选择 `Concurrent` 或 `Serial`。
4. 使用结构化参数反序列化，不手工拼接 JSON 字符串。
5. 在产生文件、shell 或其他外部副作用前完成权限与审批判断。
6. 返回结构化 `ToolResult`，必要时提供 `ToolExecutionSummary` 给 CLI/Web 展示。
7. 注册到 `ToolRegistry`，重复名称会被拒绝。

MCP 工具也会被包装成 `Tool` 并注册，因此 core 不需要区分“内置工具”和“MCP 工具”。需要注意，`agent-sandbox` 当前主要保护本地内置文件和 shell 工具；外部 MCP server 的能力边界还取决于该 server 自己的实现和配置。

## 10. 审批边界

审批由工具和权限策略触发，core 只负责暂停和恢复状态机：

```text
ToolRuntime.execute(call, None)
    │
    ├── Completed(result) ───────────────> 继续
    │
    └── ApprovalRequired(request)
            │
            ▼
       AgentEvent::ApprovalRequested
            │
            ▼
       TurnEventHandler::resolve_approval
            │
            ├── deny    -> 生成 approval denied 工具结果
            └── approve -> execute(call, Some(ToolApproval))
```

CLI 可以在终端询问用户；server 使用 WebSocket 接收浏览器决定。core 会校验 `request_id`，错误或过期的决定不能应用到另一个审批请求。

审批不是操作系统级沙箱，也不是事务回滚：

- 它必须发生在副作用之前。
- 工具实现必须验证批准的 request 与当前调用匹配。
- 新增有副作用的工具时，不能仅依赖 UI 提示文本保证安全。
- `DangerFullAccess` 会按当前策略放宽本地文件和 shell 限制，应当显式使用。

## 11. 取消边界

Web server 按 `session + turn_id` 识别运行中的 turn。取消采用协作式 `CancellationToken`，信号会传到 runtime、core 和工具层；同一 Session 的运行槽位会一直保留到 worker 真正退出。若五秒后仍未收束，server 才使用 Tokio abort 兜底，并在 task future 已被 drop 后释放槽位。

各层的实际语义如下：

- core 生命周期：未完成的 `AgentTurnStream` 被提前 drop 时会自动触发取消，避免事件处理器报错等提前返回路径留下后台工具。
- 模型请求或模型流：core 停止轮询并丢弃对应 future/stream；本地 HTTP 等待会停止，但远端服务已经收到的请求仍可能继续执行。
- 审批等待：立即停止等待，并把当前 turn 收束为 `Failed`。
- 文件变更：事务开始前检查取消；一旦提交阶段已经开始，就继续完成提交或回滚，避免留下半写状态。已经完整提交的旧操作不会因之后取消而撤销。
- shell：Unix 下每次命令使用独立进程组。timeout 或工具 future 仍在被轮询并观察到取消时，会终止进程组并异步等待根进程及 stdout/stderr 管道收束；future 被直接 drop 时，RAII guard 会同步发送 killpg，但无法在 `Drop` 中等待或回报清理错误。Windows 当前只能尽力终止根 shell，尚不具备等价的进程树保证。
- MCP：调用方在取消后立即返回，尚在 actor 队列中的调用会在执行前跳过；已经发出的远端操作是否停止，仍取决于 MCP server 和 transport。
- `spawn_blocking` 文件任务：future 被丢弃不会强制终止线程，因此提交前取消检查和事务边界仍然是必要保护。

取消不是通用回滚协议。工具仍应把审批放在副作用之前，并明确区分“可取消等待”和“必须原子收束的提交”。

CLI 没有独立的 turn cancellation 协议；进程级中断仍属于入口层行为。

## 12. Session 持久化与事件协议

`agent-runtime::SessionStore` 把 Session 保存为版本化 JSON 文档。当前写出 schema v3，并兼容读取旧的 v1/v2 Thread 文档。旧文档加载后会转换为 Session，再在下一次保存时写成 v3。

实时事件使用 `AgentEventEnvelope` 包装，包含：

- 事件 schema version。
- Session 名称和 workspace root。
- turn index 和 event index。
- 时间戳与具体 `AgentEvent`。

CLI 的 JSONL 和 server 的 WebSocket 共用该事件结构。修改事件 JSON 形状时需要把它当作外部协议变更，而不是普通内部重构。

当前 SessionStore 是本地文件存储。server 会在单个进程内阻止同一 Session 同时启动两个 turn，但不要假设多个独立进程同时写同一个 Session 文件也是安全的。

## 13. 新代码应该放在哪里

| 需求 | 推荐位置 |
| --- | --- |
| 新增共享消息、turn、审批或事件类型 | `agent-protocol` |
| 修改模型/工具循环和 turn 状态机 | `agent-core` |
| 新增模型 HTTP 协议或流解析 | `agent-model` |
| 新增内置工具或 MCP 适配 | `agent-tools` |
| 修改路径约束和权限策略 | `agent-sandbox` |
| 修改上下文压缩、SessionStore 或一次 turn 的应用编排 | `agent-runtime` |
| 新增配置字段和校验 | `agent-config` |
| 修改参数、REPL 或终端输出 | `agent-cli` |
| 修改 HTTP、WebSocket 或 Web UI | `agent-server` |

判断位置时可以问两个问题：

1. 这段代码描述的是稳定业务规则，还是某个外部系统的接入细节？
2. 替换模型、工具或 UI 后，这段代码是否仍然成立？

稳定的 turn 规则放在 core；可替换的外部细节放在适配器；跨组件用例编排放在 runtime。

## 14. 分层测试策略

- `agent-core`：使用假的 `Model` 和 `ToolRuntime`，验证纯状态机、并发顺序、轮次上限和审批恢复。
- `agent-model`：验证 HTTP 请求与 SSE 到 `ModelEvent` 的转换。
- `agent-tools`：验证参数、路径、权限、副作用前审批和结构化结果。
- `agent-runtime`：验证压缩、事件 envelope、`Session::apply_turn` 和持久化时机。
- `agent-server`：验证同 Session 的运行限制、审批 request id、取消和 WebSocket 消息。
- `agent-protocol`：锁定 Session v3、事件和消息的 JSON 契约。

端口架构的直接收益是：core 测试不需要启动 HTTP server、真实 MCP 进程或写入用户 Session，就可以覆盖绝大多数 agent 循环行为。
