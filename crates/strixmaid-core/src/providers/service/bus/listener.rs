//! 按需信号监听、独立冲刷任务与退役协议。
use super::super::{FlushQueue, ServiceEvent, summary_for_vanished, with_timeout};
use super::proxy::{JobRemovedStream, ManagerProxy, UnitNewStream, UnitRemovedStream};
use super::{Shared, SystemdBus, map_zbus_error, unit_name_from_path};
use futures::StreamExt as _;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use strixmaid_types::service::{UnitScope, UnitSummary};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::mpsc::error::TrySendError;
use zbus::{Connection, MatchRule, MessageStream};
/// 监听任务多久检查一次「还有没有人在看」。
///
/// 这是 2026-09-25 事故的**暴露面收敛**：没有订阅者就退场，不再长期持有
/// 对系统总线的信号订阅。间隔取 30 秒——退场晚一点无害，查得太勤则是白费。
const IDLE_CHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// 起一个监听任务。`ensure_listener` 与监听自己的「退场后发现又有人来了」
/// 都走这里——两处必须是同一套语义，分两份写迟早会走样。
///
/// 标志的所有权：本函数抢到标志（`set_listener_flag` 返回 true）就归它，
/// 一路交到 [`run_listener`] 手里，由后者在退场收尾时归还。中途失败则当场归还。
pub(super) async fn spawn_listener(conn: Connection, scope: UnitScope, shared: Arc<Shared>) {
    if !shared.set_listener_flag(scope, true) {
        return; // 已经有一个在跑
    }
    // setup 里是六连发的总线往返，而本函数被 WS 订阅经 block_in_place
    // 同步等着：不包超时的话，总线一慢每个订阅请求就永久占一个运行时线程。
    match with_timeout("监听建立", ListenerStreams::setup(&conn)).await {
        Ok(streams) => {
            tokio::spawn(run_listener(conn, scope, shared, streams));
        }
        Err(e) => {
            tracing::warn!(?scope, error = %e, "systemd 事件监听启动失败");
            shared.set_listener_flag(scope, false);
        }
    }
}

impl Shared {
    /// 退场协议第一步：**只判断**该不该退场，不动标志。
    ///
    /// 标志要一直攥到退订真正做完为止（见 [`Shared::finish_retire`]）——
    /// 这一条是 2026-09-25 事故复盘里补上的：先清标志再退订的话，窗口里
    /// 来的订阅者会撞上 systemd 的 `AlreadySubscribed`，或者它刚订阅成功就被
    /// 我们这条正在收尾的 `Unsubscribe` 撤销掉，两种都表现为「页面一直没有
    /// 事件」而且不会自愈。
    fn should_retire(&self) -> bool {
        // 与 `ServiceProvider::subscribe` 的顺序配合：那边**先**拿 receiver
        // 再调 `ensure_listener`，所以只要它拿到了 receiver，这里就看得见
        // `receiver_count() > 0`，不会误退场。
        self.events.receiver_count() == 0
    }

    /// 退场协议第二步：退订做完了，归还标志。
    ///
    /// 返回 `true` 表示**退场期间又有人订阅了**——那个订阅者调
    /// `ensure_listener` 时看到标志还被我们占着，于是什么也没做，它现在
    /// 一个事件都收不到。调用方据此替它重开一个监听。
    ///
    /// 检查与归还在同一把锁内完成，否则这个「又有人来了」的判断本身还会有窗口。
    fn finish_retire(&self, scope: UnitScope) -> bool {
        let mut flags = self.listeners.lock().unwrap_or_else(|p| p.into_inner());
        let flag = match scope {
            UnitScope::System => &mut flags.system,
            UnitScope::User => &mut flags.user,
        };
        *flag = false;
        self.events.receiver_count() > 0
    }

