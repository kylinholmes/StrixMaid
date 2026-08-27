//! worker 内的 provider 注册（`roadmap/01-worker-execution.md` §4.2）。
//!
//! # 这里是授权模型真正落地的地方
//!
//! `design.md` §5.1 的核心是「以登录用户身份执行操作，由 PAM / polkit /
//! journald ACL / 文件权限裁决」。worker 由 helper `setuid` 到登录用户后 exec，
//! 因此**在这里构造的 provider 天生就是那个用户的**：
//!
//! - `pick_service_provider` 在 worker 内连 system bus 时，zbus 的 EXTERNAL 认证
//!   携带的是该用户的 uid，polkit 据此裁决；
//! - `scope = user` 连的是 `/run/user/<uid>/bus`，此时 uid 即 worker 自身，
//!   认证天然通过——这直接解决了跨用户的问题，不需要额外机制；
//! - `journalctl` / `log show` 子进程继承 worker 的 uid，可见范围由 journald ACL
//!   （macOS 上由统一日志的权限）决定；
//! - 文件与信号由内核按 uid 裁决。
//!
//! 服务端因此**不含任何授权判断代码**——它只决定「派给哪个 worker」。
//!
//! # provider 缺失不是错误
//!
//! 某个 provider 在本机不可用时（没有 systemd、没有 journald），对应方法返回
//! `capability_unavailable`（501），而不是让 worker 起不来。这是 `design.md` §1
//! 第 2 条「优雅降级，而非报错」在 RPC 层的落地。
//!
//! # 每个 worker 一份 provider 实例
//!
//! `ProcProvider` 的 CPU% 差分快照是实例状态，随 worker 生命周期存在。
//! 因此**每个会话第一次取进程列表时 CPU% 为 0**（没有基线）——这与改造前
//! 主进程共用一个实例的表现一致，只是「第一次」的粒度从进程变成了会话。

use std::sync::Arc;

