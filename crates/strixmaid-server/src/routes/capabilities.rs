//! `GET /api/v1/capabilities` —— 两层能力（`docs/design.md` §6）。
//!
//! - **system 层**：启动时由 `CapabilityRegistry::probe_all` 探测一次，放进
//!   [`CapabilityState`]，之后不变。
//! - **user 层**：先由 `UserIdentity` 按组**推导**，再用 user worker 里的**实测**
//!   结果覆盖（`roadmap/01-worker-execution.md` §4.6）。
//!
//! **未认证时 `user` 为 `null`，接口仍返回 200**——登录页要靠 system 层判断
//! helper 是否可用，若这里因为没登录就 401，用户只会对着一个神秘失败的登录框
//! 反复重试（design.md §6）。
//!
//! # 为什么要实测，不能只靠推导
//!
//! 按组推导快且离线，但只是猜：「在 `adm` 组」不等于「真读得到系统日志」——
//! ACL 可能被改过，日志后端可能压根没装。实测在 user worker 内真的去试一次，
//! 因此测的是**该用户**的可见范围。两者合并时实测值覆盖推导值；实测某项
//! 没测出结论（`None`）时沿用推导值，而不是当成 `false`。
//!
//! # 为什么要缓存
//!
//! 实测要起子进程（`journalctl` / `log show`），比推导贵得多，而这个端点会被
//! 前端频繁轮询。按会话缓存 60 秒：能力不是每秒都在变的东西，60 秒的陈旧度
//! 换掉每次请求一个子进程，划算。提权会改变结果，因此缓存键里带上提权状态。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Extension, State};
use strixmaid_core::capability::UserIdentity;
use strixmaid_core::session::Session;
use strixmaid_types::capability::{Capabilities, SystemCapabilities, UserCapabilities, UserProbe};
use strixmaid_types::rpc;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::exec::{self, Privilege};
use crate::auth::AuthState;

/// user 层实测结果的缓存时长。
const PROBE_TTL: Duration = Duration::from_secs(60);

/// 能力路由的共享状态。
///
/// `elevate_groups` 必须与 `session.elevate_groups`（进而与 helper 的权威判断）
/// **同源**。这里报的 `can_elevate` 决定前端显不显示「启用管理访问」，
/// 若两边取值不同，用户就会看到一个点了必被拒的按钮。
pub struct CapabilityState {
    pub system: SystemCapabilities,
    pub elevate_groups: Vec<String>,
    /// 会话身份 → (实测时刻, 结果)。见模块文档「为什么要缓存」。
    probes: Mutex<HashMap<ProbeKey, (Instant, UserProbe)>>,
    /// 发起实测要走 worker，因此需要 `AuthState`。
    auth: Arc<AuthState>,
}

/// 实测缓存的键：会话 + 提权状态。
///
/// 带上 `elevated` 是因为提权会改变结果（admin worker 起来之后
/// `can_manage_units` 从「测不出」变成真），共用一条缓存会让刚提完权的用户
/// 在 60 秒内看到旧能力。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProbeKey {
    token_hash: String,
    elevated: bool,
}

impl std::fmt::Debug for CapabilityState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityState")
            .field("system", &self.system)
            .field("elevate_groups", &self.elevate_groups)
            .finish_non_exhaustive()
    }
}

impl CapabilityState {
    pub fn new(
        system: SystemCapabilities,
        elevate_groups: Vec<String>,
        auth: Arc<AuthState>,
    ) -> Self {
        Self {
            system,
            elevate_groups,
            probes: Mutex::new(HashMap::new()),
            auth,
        }
    }

    /// 取该会话的实测结果，命中缓存就直接用。
    ///
    /// 实测失败不算错误：返回 `None`，调用方沿用推导值。能力探测本身失败
    /// 而让 `/capabilities` 整个 500，是最不该发生的事——前端正是靠它决定
    /// 显示什么，它挂了整个界面就没了依据。
    async fn probe(&self, session: &Session) -> Option<UserProbe> {
        let key = ProbeKey {
            token_hash: session.token_hash.clone(),
            elevated: session.elevated,
        };

        if let Some((at, probe)) = self
            .probes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            && at.elapsed() < PROBE_TTL
        {
            return Some(probe.clone());
        }

        let probed: UserProbe = match exec::call(
            &self.auth,
            session,
            Privilege::User,
            rpc::CAPS_PROBE_USER,
            (),
        )
        .await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e.message, "user 层实测失败，沿用按组推导的结果");
                return None;
            }
        };

        let mut cache = self.probes.lock().unwrap_or_else(|e| e.into_inner());
        // 顺手清掉过期项：这张表随会话增长，没有别的地方回收它。
        cache.retain(|_, (at, _)| at.elapsed() < PROBE_TTL);
        cache.insert(key, (Instant::now(), probed.clone()));
        Some(probed)
    }
}