    /// 置监听标志；返回值表示**本次调用改变了它**（用于「只启动一次」）。
    fn set_listener_flag(&self, scope: UnitScope, value: bool) -> bool {
        let mut flags = self.listeners.lock().unwrap_or_else(|p| p.into_inner());
        let flag = match scope {
            UnitScope::System => &mut flags.system,
            UnitScope::User => &mut flags.user,
        };
        let changed = *flag != value;
        *flag = value;
        changed
    }
}

/// 四路信号流。**所有权始终交给 [`listen_loop`]，它返回即全部释放**——
/// 这不是风格偏好：这四路流是连接上仅有的有界广播队列，谁在持有它们的
/// 同时 `await` 总线往返，谁就可能复刻 2026-09-25 的环形死锁（流不被
/// poll → 队列满 → zbus 读取任务挂在投递上 → 回复读不出来）。把它们
/// 装进独立结构、以值传给热循环，就是让「退场路径上没有队列」由借用
/// 检查器保证，而不是由注释恳求。
struct SignalStreams {
    unit_new: UnitNewStream,
    unit_removed: UnitRemovedStream,
    job_removed: JobRemovedStream,
    props_changed: MessageStream,
}

/// 监听的全部总线资源：manager proxy（冲刷任务用）+ 四路信号流（热循环用）。
struct ListenerStreams {
    mgr: ManagerProxy<'static>,
    signals: SignalStreams,
}