use futures::stream::{self, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use strixmaid_types::log::LogQuery;
use strixmaid_types::rpc;
use strixmaid_types::{ApiError, ApiResult};

use super::Dispatcher;
use crate::providers::log::{LogProvider, pick_log_provider};
use crate::providers::process::ProcProvider;
use crate::providers::service::{ServiceProvider, pick_service_provider};
use crate::providers::system::HostProvider;

/// 构造 worker 的完整分发表。
///
/// 探测 service / log provider 各一次（都要起子进程或连 bus，不该每次调用都做）。
pub async fn default_dispatcher() -> Dispatcher {
    let mut d = Dispatcher::new();
    register_host(&mut d);
    register_proc(&mut d);
    register_service(&mut d, pick_service_provider().await);
    register_log(&mut d, pick_log_provider().await);
    register_caps(&mut d);
    // 终端不是 provider：它没有「本机可不可用」的问题（PTY 是内核基本功能），
    // 也没有可替换的后端，因此直接由 `worker::terminal` 自己挂进来。
    // 返回的终端表在这里被丢掉——三个 `term.*` 处理器各自持有一份克隆，
    // 分发表活着它就活着，分发表析构时最后一份消失，终端随之回收。
    super::terminal::register(&mut d);
    tracing::info!(methods = ?d.methods(), "worker 分发表就绪");
    d
}

// ---------------------------------------------------------------------------
// 参数与结果的样板
// ---------------------------------------------------------------------------

/// 把 JSON 参数解成 `P`。
///
/// 解不出来是**主进程的错**（它构造了不合协议的调用），不是用户的错，
/// 因此报 `internal` 而不是 `invalid_request`——后者会让前端以为是用户输入有问题。
fn params<P: DeserializeOwned>(method: &'static str, v: Value) -> ApiResult<P> {
    serde_json::from_value(v).map_err(|e| {
        ApiError::internal(format!("worker 无法解析 {method} 的参数")).with_detail(e.to_string())
    })
}

/// 把 JSON 参数解成 `P`，但允许「没有参数」。
///
/// 订阅帧的 `params` 缺省是 `null`，而 `LogQuery` 这类全可选字段的结构体
/// 只认得 `{}`。这里把 `null` 当成「全用缺省值」，客户端就不必为了订阅
/// 全部日志而专门送一个空对象。
fn optional_params<P: DeserializeOwned + Default>(method: &'static str, v: Value) -> ApiResult<P> {
    match v {
        Value::Null => Ok(P::default()),
        other => params(method, other),
    }
}

/// 把结果编成 JSON。
fn result<R: Serialize>(r: R) -> ApiResult<Value> {
    serde_json::to_value(r).map_err(|e| {
        ApiError::internal("worker 无法序列化结果").with_detail(e.to_string())
    })
}

/// provider 不可用时统一的错误。
fn unavailable(capability: &'static str, what: &str) -> ApiError {
    ApiError::capability_unavailable(capability, format!("本机没有可用的{what}"))
}

// ---------------------------------------------------------------------------
// host
// ---------------------------------------------------------------------------

fn register_host(d: &mut Dispatcher) {
    let host = HostProvider::new();

    d.register_fn(rpc::HOST_INFO, move |_| async move {
        result(host.system_info().await?)
    });
    d.register_fn(rpc::HOST_HEALTH, move |_| async move {
        result(host.health().await?)
    });
    d.register_fn(rpc::HOST_TIME, move |_| async move {
        result(host.time().await?)
    });
    d.register_fn(rpc::HOST_SET_HOSTNAME, move |v| async move {
        host.set_hostname(params(rpc::HOST_SET_HOSTNAME, v)?).await?;
        result(())
    });
    d.register_fn(rpc::HOST_SET_TIMEZONE, move |v| async move {
        let req: strixmaid_types::system::SetTimezoneReq =
            params(rpc::HOST_SET_TIMEZONE, v)?;
        host.set_timezone(req.timezone).await?;
        result(())
    });
    d.register_fn(rpc::HOST_POWER, move |v| async move {
        let req: strixmaid_types::system::PowerReq = params(rpc::HOST_POWER, v)?;
        host.power(req.action).await?;
        result(())
    });
}

// ---------------------------------------------------------------------------
// proc
// ---------------------------------------------------------------------------

fn register_proc(d: &mut Dispatcher) {
    let proc = ProcProvider::new();

    let p = proc.clone();
    d.register_fn(rpc::PROC_LIST, move |v| {
        let p = p.clone();
        async move { result(p.list(params(rpc::PROC_LIST, v)?).await?) }
    });

    let p = proc.clone();
    d.register_fn(rpc::PROC_DETAIL, move |v| {
        let p = p.clone();
        async move {
            let q: rpc::PidParams = params(rpc::PROC_DETAIL, v)?;
            result(p.detail(q.pid).await?)
        }
    });

    let p = proc.clone();
    d.register_fn(rpc::PROC_SIGNAL, move |v| {
        let p = p.clone();
        async move {
            let q: rpc::SignalParams = params(rpc::PROC_SIGNAL, v)?;
            p.signal(q.pid, q.signal)?;
            result(())
        }
    });

    d.register_fn(rpc::PROC_RENICE, move |v| {
        let p = proc.clone();
        async move {
            let q: rpc::ReniceParams = params(rpc::PROC_RENICE, v)?;
            p.renice(q.pid, q.nice)?;
            result(())
        }
    });
}

// ---------------------------------------------------------------------------
// service
// ---------------------------------------------------------------------------

fn register_service(d: &mut Dispatcher, provider: Option<Arc<dyn ServiceProvider>>) {
    /// 取 provider 或报能力缺失。
    fn get(p: &Option<Arc<dyn ServiceProvider>>) -> ApiResult<Arc<dyn ServiceProvider>> {
        p.clone().ok_or_else(|| unavailable("systemd", "服务管理器"))
    }

    let p = provider.clone();
    d.register_fn(rpc::SERVICE_LIST, move |v| {
        let p = p.clone();
        async move { result(get(&p)?.list_units(&params(rpc::SERVICE_LIST, v)?).await?) }
    });

    let p = provider.clone();
    d.register_fn(rpc::SERVICE_DETAIL, move |v| {
        let p = p.clone();
        async move {
            let q: rpc::UnitParams = params(rpc::SERVICE_DETAIL, v)?;
            result(get(&p)?.unit_detail(q.scope, &q.unit).await?)
        }
    });

    let p = provider.clone();
    d.register_fn(rpc::SERVICE_FILE, move |v| {
        let p = p.clone();
        async move {
            let q: rpc::UnitParams = params(rpc::SERVICE_FILE, v)?;
            result(get(&p)?.unit_file(q.scope, &q.unit).await?)
        }
    });

    let p = provider.clone();
    d.register_fn(rpc::SERVICE_DEPS, move |v| {
        let p = p.clone();
        async move {
            let q: rpc::UnitParams = params(rpc::SERVICE_DEPS, v)?;
            result(get(&p)?.unit_deps(q.scope, &q.unit).await?)
        }
    });

    d.register_fn(rpc::SERVICE_ACTION, move |v| {
        let p = provider.clone();
        async move {
            let q: rpc::UnitActionParams = params(rpc::SERVICE_ACTION, v)?;
            result(get(&p)?.unit_action(q.scope, &q.unit, q.action).await?)
        }
    });
}

// ---------------------------------------------------------------------------
// log
// ---------------------------------------------------------------------------

fn register_log(d: &mut Dispatcher, provider: Option<Arc<dyn LogProvider>>) {
    fn get(p: &Option<Arc<dyn LogProvider>>) -> ApiResult<Arc<dyn LogProvider>> {
        p.clone().ok_or_else(|| unavailable("journal", "日志后端"))
    }

    let p = provider.clone();
    d.register_fn(rpc::LOG_QUERY, move |v| {
        let p = p.clone();
        async move { result(get(&p)?.query(&params(rpc::LOG_QUERY, v)?).await?) }
    });

    let p = provider.clone();
    d.register_fn(rpc::LOG_ENTRY, move |v| {
        let p = p.clone();
        async move {
            let q: rpc::CursorParams = params(rpc::LOG_ENTRY, v)?;
            result(get(&p)?.entry(&q.cursor).await?)
        }
    });

    let p = provider.clone();
    d.register_fn(rpc::LOG_BOOTS, move |_| {
        let p = p.clone();
        async move { result(get(&p)?.boots().await?) }
    });

    // `log.follow` 是订阅而不是调用：`journalctl -f` 的子进程必须跑在 worker 里，
    // 日志的可见范围才会随登录用户走（journald ACL 裁决的是这个进程的 uid）。
    // 主进程里那份实现看到的是主进程的身份，对多用户是错的。
    d.register_stream(rpc::LOG_FOLLOW, move |v| {
        let p = provider.clone();
        async move {
            let q: LogQuery = optional_params(rpc::LOG_FOLLOW, v)?;
            let follow = get(&p)?.follow(&q).await?;
            // 每一项是一**批** LogEntry：批次已在 provider 内合并，
            // 一帧一条会把 IPC 变成瓶颈。
            Ok(stream::unfold(follow, |mut f| async move {
                let batch = f.next().await?;
                Some((serde_json::to_value(&*batch), f))
            })
            .filter_map(|encoded| async move {
                match encoded {
                    Ok(v) => Some(v),
                    // 单批编码失败不该终止整条 follow：跳过它，下一批照常。
                    Err(e) => {
                        tracing::warn!(error = %e, "log.follow 的一批条目无法序列化，已跳过");
                        None
                    }
                }
            }))
        }
    });
}

// ---------------------------------------------------------------------------
// caps
// ---------------------------------------------------------------------------

fn register_caps(d: &mut Dispatcher) {
    d.register_fn(rpc::CAPS_PROBE_USER, |_| async {
        result(super::probe::probe_user().await)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 分发表覆盖方法表里的每一项() {
        let d = default_dispatcher().await;
        let methods = d.methods();
        // roadmap/01 §4.1 的表；订阅类（PROC_LIVE / LOG_FOLLOW）由 register_stream
        // 单独注册，不在这张表里。
        for m in [
            rpc::HOST_INFO,
            rpc::HOST_HEALTH,
            rpc::HOST_TIME,
            rpc::HOST_SET_HOSTNAME,
            rpc::HOST_SET_TIMEZONE,
            rpc::HOST_POWER,
            rpc::PROC_LIST,
            rpc::PROC_DETAIL,
            rpc::PROC_SIGNAL,
            rpc::PROC_RENICE,
            rpc::SERVICE_LIST,
            rpc::SERVICE_DETAIL,
            rpc::SERVICE_FILE,
            rpc::SERVICE_DEPS,
            rpc::SERVICE_ACTION,
            rpc::LOG_QUERY,
            rpc::LOG_ENTRY,
            rpc::LOG_BOOTS,
            rpc::CAPS_PROBE_USER,
            // `term.open` 交出 fd，注册在另一张表里，但 `methods()` 两张都算——
            // 它对调用方而言就是一个普通方法名。
            rpc::TERM_OPEN,
            rpc::TERM_RESIZE,
            rpc::TERM_CLOSE,
        ] {
            assert!(methods.iter().any(|x| x == m), "方法表缺 {m}；已注册：{methods:?}");
        }
        // 订阅类的频道单独一张表；`log.follow` 不该同时是一个可调用的方法。
        assert!(d.stream_channels().iter().any(|c| c == rpc::LOG_FOLLOW));
        assert!(!methods.iter().any(|m| m == rpc::LOG_FOLLOW));
    }

    #[tokio::test]
    async fn log_follow_的订阅参数可以为空() {
        let d = default_dispatcher().await;
        // 没有日志后端时报 capability_unavailable，有则真的建起 follow——
        // 两种结果都说明 `null` 被当成了缺省的 LogQuery 而不是解析失败。
        match d.open_stream(rpc::LOG_FOLLOW, Value::Null).await {
            Ok(_) => {}
            Err(e) => assert_eq!(
                e.code,
                strixmaid_types::ErrorCode::CapabilityUnavailable,
                "空参数不该被当成参数错误：{e:?}"
            ),
        }
        match d
            .open_stream(rpc::LOG_FOLLOW, serde_json::json!({ "limit": "abc" }))
            .await
        {
            Ok(_) => panic!("limit 不是数字，不该建起流"),
            Err(e) => assert!(matches!(
                e.code,
                strixmaid_types::ErrorCode::Internal
                    | strixmaid_types::ErrorCode::CapabilityUnavailable
            )),
        }
    }

    #[tokio::test]
    async fn 读方法在本进程内就能跑通() {
        let d = default_dispatcher().await;

        let info = d.dispatch(rpc::HOST_INFO, Value::Null).await.unwrap();
        assert!(info["hostname"].as_str().is_some_and(|s| !s.is_empty()));

        let list = d
            .dispatch(rpc::PROC_LIST, serde_json::json!({ "limit": 5 }))
            .await
            .unwrap();
        assert!(list.as_array().is_some_and(|a| !a.is_empty()));

        let probe = d.dispatch(rpc::CAPS_PROBE_USER, Value::Null).await.unwrap();
        // SAFETY: getuid 无副作用。
        assert_eq!(probe["uid"].as_u64(), Some(u64::from(unsafe { libc::getuid() })));
    }

    #[tokio::test]
    async fn 参数不合协议报_internal_而不是_invalid_request() {
        let d = default_dispatcher().await;
        // pid 应是数字，这里给字符串
        let err = d
            .dispatch(rpc::PROC_DETAIL, serde_json::json!({ "pid": "abc" }))
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            strixmaid_types::ErrorCode::Internal,
            "参数不合协议是调用方的 bug，不该报成用户输入错误"
        );
    }

    #[tokio::test]
    async fn 未知方法报_not_found() {
        let d = default_dispatcher().await;
        let err = d.dispatch("no.such.method", Value::Null).await.unwrap_err();
        assert_eq!(err.code, strixmaid_types::ErrorCode::NotFound);
    }
}
