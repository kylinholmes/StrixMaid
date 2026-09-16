//! 进程图标：按**程序名**取一张 PNG，带缓存、负缓存、并发合并与后台预热。
//!
//! 本文件是**与平台无关**的那一半：缓存策略、名字消毒、single-flight、预热与
//! 补热的调度、容量上限。平台那一半只有两个函数，见下。
//!
//! | 模块 | 数据源 | 状态 |
//! |---|---|---|
//! | [`windows`] | exe 的 PE 资源里的图标（`PrivateExtractIconsW` + GDI） | 已实现 |
//! | [`macos`] | `NSWorkspace` 的 `iconForFile:` | 待实现，[`available`] 恒为 false |
//! | [`linux`] | `.desktop` 条目 + freedesktop 图标主题 | 待实现，[`available`] 恒为 false |
//!
//! # 为什么 key 是程序名，不是 pid，也不是 exe 路径
//!
//! 三者里名字最稳：pid 每次重启都变，exe 路径在程序升级（尤其是带版本号目录的
//! 那类，`app\1.2.3\app.exe`）之后也会变，两者都会让 URL 与浏览器缓存频繁失效。
//!
//! 代价是**同名不同程序共用一张图标**——机器上三个来路不同的 `python.exe`
//! 只会有一张图。这是已经接受的取舍，不是缺陷。
//!
//! # 平台侧只有两个函数
//!
//! ```ignore
//! fn icon_png(name: &str, exe: Option<&Path>) -> Option<Vec<u8>>;
//! fn available() -> bool;
//! ```
//!
//! 第一个函数**两个参数都要**，这不是冗余：
//!
//! - Windows 是「exe → PE 资源里的图标」，名字只用来反查出 exe；
//! - Linux 完全不是这条路——它要拿**名字**去 `.desktop` 条目里匹配，再按图标
//!   主题规范查图，图标根本不在可执行文件里。
//!
//! 只传 `exe` 会让 Linux 那条路实现不了；只传名字又会让已经知道路径的调用方
//! 白白再反查一次。两个都给，各平台用自己需要的那个。缓存这一层按名字进，
//! 因此传 `exe = None`；参数留给将来「已经握着路径」的调用方（例如进程详情）。
//!
//! # [`available`] 是 headless 环境的总闸
//!
//! 为 false 时**完全不起预热与补热任务**，端点直接 404，一次缓存都不碰。
//! 理由是 Linux 服务器的常态：既没有 `.desktop` 文件也没有安装图标主题，
//! 那种机器上预热是纯粹的浪费——每 2 分半钟遍历一遍进程表、对几十个名字
//! 各做一次注定失败的查找。有了这道闸，后来人实现 Linux 侧时只要把
//! `available()` 写对，整套调度会自己让开。
//!
//! # 缓存
//!
//! | 维度 | 取值 | 理由 |
//! |---|---|---|
//! | TTL | [`TTL`]（5 分钟） | 程序升级后图标能换掉；再久就要人工重启才更新 |
//! | 失败 | 同样缓存 [`TTL`]（**负缓存**） | 系统进程（`services.exe`、`csrss.exe`）非管理员下取不到 exe 路径，是常态不是异常；不缓存失败的话每次列表刷新都会重新捶一遍 GDI |
//! | 容量 | [`CAPACITY`] 条 | 名字来自进程表，本来就有限；上限防的是被人用乱造的名字灌爆 |
//! | 并发 | single-flight | 见下 |
//!
//! ## single-flight：同名的并发请求只提取一次
//!
//! 前端表格渲染时会同时发几十个图标请求，其中同名的往往有好几个（五个标签页
//! 就是五个 `chrome.exe`）。没有合并的话，同一个名字会并发触发多次 GDI 提取。
//!
//! 做法是在缓存表里放一个「正在提取」的占位（[`Slot::Loading`]），里面是一个
//! `tokio::sync::watch::Sender`。后到的请求在**同一把锁内**取走它的
//! `Receiver` 就放开锁去等；提取方完成时把占位换成结果，`Sender` 随之析构，
//! 所有等待者的 `changed()` 一起返回，再回头读一次缓存拿到结果。
//!
//! 选 `watch` 而不是 `Notify`，是因为 `Notify::notify_waiters()` 只叫醒**此刻
//! 已登记**的等待者：提取方跑得快一点，先 notify 再有人 await，那个人就永远
//! 醒不过来了。`watch` 的唤醒带版本号，先后顺序不影响结果。
//!
//! 提取方的 future 被丢弃（客户端断开、请求超时）时，[`LoadGuard`] 会把占位
//! 撤掉，等待者醒来后发现没有结果，其中一个接手成为新的提取方——不会有人
//! 卡在一个再也不会完成的占位上。
//!
//! # 预热与补热
//!
//! [`IconCache::refresh`] 被两个时机调用，**走的是同一套缓存与 single-flight**，
//! 没有第二条路径：
//!
//! 1. **启动后立刻一轮**（预热）。在后台任务里跑，不阻塞服务开始接受请求。
//! 2. **此后每 [`REFRESH_INTERVAL`] 一轮**（补热）。
//!
//! ## 为什么不能只预热一次
//!
//! TTL 是 5 分钟。只在启动时热一遍的话，5 分钟之后条目全部过期，缓存重新变冷，
//! 预热就只在头 5 分钟有意义。补热的间隔取 **TTL 的一半**（[`REFRESH_INTERVAL`]）
//! 正是为了解决这件事：每一轮只重取「年龄已经超过 `TTL − REFRESH_INTERVAL`」
//! 的条目，也就是「撑不到下一轮」的那些。于是每个条目在过期前必被刷新一次，
//! 稳态下缓存**永远不会因为过期而落空**，同时 5 分钟的新鲜度语义原样保留
//! （程序升级后最迟 5 分钟换掉图标）。
//!
//! 间隔再短只是白做功（条目还新鲜，一轮下来全被跳过），再长就会出现「过期了
//! 但还没轮到补热」的空窗。
//!
//! ## 并发上限
//!
//! 一轮补热同时最多 [`WARM_CONCURRENCY`] 个提取。这是后台任务，不赶时间，
//! 不该在服务刚起来时跟正常请求抢 GDI 与阻塞线程池。
//!
//! ## 失败的名字不在后台反复重试
//!
//! 取不到 exe 路径的那批（受保护进程、其它用户的进程）每一轮都会同样失败，
//! 反复试是纯浪费。补热因此**跳过**上一轮结果为失败的名字；它们的负缓存到期
//! 后由真实请求再试一次，仍失败就再压住 5 分钟。
//!
//! # [`IconCache`] 本身不认识「进程名」
//!
//! 缓存这一层只见到「一个字符串 key」与「一个把 key 变成 PNG 的函数」，
//! key 的**语义由 extractor 决定**。进程这一侧传的是映像名（见上），
//! 服务那一侧（[`crate::providers::service::icon`]）传的是**解析出来的二进制
//! 路径**——那边二百多个服务都由同一个 `svchost.exe` 托管，按服务名做 key
//! 会把同一条结论存两百多份，按路径做 key 则天然合并成一条。
//!
//! 两种语义不冲突，也没有第二份实现：服务侧用的就是本文件的
//! [`IconCache`]，只是经 [`IconCache::with_extractor`] 换了一个 extractor。
//! 缓存策略（TTL、负缓存、容量、single-flight）两边完全一致，改一处两边同时生效。

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;
#[cfg(windows)]
use windows as sys;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::watch;