impl ListenerStreams {
    async fn setup(conn: &Connection) -> ApiResult<Self> {
        let mgr = SystemdBus::manager(conn).await?;
        match mgr.subscribe().await {
            Ok(()) => {}
            // systemd 对同一条连接重复 Subscribe 报 AlreadySubscribed。对我们
            // 这是成功：上一任监听的退订没送达（超时、连接抖动）时订阅本来
            // 就还挂着。当失败处理的话，一次漏掉的退订就会让本频道在进程
            // 余生里再也订不上——每次都撞这个错、每次都放弃。
            Err(zbus::Error::MethodError(ref name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.AlreadySubscribed" => {}
            Err(e) => return Err(map_zbus_error(e, "Subscribe")),
        }

        let unit_new = mgr
            .receive_unit_new()
            .await
            .map_err(|e| map_zbus_error(e, "UnitNew"))?;
        let unit_removed = mgr
            .receive_unit_removed()
            .await
            .map_err(|e| map_zbus_error(e, "UnitRemoved"))?;
        let job_removed = mgr
            .receive_job_removed()
            .await
            .map_err(|e| map_zbus_error(e, "JobRemoved"))?;

        // PropertiesChanged 从每个 unit 对象各自发出，proxy 绑定单个路径接不到，走 match rule。
        // 不设 sender：systemd 之外没有别人会在这个路径命名空间下发信号。
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .and_then(|b| b.member("PropertiesChanged"))
            .and_then(|b| b.path_namespace("/org/freedesktop/systemd1/unit"))
            .map_err(|e| map_zbus_error(e, "PropertiesChanged"))?
            .build();
        let props_changed = MessageStream::for_match_rule(rule, conn, Some(512))
            .await
            .map_err(|e| map_zbus_error(e, "PropertiesChanged"))?;

        Ok(Self {
            mgr,
            signals: SignalStreams {
                unit_new,
                unit_removed,
                job_removed,
                props_changed,
            },
        })
    }
}

/// [`listen_loop`] 的退出方式，决定 [`run_listener`] 的收尾动作。
enum ListenerExit {
    /// 没有订阅者了：退订、归还标志；若退场期间又有人来了则重开。
    Idle,
    /// 连接断了：清标志走人，没有可收尾的。
    Disconnected,
    /// 冲刷任务没了（panic）：订阅与连接都还好，重建一轮即可。
    FlusherGone,
}

/// 监听任务的**生命周期外壳**：每轮 = 起冲刷任务 → 跑热循环 → 收尾。
///
/// 分层是这段代码的全部要点（背景见
/// `docs/incidents/2026-09-25-dbus-wedge.md`）：
///
/// - [`listen_loop`]（热循环）**只**做入队与投递，绝不 `await` 总线往返；
/// - 总线往返集中在 [`spawn_flusher`] 的独立任务里；
/// - 收尾动作（等冲刷收摊、退订、归还标志）全部发生在热循环返回**之后**
///   ——那时四路信号流已随 `listen_loop` 的参数一起释放，连接上没有任何
///   有界队列，这些 `await` 不可能把 zbus 的读取任务挡住。
///
/// 标志（`ListenerFlags`）从 [`spawn_listener`] 抢到手起就归本任务，
/// 直到某条退出路径显式归还。退场（`Idle`）用两步协议：先退订、后归还
/// （[`Shared::finish_retire`]），归还时发现又有人订阅了就地重开——
/// 那个订阅者看到标志被占没敢开新监听，只能由我们替它接上。
async fn run_listener(
    conn: Connection,
    scope: UnitScope,
    shared: Arc<Shared>,
    mut streams: ListenerStreams,
) {
    loop {
        let ListenerStreams { mgr, signals } = streams;
        let (flush_tx, flusher) = spawn_flusher(conn.clone(), scope, Arc::clone(&shared), mgr);
        let exit = listen_loop(&conn, scope, &shared, signals, &flush_tx).await;
        // ← 四路信号流已释放。先断投递口再等冲刷收摊，免得它卡在 recv 上。
        drop(flush_tx);
        let _ = flusher.await;

        match exit {
            ListenerExit::Disconnected => {
                tracing::warn!(?scope, "systemd 事件监听退出（bus 断开）");
                shared.set_listener_flag(scope, false);
                return;
            }
            ListenerExit::FlusherGone => {
                // 不退订：订阅还挂着，setup 会以 AlreadySubscribed 兼容。
                tracing::warn!(?scope, "冲刷任务异常退出，重建监听");
            }
            ListenerExit::Idle => {
                unsubscribe_with_timeout(&conn, scope).await;
                if !shared.finish_retire(scope) {
                    tracing::debug!(?scope, "systemd 事件监听退场（无订阅者）");
                    return;
                }
                // 退场期间来了新订阅者。标志刚被归还，重新抢；抢不到说明
                // 它（或更晚的谁）已经自己开好了新监听，我们功成身退。
                if !shared.set_listener_flag(scope, true) {
                    return;
                }
                tracing::debug!(?scope, "退场途中来了新订阅者，重开监听");
            }
        }

        streams = match with_timeout("监听重建", ListenerStreams::setup(&conn)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?scope, error = %e, "重建监听失败");
                shared.set_listener_flag(scope, false);
                return;
            }
        };
    }
}

