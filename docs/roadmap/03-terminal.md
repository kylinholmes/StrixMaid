# 03 终端：已落地设计

终端已实现，正式前端位于 `web/src/workspace/`。本文只保留仍有效的协议与生命周期约束；
原实施步骤和旧验收数字见 git 历史。目标环境验收见 [07](07-verification.md)。

## 1. 目标

用户身份下的 PTY / ConPTY，支持多终端、重连、回看、resize、退出码和空闲回收。

## 2. 实现入口

`core/src/terminal/` 管注册表，`core/src/worker/terminal/` 管真实终端；
`node/src/routes/terminals.rs` 与 `node/src/ws/terminal.rs` 暴露接口。

## 3. 设计约束

自己的终端走 user worker；指定其他用户需要管理访问并走 admin worker。
终端使用独立 WebSocket，避免刷屏挤占控制面的日志与指标。

## 4. 数据通路与生命周期

### 4.1 数据通路

浏览器 ⇄ 终端 WebSocket ⇄ 注册表 ⇄ 附件通道 ⇄ worker ⇄ PTY。
Unix 使用 socketpair + SCM_RIGHTS；Windows 使用命名管道与句柄附件。
协议实现见 `types/src/ipc.rs` 和 `core/src/session/channel.rs`。

### 4.2 身份

身份由路由的 worker 选择决定；worker 不重复实现授权策略。
Unix fork 前解析组，子进程按 setgroups → setgid → setuid 放弃权限并关闭额外 fd。

### 4.3 注册表

注册表维护所有权、回看缓冲、唯一附着、退出状态和空闲回收。
断开浏览器不关闭 shell；登出必须先关闭终端再关闭 worker。
已退出 shell 的条目短暂保留供主进程领取退出码，不能再向复用的 PID 发信号。

### 4.4 WebSocket

`/ws/terminal/{id}` 在升级前验证会话与终端所有权。数据走二进制帧，
resize 与退出通知走文本帧。刷新或断网允许重新附着。

### 4.5 RPC

`TERM_OPEN`、`TERM_RESIZE`、`TERM_CLOSE` 及其 DTO 以 `types/src/rpc.rs` 为准。

## 5. 前端

xterm.js、多标签、列表恢复与独立字节流客户端均在 `web/src/workspace/`。
工作区当前行为见 [12](12-workspace.md)，终端与文件目录不联动。

## 6. 验证约束

### 6.1 身份与附件

验证真实用户身份及 shell 不继承控制 fd。

### 6.2 生命周期

用真 PTY 检查自行退出、断线重连、重复关闭、登出回收。

### 6.3 退出帧

`term.close` 返回的 code / signal 原样传给浏览器；取不到时省略，不能编造 code=0。

## 7. 审计

REST 显式删除由路由记审计；shell 自退、空闲、登出由关闭观察者记录。
actor 在创建终端时保存，每次真实关闭只记一次，避免重复或丢失身份。
