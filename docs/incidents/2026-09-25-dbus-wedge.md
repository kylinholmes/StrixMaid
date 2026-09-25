# 事故：监听死锁把整台机器的 system bus 拖垮

> 2026-09-25。一台 Ubuntu 22.04 测试机上，`strixmaid.service` 连续多日
> 把 system bus 拖到不可用。原始证据（进程/socket/dbus 统计、时间线）
> 留在采集机本地，**未随仓库分发**——那些 dump 带主机名、地址与配置指纹，
> 本仓库是公开的。这里只留可公开、也是真正有用的那部分：机理与判据。
>
> 采集与初步定位由 DeepSeek 完成，其「接收侧被背压卡住」的推断方向正确；
> 本文把它坐实到具体代码行与常数。

## 1. 现象

- `dbus-daemon` 持续拒绝一切发往本进程的消息，**连 systemd 的
  `method_return` 都被丢**；`max_replies_per_connection`（默认 1024）被打满
  后长期不降，稳定 4 行拒绝日志/分钟，持续 25 小时以上从未自愈。
- 连带把 `systemd-logind` 拖垮：建不出 session scope，`pam_systemd` 每次
  超时 25 秒 → **SSH 登录每次卡 25 秒以上**，`systemctl` 全部超时。
- **进程自己看起来是健康的**：HTTP 健康端点 200、CPU≈0、RSS 25 MB。
  只有那一条总线连接死了。
- 停掉服务后一切立刻恢复（拒绝日志归零、登录回到 1.5 秒）。
- **会复发**：重启之后过一段时间又会卡住，不是一次性偶发。

决定性的一条现场证据：该进程 bus socket 的 `Recv-Q` 积压约 47 KB
**且没有任何线程在读**，所有任务都 park 在 futex/epoll 上。

## 2. 根因：四者互等的环形死锁

`providers/service/bus.rs` 的 `run_listener` 在 `select!` 的 flush 分支里
直接 `await` 了 `summaries_for`——那里面是 D-Bus 方法调用。**等回复期间，
同一个 `select!` 下的四路信号流全部停止被 poll。**

而 zbus 的读取任务对每条收到的消息，逐个 `sender.broadcast_direct(msg).await`
（zbus 5.19 `src/connection/socket_reader.rs:93`）。那是 async-broadcast 的
**有界**发送，队列满就等——没有开 overflow 模式。队列容量是 zbus 默认的
**64**（`src/connection/mod.rs` 的 `DEFAULT_MAX_QUEUED`），不是我们给
`props_changed` 那路显式设的 512，所以**最先满的是三路 `receive_*`**。

于是：

```
监听等回复 → 回复等读取任务 → 读取任务等队列腾空 → 队列等监听回去 poll
```

四者互等，**永久**，且不需要任何外部条件维持。触发只需要一次事件刷新期间
涌入 ≥64 条 systemd 信号——`daemon-reload`、批量 unit 状态变化都能轻易达到。

现场证据逐条对上：

| 现象 | 机理解释 |
|---|---|
| Recv-Q 积压且无人读 | 读取任务挂在 broadcast 发送上，不在 `read_socket` |
| 所有线程 futex/epoll | 读取与监听两个**任务**都 park（不是线程阻塞） |
| systemd 的 `method_return` 被拒 | 正是我们自己发出的调用的回复，投不进来 |
| pending replies 钉在 1024 | 调用发得出去、回复收不进来 |
| **进程连日零日志** | `run_listener` 从没返回，它退出时那条 warn 自然没打出来 |
| HTTP 仍 200 | 只有这一条总线连接死了 |

## 3. 修复（三层）

1. **根因**：冲刷移出监听循环——交给独立任务，循环里不再有任何总线
   `await`。容量 1 的 mpsc + `try_send`：同一时刻最多一批在飞，送不进去就把
   名字放回队列下个窗口再刷（天然合并，也不会无限 spawn）。去抖/合并/放回
   这套纯逻辑抽成 `FlushQueue`（`providers/service/mod.rs`），**不依赖
   D-Bus，三平台都能测**。
2. **暴露面**：监听改为**按需存在**。每 30 秒查一次还有没有订阅者，没有就
   退场并 `Unsubscribe`。退场后这条连接上不再持有任何信号流，也就没有能把
   读取任务挡住的队列了——暴露面从「7×24」缩到「有人正看着服务页的那几
   分钟」。退场判定与清标志在同一把锁内完成，否则「决定退场」与「新订阅者
   到来」之间的窗口会让那个订阅者收不到任何事件。
3. **纵深**：`summaries_for` 的两条路补上 `with_timeout`——
   `summary_no_load` 与 `list_units_raw` 曾是该文件里仅有的两个没套超时的
   总线调用，恰好就在这条死锁路径上。unit 文件补 `LimitNOFILE`。

## 4. 怎么验

`scripts/verify/` 下两个脚本配套使用：

- `dbus-wedge-check.sh`：验「现在没卡住」——拒绝日志、Recv-Q 稳定性、
  `Peer.Ping` 与 `systemctl` 延迟、logind/sshd 的关联症状。
- `dbus-wedge-stress.sh`：**制造触发条件**——`daemon-reload` 与批量 unit
  启停连续灌信号，并行按秒采样 Recv-Q。

**两个前提，漏掉哪个都会得到毫无意义的「全绿」**：

1. 必须有客户端**正订阅** `services.changed`（浏览器打开服务页）。修复后
   监听按需存在，没有订阅者时它根本不启动。
2. 判据是「能排空」而不是「始终为零」。施压期间瞬时积压正常；事故的特征是
   **固定不变且从不下降**。

单元测试证明不了这条修复——它需要真 systemd、真信号洪流。所以合并后必须
在真机上压一轮。

## 5. 教训

- **持有共享连接的事件循环里，绝不能 `await` 走同一条连接的请求。** 这类
  死锁不限于 zbus：任何「单读取任务 + 有界分发队列」的客户端库都有同构的坑。
- **看起来健康的进程可能已经功能性死亡。** HTTP 200、CPU 0%、内存平稳，
  而一条连接已经死了 25 小时。`Restart=on-failure` 对这种情况完全无效。
- **长期持有对系统总线的订阅，就是长期暴露。** 观测工具不该在没人看的时候
  还挂着订阅——这是第 2 条修复的由来。