/// 事件监听热循环。**循环体内绝不 `await` 总线调用**——这是 2026-09-25
/// 总线死锁的根因修复：等回复期间四路流停止被 poll，zbus 的读取任务会在
/// 满队列的投递上挂住，回复就永远读不出来（环形等待，细节见事故记录）。
/// 冲刷经 `flush_tx` 交给独立任务，本循环只做入队、投递与退场判断。
async fn listen_loop(
    conn: &Connection,
    scope: UnitScope,
    shared: &Shared,
    signals: SignalStreams,
    flush_tx: &tokio::sync::mpsc::Sender<HashSet<String>>,
) -> ListenerExit {
    let SignalStreams {
        mut unit_new,
        mut unit_removed,
        mut job_removed,
        mut props_changed,
    } = signals;
    tracing::debug!(?scope, "systemd 事件监听已启动");

    let mut queue = FlushQueue::default();
    let mut idle = tokio::time::interval(IDLE_CHECK);
    idle.tick().await; // interval 的第一个 tick 立刻就绪，丢掉

    loop {
        // 拷一份给 future 用，避免与下面各分支对 `queue` 的可变借用冲突。
        let flush_deadline = queue.deadline();
        let flush_at = async move {
            match flush_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            Some(sig) = unit_new.next() => {
                if let Ok(a) = sig.args() { queue.mark(a.id().clone()); }
            }
            Some(sig) = unit_removed.next() => {
                if let Ok(a) = sig.args() { queue.mark(a.id().clone()); }
            }
            Some(sig) = job_removed.next() => {
                if let Ok(a) = sig.args() { queue.mark(a.unit().clone()); }
            }
            Some(msg) = props_changed.next() => {
                if let Ok(m) = msg
                    && let Some(p) = m.header().path()
                    && let Some(name) = unit_name_from_path(p.as_str())
                {
                    queue.mark(name);
                }
            }
            _ = flush_at => {
                match flush_tx.try_send(queue.take()) {
                    Ok(()) => {}
                    // 上一批还在飞：放回去，下一个窗口连同新到的一起刷。
                    Err(TrySendError::Full(names)) => queue.requeue(names),
                    Err(TrySendError::Closed(_)) => return ListenerExit::FlusherGone,
                }
            }
            _ = idle.tick() => {
                if conn.is_closed() {
                    return ListenerExit::Disconnected;
                }
                // 没人在看就退场：不再长期持有对系统总线的订阅，暴露面从
                // 「7×24」缩到「有人正看着服务页的那几分钟」。
                if shared.should_retire() {
                    return ListenerExit::Idle;
                }
            }
            else => return ListenerExit::Disconnected,
        }
    }
}

/// 起冲刷任务：拿一批 unit 名 → 总线往返取摘要 → 广播。
///
/// 通道容量 1 + 调用方 `try_send`：同一时刻最多一批在飞，送不进去的名字
/// 放回 [`FlushQueue`] 下个去抖窗口再刷——天然合并，也不会无限 spawn。
fn spawn_flusher(
    conn: Connection,
    scope: UnitScope,
    shared: Arc<Shared>,
    mgr: ManagerProxy<'static>,
) -> (
    tokio::sync::mpsc::Sender<HashSet<String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<HashSet<String>>(1);
    let task = tokio::spawn(async move {
        while let Some(names) = rx.recv().await {
            let units = summaries_for(&conn, &mgr, names, scope).await;
            if !units.is_empty() {
                // 没有订阅者时 send 返回 Err，无所谓。
                let _ = shared.events.send(ServiceEvent { units });
            }
        }
    });
    (tx, task)
}

/// 退订，带超时。
///
/// 失败记 warn 而不是 debug：漏掉的退订曾经意味着本频道永久死亡
/// （AlreadySubscribed 被当失败），现在 setup 已兼容，但它仍是值得在
/// 日志里看得见的异常。超时兜底与其余总线调用同一档（15 秒）。
async fn unsubscribe_with_timeout(conn: &Connection, scope: UnitScope) {
    let done = with_timeout("Unsubscribe", async {
        let mgr = SystemdBus::manager(conn).await?;
        mgr.unsubscribe()
            .await
            .map_err(|e| map_zbus_error(e, "Unsubscribe"))
    })
    .await;
    if let Err(e) = done {
        tracing::warn!(?scope, error = %e, "退订失败（下次 Subscribe 按已订阅兼容）");
    }
}