/// 把实测结果合并进推导结果。**实测值覆盖推导值；`None` 表示没测出结论，沿用推导。**
///
/// `can_read_journal` 有一处例外：实测在 **user worker** 里做（`worker/probe.rs`——
/// 必须如此，`user_units` 与 `uid` 只有在那里测才有意义），而日志读取走
/// `auth::exec::escalate`，**已提权的会话被 journald 拒绝后会换 admin worker 重试**。
/// 所以 user worker 里测出的「读不到」对已提权的会话不成立，不能拿它盖掉提权带来的
/// 可见性——否则前端会给一个已经是管理员的人灰掉日志页，而他点进去其实是能看的。
pub fn merge(mut derived: UserCapabilities, probe: &UserProbe) -> UserCapabilities {
    if let Some(v) = probe.can_read_journal {
        derived.can_read_journal = v || derived.elevated;
    }
    if let Some(v) = probe.can_manage_units {
        derived.can_manage_units = v;
    }
    derived
}

/// 构建 `/capabilities` 路由（相对 `/api/v1`）。
pub fn router(state: Arc<CapabilityState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(capabilities))
        .with_state(state)
}

/// 能力探测
///
/// `system`：这台机器有没有（启动时探测一次）；`user`：当前登录用户能不能用
/// （按组推导 + user worker 内实测，实测结果按会话缓存 60 秒）。
/// **未认证时 `user` 为 `null`，仍返回 200。**
#[utoipa::path(
    get,
    path = "/capabilities",
    tag = "capabilities",
    responses(
        (status = 200, description = "两层能力", body = Capabilities),
    ),
)]
pub async fn capabilities(
    State(state): State<Arc<CapabilityState>>,
    user: Option<Extension<UserIdentity>>,
    session: Option<Extension<Session>>,
) -> Json<Capabilities> {
    let user = match user {
        None => None,
        Some(Extension(identity)) => {
            let derived = identity.capabilities(&state.elevate_groups);
            // 有会话才谈得上实测——它要在那个会话的 worker 里跑。
            match session {
                Some(Extension(s)) => match state.probe(&s).await {
                    Some(probe) => Some(merge(derived, &probe)),
                    None => Some(derived),
                },
                None => Some(derived),
            }
        }
    };

    Json(Capabilities {
        system: state.system,
        user,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derived() -> UserCapabilities {
        UserCapabilities {
            uid: 1000,
            name: "alice".into(),
            groups: vec!["alice".into(), "adm".into()],
            can_read_journal: true,
            can_manage_units: false,
            can_elevate: true,
            elevated: false,
        }
    }

    #[test]
    fn 实测值覆盖推导值() {
        // 推导说「在 adm 组，应该能读日志」，实测说读不到——以实测为准。
        let probe = UserProbe {
            can_read_journal: Some(false),
            can_manage_units: None,
            user_units: None,
            uid: 1000,
        };
        let merged = merge(derived(), &probe);
        assert!(!merged.can_read_journal, "实测结果必须覆盖推导");
    }

    /// 提权是这条覆盖规则的唯一例外：实测在 user worker 里做，而已提权的会话
    /// 读日志会被 `auth::exec::escalate` 换到 admin worker 重试，所以那个「读不到」
    /// 不适用。roadmap/07 在 Rocky 9 上撞到的正是这一幕：bob 提权后 elevated=true，
    /// 日志页却被灰掉。
    #[test]
    fn 已提权时实测的读不到不应盖掉提权带来的可见性() {
        let mut d = derived();
        d.elevated = true;
        d.can_read_journal = true; // 推导：elevated ⇒ 可读
        let probe = UserProbe {
            // user worker 里 bob 确实读不到内核日志——但那不是提权后的实际处境
            can_read_journal: Some(false),
            can_manage_units: None,
            user_units: None,
            uid: 1000,
        };
        assert!(
            merge(d, &probe).can_read_journal,
            "已提权的会话不该因 user worker 的实测而被判定读不到日志"
        );
    }

    /// 未提权时不受影响：实测说读不到就是读不到，不能借这条例外放宽。
    #[test]
    fn 未提权时实测的读不到仍然生效() {
        let probe = UserProbe {
            can_read_journal: Some(false),
            can_manage_units: None,
            user_units: None,
            uid: 1000,
        };
        let merged = merge(derived(), &probe);
        assert!(!merged.elevated);
        assert!(!merged.can_read_journal);
    }

    #[test]
    fn 没测出结论时沿用推导值() {
        // 全 None：合并后应与推导完全一致，而不是被清成 false
        let probe = UserProbe {
            can_read_journal: None,
            can_manage_units: None,
            user_units: None,
            uid: 1000,
        };
        assert_eq!(merge(derived(), &probe), derived());
    }

    #[test]
    fn 推导为假而实测为真时也要覆盖() {
        let probe = UserProbe {
            can_read_journal: None,
            can_manage_units: Some(true),
            user_units: None,
            uid: 0,
        };
        let merged = merge(derived(), &probe);
        assert!(merged.can_manage_units);
    }

    #[test]
    fn 缓存键区分提权状态() {
        // 提权会改变实测结果，两种状态不能共用一条缓存
        let a = ProbeKey {
            token_hash: "x".into(),
            elevated: false,
        };
        let b = ProbeKey {
            token_hash: "x".into(),
            elevated: true,
        };
        assert_ne!(a, b);
    }
}