/// 输出图标的边长，像素。
///
/// 这是**跨平台的输出契约**，不是某个平台的实现细节：前端按固定尺寸渲染，
/// 各平台给出的尺寸不一致会让表格看起来参差。Windows 直接向
/// `PrivateExtractIconsW` 要这个尺寸；Linux 的主题图标尺寸不固定
/// （`16`/`22`/`24`/`32`/`48`/`scalable` 都有），实现时需要缩放到这里。
pub const ICON_SIZE: u32 = 32;

/// 缓存条目的存活时长，成功与失败同此值。
pub const TTL: Duration = Duration::from_secs(300);

/// 补热的间隔，取 [`TTL`] 的一半。关系与理由见模块文档「为什么不能只预热一次」。
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(TTL.as_secs() / 2);

/// 缓存条目数上限。
pub const CAPACITY: usize = 512;

/// 一轮预热 / 补热里同时进行的提取数。
const WARM_CONCURRENCY: usize = 2;

/// 名字的长度上限。Windows 的映像名不会超过 `MAX_PATH` 的一个分量，
/// 128 已经宽松得多；这个上限是用来挡「拿超长串来试探」的。
const MAX_NAME_LEN: usize = 128;

/// 单次取图标最多绕几圈。正常路径最多两圈（等一个提取方 → 读到结果）；
/// 超出说明反复被取消，此时不再进缓存，直接提取一次了事。
const MAX_ATTEMPTS: usize = 4;