/// 为一批 unit 名取当前摘要。批量大（daemon-reload 时几百个）就整表拉一次，小批量逐个查。
///
/// **两条路都套 `with_timeout`**：`summary_no_load` 与 `list_units_raw` 曾是
/// 本文件仅有的两个不带超时的总线调用，而它们恰好在 2026-09-25 死锁被无限
/// 等下去的那条路径上。根因已由「冲刷移出监听循环」修掉，这里是纵深——
/// 即便将来某个调用又挂住，挂住的也只是冲刷任务，且 15 秒后自己解开。
///
/// **超时（或整表失败）时丢弃条目而不是伪造结论**：`summary_for_vanished`
/// 序列化成 `load_state = not_found`，前端按频道契约会**删行**
/// （`ws/channels/services_changed.rs`）。一次 15 秒的总线抖动不该在界面上
/// 表现成「这个服务被删了」；漏掉的这次刷新由窗口重获焦点的整表刷新兜底。
async fn summaries_for(
    conn: &Connection,
    mgr: &ManagerProxy<'_>,
    names: HashSet<String>,
    scope: UnitScope,
) -> Vec<UnitSummary> {
    const PER_UNIT_LIMIT: usize = 16;
    if names.len() > PER_UNIT_LIMIT {
        let all = with_timeout(
            "事件刷新 ListUnits",
            SystemdBus::list_units_raw(conn, scope),
        )
        .await;
        return match all {
            Ok(all) => {
                let mut by_name: HashMap<String, UnitSummary> =
                    all.into_iter().map(|u| (u.name.clone(), u)).collect();
                names
                    .into_iter()
                    .map(|n| {
                        // 整表是刚拉的权威快照：不在表里 = 真的没了，
                        // 这里的 vanished 是事实而不是猜测。
                        by_name
                            .remove(&n)
                            .unwrap_or_else(|| summary_for_vanished(&n, scope))
                    })
                    .collect()
            }
            Err(e) => {
                tracing::warn!(error = %e, dropped = names.len(), "事件刷新时 ListUnits 失败，这批更新已丢弃");
                Vec::new()
            }
        };
    }
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        match with_timeout("事件刷新 unit 摘要", async {
            Ok::<_, ApiError>(SystemdBus::summary_no_load(conn, mgr, &name, scope).await)
        })
        .await
        {
            Ok(s) => out.push(s),
            // 超时 ≠ 消失：跳过这一条，见函数文档。
            Err(e) => {
                tracing::warn!(unit = %name, error = %e, "事件刷新时取 unit 摘要超时，已跳过")
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::service::{EVENT_CAPACITY, cgroup::CgroupReader};
    use std::sync::Mutex;

    fn shared() -> Shared {
        Shared {
            cgroup: CgroupReader::new(),
            events: tokio::sync::broadcast::channel(EVENT_CAPACITY).0,
            listeners: Mutex::new(Default::default()),
        }
    }

    #[test]
    fn retiring_listener_keeps_flag_until_unsubscribe_finishes() {
        for scope in [UnitScope::System, UnitScope::User] {
            let shared = shared();
            assert!(shared.set_listener_flag(scope, true));
            assert!(shared.should_retire());
            // A subscriber arrives while Unsubscribe is in flight. It must not
            // start a second listener that the old Unsubscribe could revoke.
            let rx = shared.events.subscribe();
            assert!(!shared.set_listener_flag(scope, true));
            assert!(!shared.should_retire());
            assert!(shared.finish_retire(scope), "the old listener must restart");
            assert!(shared.set_listener_flag(scope, true));
            assert!(!shared.set_listener_flag(scope, true));
            drop(rx);
            assert!(shared.should_retire());
            assert!(!shared.finish_retire(scope));
            assert!(
                shared.set_listener_flag(scope, true),
                "next subscriber can start"
            );
        }
    }

    #[tokio::test]
    async fn full_flush_channel_merges_pending_names_without_waiting() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(HashSet::from(["first.service".to_owned()]))
            .unwrap();
        let mut queue = FlushQueue::default();
        queue.mark("pending.service".to_owned());
        let Err(TrySendError::Full(names)) = tx.try_send(queue.take()) else {
            panic!("the bounded channel must reject a second pending batch");
        };
        queue.requeue(names);
        queue.mark("pending.service".to_owned());
        queue.mark("new.service".to_owned());
        assert_eq!(rx.recv().await.unwrap().len(), 1);
        tx.try_send(queue.take()).unwrap();
        assert_eq!(
            rx.recv().await.unwrap(),
            HashSet::from(["pending.service".to_owned(), "new.service".to_owned()])
        );
    }
}
