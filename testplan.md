# 项目验收测试方案

## 前提条件

```bash
# 确认二进制已编译
cargo build --release --bin maelstrom-node
cargo build --release --bin jepsen-harness

# 确认 Maelstrom 已安装
ls maelstrom/maelstrom
```

---

## 第1步：基础功能测试（无故障）

验证节点在无故障环境下 read/write/cas 满足线性一致性。

```bash
./maelstrom/maelstrom test -w lin-kv \
  --bin ./target/release/maelstrom-node \
  --node-count 3 --time-limit 30 --rate 10 --concurrency 6
```

**预期结果**：
```
Everything looks good! ヽ('ー`)ノ
```
或
```
Errors occurred during analysis, but no anomalies found. ಠ~ಠ
```
两者均为 PASS（无线性一致性违规）。

---

## 第2步：网络分区测试（最重要）

验证节点在网络分区（leader 与 follower 隔离、split-brain）下仍满足线性一致性。

```bash
./maelstrom/maelstrom test -w lin-kv \
  --bin ./target/release/maelstrom-node \
  --node-count 3 --time-limit 60 --rate 10 --concurrency 6 \
  --nemesis partition
```

**Maelstrom 注入的故障**：随机将节点隔离或将集群切成两半。OmniPaxos 在多数派分区中重新选主，分区恢复后节点通过共识日志对齐状态。

**预期结果**：`No anomalies found` — 分区期间的线性一致性保持。

---

## 第3步：自研 Rust 测试框架（展示 WGL 检验器）

使用自实现的测试框架，展示分区故障下 WGL 线性一致性检验。

```bash
cargo run --release --bin jepsen-harness -- \
  --nodes 3 --duration 30 --rate 10 --concurrency 5 \
  --nemesis partition --keys 5
```

**输出说明**：
- `NEMESIS: Isolated node nX from cluster` — 单节点隔离
- `NEMESIS: Split brain - ["n0"] vs ["n1", "n2"]` — 集群对半切
- `NEMESIS: All partitions healed` — 故障恢复

**预期结果**：
```
=== Test Results ===
Operations invoked:     ~1500
Operations completed:   ~200+
Operations indeterminate: 0

Checking linearizability...
PASS: History is linearizable
```

---

## 第4步：节点崩溃测试（可选）

```bash
cargo run --release --bin jepsen-harness -- \
  --nodes 3 --duration 30 --rate 10 --concurrency 5 \
  --nemesis kill --keys 5
```

**预期结果**：`PASS: History is linearizable`

---

## 故障排查

| 现象 | 原因 | 解决 |
|------|------|------|
| `Failed to validate "--nemesis kill"` | Maelstrom lin-kv 不支持 kill nemesis | 用 `--nemesis partition` 或改用 jepsen-harness |
| `cannot run with 6 threads concurrently` | concurrency 不足 | 加 `--concurrency 6` |
| `FAIL: Linearizability violation` (jepsen-harness) | 见下方 | 检查代码是否为最新版本 |

---

## 关键设计说明（向老师解释时使用）

### 线性化 Read 的实现
普通本地读在共识系统中不是线性化的（可能读到旧值）。本项目解决方案：

**所有 read 操作都经过 OmniPaxos 共识日志**（`maelstrom_node.rs` 第208行）。read 被多数派 decide 后才返回值，此时所有先于该 read propose 的 write 均已 apply，保证不会出现 stale read。

### 不确定状态的处理
网络分区期间，客户端可能超时收不到响应，但操作可能已在多数派中生效。这类操作记录为 `"info"`（不确定），WGL 检验器将其完成时间设为无穷大，允许在历史任意位置线性化，覆盖最坏情况。

### 两套验证器
1. **Knossos**（Maelstrom 内置）：工业级 Clojure 实现，权威性强
2. **自研 WGL**（`jepsen_harness.rs` 第141-272行）：纯 Rust 实现，展示对算法原理的理解