/// 本平台 / 本运行环境能不能取进程图标。见模块文档「[`available`] 是 headless
/// 环境的总闸」。
pub fn available() -> bool {
    sys::available()
}

// ===========================================================================
// 名字消毒
// ===========================================================================

/// 校验端点收到的程序名。
///
/// # 这道闸现在是多余的，但仍然要有
///
/// 名字最终是拿去**和当前进程表逐个比对**的（见
/// [`super::windows::image_path_by_name`]），内核给出的映像名从不含路径，
/// 所以带 `..` 或分隔符的名字本来就匹配不到任何东西，路径注入无从谈起。
///
/// 留着它是为了**日后**：万一有人把解析方式改成「按名字去某个目录里找文件」，
/// 这道闸还在原地。安全性质不该依赖「当前实现恰好不受影响」。
///
/// 拒绝的形态：空串、超过 [`MAX_NAME_LEN`]、含路径分隔符（`/` `\`）、
/// 含 `..`、含盘符与备用数据流用的 `:`、含 Windows 文件名非法字符
/// （`* ? " < > |`）、含控制字符（含 NUL）。
pub fn validate_name(name: &str) -> ApiResult<()> {
    if name.is_empty() {
        return Err(ApiError::invalid_request("程序名不能为空"));
    }
    if name.len() > MAX_NAME_LEN {
        return Err(ApiError::invalid_request(format!(
            "程序名过长（{} 字节，上限 {MAX_NAME_LEN}）",
            name.len()
        )));
    }
    if name.contains("..") {
        return Err(ApiError::invalid_request("程序名不能含 `..`"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control())
    {
        return Err(ApiError::invalid_request(format!(
            "程序名含非法字符：{bad:?}"
        )));
    }
    Ok(())
}

// ===========================================================================
// 缓存
// ===========================================================================

/// 缓存里的一格。
enum Slot {
    /// 已有结论。`png` 为 `None` 表示**负缓存**：这个名字取不到图标。
    Ready {
        at: Instant,
        png: Option<Arc<Vec<u8>>>,
    },
    /// 有人正在提取。后到者取走 `Sender` 的 `Receiver` 去等；
    /// 提取方把这一格换成 [`Slot::Ready`] 时 `Sender` 随之析构，等待者一起醒。
    Loading(watch::Sender<()>),
}

/// 取图标的实现。用函数指针而不是 `Box<dyn Fn>`：它是 `Copy` 且
/// `'static`，可以原样搬进 `spawn_blocking`，测试里换成桩也只是换一个值。
pub type Extractor = fn(&str) -> Option<Vec<u8>>;

/// key → PNG 的缓存。key 的语义由 [`Extractor`] 决定，见模块文档
/// 「[`IconCache`] 本身不认识「进程名」」。
///
/// 进程这一侧的实例挂在 [`super::super::process::ProcProvider`] 上，
/// 服务那一侧挂在 [`crate::providers::service::icon::ServiceIcons`] 上，
/// 两者生命周期各随其宿主。
pub struct IconCache {
    slots: Mutex<HashMap<String, Slot>>,
    extract: Extractor,
}

impl Default for IconCache {
    fn default() -> Self {
        Self::new()
    }
}

impl IconCache {
    /// 用本平台的**进程**图标实现建一个空缓存，key 是映像名。
    pub fn new() -> Self {
        Self::with_extractor(|name| sys::icon_png(name, None))
    }

    /// 用给定的 extractor 建一个空缓存，key 的语义随之而定。
    ///
    /// 服务图标（[`crate::providers::service::icon`]）与测试里的桩都走这个入口：
    /// 缓存策略是共用的，变的只有「一个 key 怎么变成 PNG」。
    pub fn with_extractor(extract: Extractor) -> Self {
        IconCache {
            slots: Mutex::new(HashMap::new()),
            extract,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Slot>> {
        // 持锁期间只做 HashMap 的读写，不调用外部代码，因此中毒锁里的数据
        // 一定是完整的，直接接着用。
        self.slots.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 取 `name` 的图标。命中缓存（含负缓存）就直接返回，否则提取一次。
    ///
    /// 同名的并发调用只会有一个真的去提取，其余等它，见模块文档。
    pub async fn get(&self, name: &str) -> Option<Arc<Vec<u8>>> {
        for _ in 0..MAX_ATTEMPTS {
            let step = {
                let mut slots = self.lock();
                let decided = match slots.get(name) {
                    Some(Slot::Ready { at, png }) if at.elapsed() < TTL => {
                        Some(Step::Hit(png.clone()))
                    }
                    Some(Slot::Loading(tx)) => Some(Step::Wait(tx.subscribe())),
                    // 没有条目，或者条目已过期：由本次调用接手提取。
                    _ => None,
                };
                match decided {
                    Some(step) => step,
                    None => {
                        let (tx, _rx) = watch::channel(());
                        slots.insert(name.to_owned(), Slot::Loading(tx));
                        Step::Load
                    }
                }
            };
            match step {
                Step::Hit(png) => return png,
                Step::Load => return self.load(name).await,
                // 提取方完成或被取消，两种都是「回头再读一次缓存」。
                Step::Wait(mut rx) => {
                    let _ = rx.changed().await;
                }
            }
        }
        // 反复被别人抢走又取消。不再参与缓存，自己提取一次就走，
        // 免得在这里转圈。
        self.extract_once(name).await.map(Arc::new)
    }

    /// 真的去提取一次，并把结果（成功或失败）写进缓存。
    async fn load(&self, name: &str) -> Option<Arc<Vec<u8>>> {
        // 提取期间本次调用被丢弃时，由它把「正在提取」的占位撤掉。
        let mut guard = LoadGuard {
            cache: self,
            key: name,
            published: false,
        };
        let png = self.extract_once(name).await.map(Arc::new);
        {
            let mut slots = self.lock();
            slots.insert(
                name.to_owned(),
                Slot::Ready {
                    at: Instant::now(),
                    png: png.clone(),
                },
            );
            evict(&mut slots);
        }
        guard.published = true;
        png
    }

    /// 一次不经缓存的提取。GDI / 图形栈调用是同步且以毫秒计的，
    /// 一律进 `spawn_blocking`，不占 async 运行时线程。
    async fn extract_once(&self, name: &str) -> Option<Vec<u8>> {
        let extract = self.extract;
        let owned = name.to_owned();
        match tokio::task::spawn_blocking(move || extract(&owned)).await {
            Ok(png) => png,
            Err(e) => {
                tracing::warn!(name, error = %e, "图标提取任务异常终止");
                None
            }
        }
    }

    /// 预热 / 补热一批名字。
    ///
    /// 只重取「撑不到下一轮」的条目，跳过仍然新鲜的、正在提取的，以及上一轮
    /// 失败的（见模块文档）。返回这一轮的统计，供调用方记一条日志。
    pub async fn refresh(&self, names: Vec<String>) -> WarmReport {
        let total = names.len();
        let mut due = Vec::new();
        {
            let mut slots = self.lock();
            for name in names {
                let stale = match slots.get(&name) {
                    // 有人正在提取，让开。
                    Some(Slot::Loading(_)) => false,
                    // 负缓存：后台不反复重试。
                    Some(Slot::Ready { png: None, .. }) => false,
                    Some(Slot::Ready { at, .. }) => at.elapsed() >= TTL - REFRESH_INTERVAL,
                    None => true,
                };
                if stale {
                    // 先作废再走 `get`，否则 `get` 会命中这个还没过期的条目、
                    // 什么也不做。作废与判定在同一把锁里完成，不会和别的提取方打架。
                    slots.remove(&name);
                    due.push(name);
                }
            }
        }

        let attempted = due.len();
        let succeeded = stream::iter(due)
            .map(|name| async move { self.get(&name).await.is_some() })
            .buffer_unordered(WARM_CONCURRENCY)
            .filter(|ok| std::future::ready(*ok))
            .count()
            .await;
        WarmReport {
            total,
            attempted,
            succeeded,
            failed: attempted - succeeded,
        }
    }

    /// 当前缓存条目数。给测试与日志用。
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 一轮预热 / 补热的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WarmReport {
    /// 进程表里去重后的名字总数。
    pub total: usize,
    /// 其中真的重取了的（其余是仍然新鲜或上一轮失败的）。
    pub attempted: usize,
    /// 取到图标的。
    pub succeeded: usize,
    /// 没取到的（负缓存）。
    pub failed: usize,
}

/// [`IconCache::get`] 在一次绕圈里决定做什么。
///
/// 拆成一个值再在锁外执行：判定要在锁里做（否则两个调用会同时成为提取方），
/// 而等待与提取都不能持锁。
enum Step {
    Hit(Option<Arc<Vec<u8>>>),
    Wait(watch::Receiver<()>),
    Load,
}

/// 提取期间的守卫：正常完成时什么也不做，被取消（future 被丢弃）时把
/// 「正在提取」的占位撤掉，让等待者里的某一个接手。
struct LoadGuard<'a> {
    cache: &'a IconCache,
    key: &'a str,
    published: bool,
}

impl Drop for LoadGuard<'_> {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let mut slots = self.cache.lock();
        // 只撤自己那一格：占位同一时刻只可能有一个持有者，能看到 `Loading`
        // 就说明还是本次调用放进去的那一个。
        if matches!(slots.get(self.key), Some(Slot::Loading(_))) {
            slots.remove(self.key);
        }
    }
}

/// 超出容量时清理：先丢掉全部过期条目，仍然超出就按写入时刻从老到新丢。
///
/// 不用 LRU：条目数本来就受进程表规模约束（几十条），上限只是防灌爆的护栏，
/// 为它引入一条侵入式链表不划算。
fn evict(slots: &mut HashMap<String, Slot>) {
    if slots.len() <= CAPACITY {
        return;
    }
    slots.retain(|_, slot| match slot {
        Slot::Ready { at, .. } => at.elapsed() < TTL,
        // 正在提取的不能丢：丢了它的等待者就再也等不到唤醒。
        Slot::Loading(_) => true,
    });
    while slots.len() > CAPACITY {
        let oldest = slots
            .iter()
            .filter_map(|(k, slot)| match slot {
                Slot::Ready { at, .. } => Some((k.clone(), *at)),
                Slot::Loading(_) => None,
            })
            .min_by_key(|(_, at)| *at)
            .map(|(k, _)| k);
        match oldest {
            Some(k) => {
                slots.remove(&k);
            }
            // 剩下的全是「正在提取」，丢不得，就让它暂时超一点。
            None => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 桩的本体：名字以 `no-icon` 开头就当取不到，否则回名字本身的字节。
    fn 常规(name: &str) -> Option<Vec<u8>> {
        if name.starts_with("no-icon") {
            None
        } else {
            Some(name.as_bytes().to_vec())
        }
    }

    /// 慢桩的本体：用来观察 single-flight。
    fn 慢(name: &str) -> Option<Vec<u8>> {
        std::thread::sleep(Duration::from_millis(120));
        Some(name.as_bytes().to_vec())
    }

    /// 生成一个「带自己的调用计数器」的桩。
    ///
    /// 每个用例必须有**独立**的计数器：测试在同一个进程里并行跑，共用一个
    /// 静态计数会让断言读到别的用例的次数（现象是次数偏大且随机）。
    macro_rules! 计数桩 {
        ($桩:ident, $计数:ident, $本体:path) => {
            static $计数: AtomicUsize = AtomicUsize::new(0);
            fn $桩(name: &str) -> Option<Vec<u8>> {
                $计数.fetch_add(1, Ordering::SeqCst);
                $本体(name)
            }
        };
    }

    #[test]
    fn 名字消毒() {
        assert!(validate_name("chrome.exe").is_ok());
        assert!(validate_name("Idle").is_ok());
        assert!(validate_name("中文程序.exe").is_ok());

        for bad in [
            "",
            "../../windows/system32/calc.exe",
            "..",
            "a..b",
            "C:\\Windows\\explorer.exe",
            "sub/dir.exe",
            "sub\\dir.exe",
            "stream.exe:zone",
            "star*.exe",
            "q?.exe",
            "quote\".exe",
            "lt<.exe",
            "gt>.exe",
            "pipe|.exe",
            "nul\0.exe",
            "tab\t.exe",
        ] {
            assert!(
                validate_name(bad).is_err(),
                "{bad:?} 应当被拒绝"
            );
        }
        let 超长 = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&超长).is_err(), "超长名字应当被拒绝");
        assert!(validate_name(&"a".repeat(MAX_NAME_LEN)).is_ok(), "刚好到上限应当放行");
    }

    计数桩!(桩1, 计数1, 常规);
    #[tokio::test]
    async fn 命中缓存不再提取() {
        let c = IconCache::with_extractor(桩1);
        assert!(c.get("a.exe").await.is_some());
        assert!(c.get("a.exe").await.is_some());
        assert!(c.get("a.exe").await.is_some());
        assert_eq!(计数1.load(Ordering::SeqCst), 1, "同一个名字只该提取一次");
        assert_eq!(c.len(), 1);
    }

    计数桩!(桩2, 计数2, 常规);
    #[tokio::test]
    async fn 负缓存也命中() {
        let c = IconCache::with_extractor(桩2);
        assert!(c.get("no-icon.exe").await.is_none());
        assert!(c.get("no-icon.exe").await.is_none());
        assert_eq!(
            计数2.load(Ordering::SeqCst),
            1,
            "失败也要缓存，否则每次列表刷新都会重新捶一遍 GDI"
        );
        assert_eq!(c.len(), 1, "失败同样占一格");
    }

    计数桩!(桩3, 计数3, 常规);
    #[tokio::test]
    async fn 过期后重新提取() {
        let c = IconCache::with_extractor(桩3);
        assert!(c.get("a.exe").await.is_some());
        // 直接把写入时刻推回到 TTL 之前，等五分钟不现实。
        {
            let mut slots = c.lock();
            if let Some(Slot::Ready { at, .. }) = slots.get_mut("a.exe") {
                *at = Instant::now() - TTL - Duration::from_secs(1);
            }
        }
        assert!(c.get("a.exe").await.is_some());
        assert_eq!(计数3.load(Ordering::SeqCst), 2, "过期后应当重新提取");
    }

    计数桩!(慢桩1, 慢计数1, 慢);
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 同名并发只提取一次() {
        let c = Arc::new(IconCache::with_extractor(慢桩1));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let c = Arc::clone(&c);
            tasks.push(tokio::spawn(async move { c.get("chrome.exe").await }));
        }
        for t in tasks {
            assert_eq!(
                t.await.unwrap().as_deref().map(Vec::as_slice),
                Some(b"chrome.exe".as_slice())
            );
        }
        assert_eq!(
            慢计数1.load(Ordering::SeqCst),
            1,
            "八个同名并发请求应当合并成一次提取"
        );
    }

    计数桩!(慢桩2, 慢计数2, 慢);
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 提取方被取消后有人接手() {
        let c = Arc::new(IconCache::with_extractor(慢桩2));
        let 先来 = {
            let c = Arc::clone(&c);
            tokio::spawn(async move { c.get("x.exe").await })
        };
        // 让它先把占位放进去。
        tokio::time::sleep(Duration::from_millis(30)).await;
        先来.abort();
        let _ = 先来.await;
        // 占位要么已被守卫撤掉，要么马上会被撤掉；后来者必须能拿到结果而不是卡住。
        let got = tokio::time::timeout(Duration::from_secs(5), c.get("x.exe"))
            .await
            .expect("被取消的提取方不该让后来者永远等下去");
        assert_eq!(got.as_deref().map(Vec::as_slice), Some(b"x.exe".as_slice()));
    }

    计数桩!(桩4, 计数4, 常规);
    #[tokio::test]
    async fn 补热只重取快过期的_并跳过负缓存() {
        let c = IconCache::with_extractor(桩4);
        c.get("fresh.exe").await;
        c.get("stale.exe").await;
        c.get("no-icon.exe").await;
        assert_eq!(计数4.load(Ordering::SeqCst), 3);

        // 把 stale 推到「撑不到下一轮」，把 no-icon 也推老，看它会不会被重试。
        {
            let mut slots = c.lock();
            let 老 = Instant::now() - (TTL - REFRESH_INTERVAL) - Duration::from_secs(1);
            for k in ["stale.exe", "no-icon.exe"] {
                if let Some(Slot::Ready { at, .. }) = slots.get_mut(k) {
                    *at = 老;
                }
            }
        }

        let r = c
            .refresh(vec![
                "fresh.exe".to_owned(),
                "stale.exe".to_owned(),
                "no-icon.exe".to_owned(),
                "brand-new.exe".to_owned(),
            ])
            .await;
        assert_eq!(r.total, 4);
        assert_eq!(r.attempted, 2, "只该重取 stale 与 brand-new");
        assert_eq!(r.succeeded, 2);
        assert_eq!(r.failed, 0);
        assert_eq!(
            计数4.load(Ordering::SeqCst),
            5,
            "fresh 还新鲜、no-icon 是负缓存，两者都不该再提取"
        );
    }

    计数桩!(桩5, 计数5, 常规);
    #[tokio::test]
    async fn 容量上限不会无界增长() {
        let _ = &计数5;
        let c = IconCache::with_extractor(桩5);
        for i in 0..(CAPACITY + 32) {
            c.get(&format!("p{i}.exe")).await;
        }
        assert!(
            c.len() <= CAPACITY,
            "缓存涨到了 {} 条，超过上限 {CAPACITY}",
            c.len()
        );
    }

    #[test]
    fn 补热间隔与_ttl_的关系() {
        assert_eq!(REFRESH_INTERVAL * 2, TTL, "补热间隔必须是 TTL 的一半");
        assert!(
            TTL - REFRESH_INTERVAL == REFRESH_INTERVAL,
            "「撑不到下一轮」的判据依赖这个等式"
        );
    }

    #[test]
    fn 本平台能力探测有结论() {
        // 只要求它不 panic 并给出一个确定的答案；Windows 上应为 true。
        let a = available();
        #[cfg(windows)]
        assert!(a, "Windows 上应当能取图标");
        #[cfg(not(windows))]
        assert!(!a, "macOS / Linux 的实现还是空的，应当为 false");
    }
}
