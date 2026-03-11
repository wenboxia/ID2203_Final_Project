# 四项 Requirement 快速回答模板

## Requirement 1：Testable Shim（可测试的接口层）

**问：在哪个文件的哪些语句实现了？逻辑是什么？**

文件：`src/bin/maelstrom_node.rs`

Shim 通过 **stdin/stdout JSON** 暴露接口：
- 第432-448行：后台线程持续读取 stdin，每行解析为一条 JSON 消息
- 第117-122行：`send_msg()` 把响应序列化为 JSON 写入 stdout

外部代理（Maelstrom / jepsen-harness）发送 JSON 消息，`body.type` 为 `"read"`、`"write"` 或 `"cas"`。第511行的 `match msg_type` 分支将其分派到 `handle_client_request()`（第201-232行）。该函数将操作封装成 `MaelstromCommand`（第37-46行的结构体，含 coordinator、client_src、msg_id、op 字段），通过 `omnipaxos.append()` 写入共识日志。

---

## Requirement 2：Client & Generator + 不确定状态

**问：在哪个文件的哪些语句实现了？逻辑是什么？**

文件：`src/bin/jepsen_harness.rs`

**Generator**（第716-757行，`run_nemesis()` 内部）：每个 tick 对每个并发客户端按概率随机生成操作，发送给随机目标节点：
- 40%：`read`
- 40%：`write`（随机整数值）
- 20%：`cas`（随机 from/to 值）

**不确定状态处理**（第641-682行，`record_response()`）：
- 响应类型以 `_ok` 结尾 → 记为 `"ok"`（确定成功）
- error code 20（key不存在）或 22（CAS 前置条件不满足）→ 记为 `"fail"`（确定失败）
- error code 11 或其他 → 记为 `"info"`（**不确定**：网络分区期间请求可能已生效，也可能未生效）

WGL 检验器对 `"info"` 条目的处理（第183-204行）：将完成时间设为 `u128::MAX`，允许在历史的任意位置线性化，以保守方式覆盖最坏情况。

另外，Maelstrom 的 `lin-kv` workload 本身也充当 Client & Generator，并自动调用 Knossos 进行检验。

---

## Requirement 3：Fault Injection（Nemesis 故障注入）

**问：在哪个文件的哪些语句实现了？逻辑是什么？**

文件：`src/bin/jepsen_harness.rs`，`NetworkSimulator` 结构体（第349-421行）

`NetworkSimulator` 维护两个数据结构：
- `partitions: HashMap<(src, dest), bool>`：记录当前被分区的有向链路
- `killed_nodes: Vec<String>`：记录已杀死的节点

**分区注入**：
- `isolate_node()`（第376-386行）：将目标节点与所有其他节点之间的双向链路加入 `partitions` 表
- `split_brain()`（第388-404行）：将集群按 mid 切成两半，前半与后半之间的双向链路全部加入分区表
- `deliver_messages()`（第621-639行）：路由消息时调用 `is_partitioned()` 检查，被分区的消息直接丢弃，模拟网络不通

**崩溃注入**：
- `run_nemesis()` 的 "kill" 分支（第774-779行）：调用 `process.kill()` 杀死 OS 进程，并记录到 `killed_nodes`
- 恢复（第802-821行）：重新 `NodeProcess::spawn()` 启动进程，发送 `init` 消息，节点以全新状态加入集群

Maelstrom 的 `--nemesis partition` 参数也实现了等价的分区注入，由 Maelstrom 框架控制。

---

## Requirement 4：Linearizability 验证 + 线性化 Read 设计

**问：在哪个文件的哪些语句实现了？逻辑是什么？**

**验证器1：Knossos（通过 Maelstrom）**

运行 `./maelstrom/maelstrom test -w lin-kv --nemesis partition` 时，Maelstrom 自动收集所有操作的 invoke/ok/fail 历史，并用 Knossos（Clojure 实现的 WGL 算法）做线性一致性检验。输出 `"No anomalies found. ಠ~ಠ"` 即为 PASS。

**验证器2：自研 WGL（Rust 实现）**

文件：`src/bin/jepsen_harness.rs`

- `check_linearizability()`（第141-235行）：
  - 按操作 ID 配对 invoke/ok/fail/info 记录，构建 `LinearOp` 列表（含 invoke_time、complete_time）
  - 按 key 分组，对每个 key 独立检验（KV store 的 key 之间相互独立）
- `try_linearize()`（第238-272行）：回溯搜索
  - 找出所有"调用时间 ≤ 当前最早完成时间"的操作（即可能先于当前最早完成的操作发生的操作）
  - 对每个候选操作，调用 `KVModel.apply()`（第77-121行）模拟状态机，检验结果是否与实际响应一致
  - 若一致，递归继续；若所有路径均不一致，返回 false（线性化失败）

**线性化 Read 的核心设计（Hint 考点）**：

> 问题：共识系统中直接做本地读不是线性化的。节点可能在分区后用旧 leader 身份服务，读到落后的值（stale read）。
>
> 解法：**所有 read 都经过 OmniPaxos 共识日志**。
> - `handle_client_request()` 第208行：`KVOp::Read` 与 write/cas 一样被 append 进日志
> - OmniPaxos 对 read 进行多数派共识，确认其在日志中的全局顺序
> - `apply_and_respond()` 第329-355行：只有当 read 被 decide（多数派确认）后，才从本地 KV store 读值并返回
>
> 保证：read 被 decide 时，所有在它之前 propose 的 write 也已经 decide 并 apply。读到的一定是最新值，不存在 stale read。代价是读延迟增加（需要一轮共识），但换来了完整的线性一致性。
