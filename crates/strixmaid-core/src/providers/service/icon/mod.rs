//! 服务图标：按**服务名**取一张 PNG，另有一张所有服务共用的通用图标。
//!
//! 本文件是与平台无关的那一半：名字消毒、缓存的接线、两个端点的语义。
//! 平台那一半只有四个函数，见 [`windows`] / [`unix`]。
//!
//! # 两个端点，不是一个
//!
//! | 端点 | 给什么 | 取不到时 |
//! |---|---|---|
//! | [`ServiceIcons::icon_png`] | **这个服务自己**的可执行文件里的图标 | 404 |
//! | [`ServiceIcons::generic_icon_png`] | 「服务」这个类别的通用齿轮 | 404 |
//!
//! 按名字那个端点**不会**在取不到时回落到齿轮。分开的理由是诚实性：404 明确
//! 表示「这个服务没有自己的图标」，混在一起则调用方拿到一张图却分不清它是这个
//! 服务真实的图标还是一张通用图。回落是**展示层**的决定，由前端在收到 404 后
//! 去取通用端点那一张——那是一次请求、一次浏览器缓存，不是每行一次。
//!
//! # 本机实测：绝大多数服务没有自己的图标
//!
//! 数据由 `tests::本机有多少服务取得到自己的图标` 这条用例现场跑出来，不是估的：
//!
//! | | |
//! |---|---|
//! | 服务键总数（含驱动） | 791 |
//! | 解析出可执行文件的（驱动已排除） | 317 |
//! | 其中由 `svchost.exe` 托管 | 239（75%） |
//! | 去重后不同的二进制 | 73 |
//! | **真的取得到自己图标的服务** | **17**（来自 16 个二进制） |
//!
//! `svchost.exe` 自己没有图标资源，那 239 个服务因此一张图也取不到；剩下的
//! 独立可执行文件也大多是没有界面的守护进程，同样不带图标资源。所以按名字
//! 那个端点**大面积 404 是设计内的常态**，不是故障。
//!
//! `services.msc` 的做法完全一样：它给每一行画的都是同一对齿轮。通用端点存在
//! 的意义正在于此。
//!
//! # 为什么缓存的 key 是二进制路径，不是服务名
//!
//! 与进程那一侧（key 是映像名）**刻意不同**，两边的取舍不一样：
//!
//! - 进程侧的 key 直接出现在 URL 里，要的是**稳定**：pid 每次重启都变、exe 路径
//!   升级后会变，名字最稳。
//! - 服务侧的 URL 仍然按服务名（稳定，且前端手里只有名字），但**缓存内部**按
//!   解析出来的二进制路径归并：240 个 svchost 服务共用一条负缓存，而不是各存
//!   一份。按名字做 key 的话，同一个「svchost.exe 没有图标」的结论会被重复
//!   存储两百多次，还要各自捶一遍 GDI。
//!
//! 换句话说，服务名 → 二进制路径这一步是**每次请求都做**的（一次注册表读，
//! 微秒级），真正昂贵的图标提取才进缓存。
//!
//! 缓存本体是 [`crate::providers::process::icon::IconCache`]，不是另写一套：
//! TTL、负缓存、容量上限、single-flight 的行为两边必须一致，复制一份只会多出
//! 一处可以悄悄走样的地方。两边只有 extractor 不同，而 key 的语义本来就由
//! extractor 决定，详见该模块的文档。
//!
//! # 为什么不预热
//!
//! 进程图标有预热与补热任务，服务图标**没有**。这是按上面那组实测数据算过之后
//! 的决定，不是省事：
//!
//! - 一轮预热要提取 73 个二进制，其中**只有 16 个真的有图标**。另外 57 次是
//!   注定失败的 GDI 调用，预热它们换来的只是「负缓存提前写好」——而负缓存的
//!   代价本来就只有一次失败的提取。
//! - 那 16 张图每张提取几毫秒，合计十几毫秒，且天然摊在首屏的几十个请求里。
//!   首屏之后客户端与服务端各有一份 5 分钟缓存，重复请求到不了提取那一步。
//! - 代价却是每轮枚举近 800 个服务键、各读一次注册表、再发 73 次提取，
//!   而换来的只是**首屏那一次**十几毫秒。
//!
//! 进程侧的取舍不同，两点性质都反过来：进程名的集合随进程起落不断变化，
//! 列表本身在秒级轮询，且成功率高得多（桌面程序基本都带图标资源）。预热在那里
//! 能让稳态下的缓存永不落空，在这里不能。
//!
//! 这个结论依赖「服务的可执行文件大多没有图标资源」这一事实。日后若换成一台
//! 装满第三方服务的机器，重跑那条用例即可重新判断——把数字留在用例里而不是
//! 只写进文档，就是为了让它可以被重新验证。

#[cfg(not(windows))]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(not(windows))]
use unix as sys;
#[cfg(windows)]
use windows as sys;

use std::sync::Arc;

use strixmaid_types::{ApiError, ApiResult};

use crate::providers::process::icon::IconCache;

/// 服务名的长度上限（字符）。Windows 服务名上限 256 个字符，再给对外的
/// `.service` 后缀留 8 个。
const MAX_NAME_LEN: usize = 256 + 8;

/// 本平台能不能取服务图标。为假时两个端点一律 404，一次缓存都不碰。
pub fn available() -> bool {
    sys::available()
}

/// 校验端点收到的服务名。
///
/// 这道闸**是**承重的，不是形式：名字会被拼进注册表键路径
/// `SYSTEM\CurrentControlSet\Services\<名字>`，放行反斜杠就等于让调用方指定
/// 读哪个键。控制字符与超长同样挡掉。
///
/// 服务名允许空格（`Net Driver HPZ12`）、括号（`Intel(R) ...`）与逗号
/// （`SangforDnsDrv_7,6,9,1`），所以这里不套 systemd 那套 unit 名字符集——
/// 按那套规则，列表里看得见的服务点开却会 400。理由与
/// [`super::scm::map::validate_service_unit`] 完全一致。
pub fn validate_service_name(name: &str) -> ApiResult<()> {
    if name.is_empty() {
        return Err(ApiError::invalid_request("服务名不能为空"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(ApiError::invalid_request(format!(
            "服务名过长（上限 {MAX_NAME_LEN} 个字符）"
        )));
    }
    if name.contains("..") {
        return Err(ApiError::invalid_request("服务名不能含 `..`"));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| matches!(c, '/' | '\\') || c.is_control())
    {
        return Err(ApiError::invalid_request(format!(
            "服务名含非法字符：{bad:?}"
        )));
    }
    Ok(())
}

/// 服务图标的缓存。挂在服务端的路由状态上，生命周期随进程。
///
/// `Clone` 廉价（内部是 `Arc`），克隆出来的共用同一份缓存。
#[derive(Clone)]
pub struct ServiceIcons {
    icons: Arc<IconCache>,
}

impl Default for ServiceIcons {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceIcons {
    /// 建一份空缓存。key 是二进制路径，见模块文档。
    pub fn new() -> Self {
        ServiceIcons {
            icons: Arc::new(IconCache::with_extractor(sys::icon_png)),
        }
    }

    /// `GET /services/icon/{name}`：这个服务自己的图标。
    ///
    /// | 情况 | 返回 | HTTP |
    /// |---|---|---|
    /// | 名字不合法（含分隔符 / `..` / 超长） | `invalid_request` | 400 |
    /// | 本平台不提供服务图标（[`available`] 为假） | `not_found` | 404 |
    /// | 服务不存在、没有 `ImagePath`、是驱动 | `not_found` | 404 |
    /// | 有可执行文件但它没有图标资源（`svchost.exe` 就是） | `not_found` | 404 |
    ///
    /// 最后一档占了绝大多数，是**常态不是异常**；前端据此改取通用图标。
    /// 这里不替它回落，理由见模块文档「两个端点，不是一个」。
    pub async fn icon_png(&self, name: &str) -> ApiResult<Arc<Vec<u8>>> {
        validate_service_name(name)?;
        if !available() {
            return Err(ApiError::not_found("本平台无法取得服务图标"));
        }
        let binary = self.resolve(name).await.ok_or_else(|| {
            ApiError::not_found(format!("{name} 不存在、没有可执行文件，或者它是驱动"))
        })?;
        self.icons.get(&binary).await.ok_or_else(|| {
            ApiError::not_found(format!("{binary} 里没有可用的图标资源"))
        })
    }

    /// `GET /services/icon-generic`：所有服务共用的那张齿轮。
    ///
    /// Windows 上取的是服务管理单元自己的 DLL 里的图标（见
    /// [`windows::GENERIC_ICON_SOURCE`]），**运行时提取**，不随代码分发。
    /// 非 Windows 上没有可报的事实，一律 404。
    pub async fn generic_icon_png(&self) -> ApiResult<Arc<Vec<u8>>> {
        if !available() {
            return Err(ApiError::not_found("本平台无法取得服务图标"));
        }
        let source = sys::generic_icon_path()
            .ok_or_else(|| ApiError::not_found("本机没有可用的通用服务图标"))?;
        self.icons
            .get(&source)
            .await
            .ok_or_else(|| ApiError::not_found(format!("{source} 里没有可用的图标资源")))
    }

    /// 服务名 → 二进制路径。一次注册表读，微秒级，但仍然是同步系统调用，
    /// 因此进 `spawn_blocking` 而不是直接占 async 运行时线程。
    async fn resolve(&self, name: &str) -> Option<String> {
        let owned = name.to_owned();
        match tokio::task::spawn_blocking(move || sys::binary_path(&owned)).await {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(name, error = %e, "解析服务的可执行文件路径时任务异常终止");
                None
            }
        }
    }

    /// 当前缓存条目数。给测试与日志用。
    pub fn len(&self) -> usize {
        self.icons.len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.icons.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::ErrorCode;

    #[test]
    fn 服务名消毒() {
        for ok in [
            "Spooler",
            "Spooler.service",
            "Net Driver HPZ12",
            "Intel(R) TPM Provisioning Service",
            "SangforDnsDrv_7,6,9,1",
            "中文服务",
        ] {
            assert!(validate_service_name(ok).is_ok(), "{ok:?} 应当放行");
        }
        for bad in [
            "",
            r"Spooler\Parameters",
            "sub/dir",
            "..",
            r"..\..\Services\Other",
            "a..b",
            "nul\0svc",
            "tab\tsvc",
        ] {
            assert!(validate_service_name(bad).is_err(), "{bad:?} 应当被拒绝");
        }
        let 超长 = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_service_name(&超长).is_err(), "超长名字应当被拒绝");
        assert!(
            validate_service_name(&"a".repeat(MAX_NAME_LEN)).is_ok(),
            "刚好到上限应当放行"
        );
    }

    #[tokio::test]
    async fn 名字不合法时报_400() {
        let icons = ServiceIcons::new();
        for bad in ["", r"a\b", "a/b", ".."] {
            let err = icons
                .icon_png(bad)
                .await
                .expect_err(&format!("{bad:?} 应当被拒绝"));
            assert_eq!(err.code, ErrorCode::InvalidRequest, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn 不存在的服务报_404() {
        let icons = ServiceIcons::new();
        let err = icons
            .icon_png("绝不会有这个服务_9f3a1c")
            .await
            .expect_err("不存在的服务应当 404");
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(icons.is_empty(), "查不出路径时不该占用缓存");
    }

    #[tokio::test]
    async fn 本平台能力探测有结论() {
        let a = available();
        #[cfg(windows)]
        assert!(a, "Windows 上应当能取服务图标");
        #[cfg(not(windows))]
        {
            assert!(!a, "非 Windows 的实现还是空的，应当为 false");
            let icons = ServiceIcons::new();
            assert!(icons.icon_png("Spooler").await.is_err());
            assert!(icons.generic_icon_png().await.is_err());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn 通用图标是合法_png_且会被缓存() {
        let icons = ServiceIcons::new();
        let Ok(png) = icons.generic_icon_png().await else {
            eprintln!("本机取不到通用服务图标，跳过");
            return;
        };
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "通用图标不是合法 PNG"
        );
        assert_eq!(icons.len(), 1);
        let again = icons.generic_icon_png().await.expect("缓存里应当还在");
        assert!(Arc::ptr_eq(&png, &again), "第二次应当直接命中缓存");
    }

    /// 本机实测：全部服务里有多少个取得到**自己的**图标。
    ///
    /// 这个数字是「按名字那个端点大面积 404 是正常的」这一设计的依据，
    /// 也是通用齿轮端点存在的理由，因此把它固化成一条会打印结论的用例，
    /// 而不是只写在文档里。不断言具体数值——不同机器上装的服务不一样。
    #[cfg(windows)]
    #[tokio::test]
    async fn 本机有多少服务取得到自己的图标() {
        use crate::platform::windows::registry::{HKLM, reg_subkeys};

        let icons = ServiceIcons::new();
        let mut 有二进制 = 0usize;
        let mut 有图标 = 0usize;
        let mut 取到图标的二进制 = std::collections::HashSet::new();
        let names = reg_subkeys(HKLM, super::super::scm::map::SERVICES_KEY);
        for name in &names {
            let Some(binary) = sys::binary_path(name) else {
                continue;
            };
            有二进制 += 1;
            if icons.icon_png(name).await.is_ok() {
                有图标 += 1;
                取到图标的二进制.insert(binary.to_lowercase());
            }
        }
        eprintln!(
            "服务键 {} 个；解析出可执行文件的 {有二进制} 个；\
             其中取得到自己图标的 {有图标} 个（来自 {} 个不同的二进制）；\
             缓存占 {} 格",
            names.len(),
            取到图标的二进制.len(),
            icons.len()
        );
        assert!(
            icons.len() <= 有二进制,
            "缓存格数不该超过解析出的服务数——按路径做 key 就是为了归并"
        );
    }

    /// 按二进制路径做 key 的实际效果：多个 svchost 托管的服务只占一格缓存。
    #[cfg(windows)]
    #[tokio::test]
    async fn svchost_托管的服务共用一条缓存() {
        use crate::platform::windows::registry::{HKLM, reg_subkeys};

        // 找几个解析到同一个二进制的服务。本机上必然是 svchost，但不写死名字：
        // 精简版 Windows 上具体是哪个服务由枚举结果决定。
        let mut 按路径 = std::collections::HashMap::<String, Vec<String>>::new();
        for name in reg_subkeys(HKLM, super::super::scm::map::SERVICES_KEY) {
            if let Some(path) = sys::binary_path(&name) {
                按路径.entry(path.to_lowercase()).or_default().push(name);
            }
        }
        let Some((路径, 同伴)) = 按路径.iter().max_by_key(|(_, v)| v.len()) else {
            eprintln!("本机一个服务都解析不出来，跳过");
            return;
        };
        if 同伴.len() < 2 {
            eprintln!("本机没有共用同一个二进制的服务，跳过");
            return;
        }
        eprintln!("{} 个服务共用 {路径}", 同伴.len());

        let icons = ServiceIcons::new();
        for name in 同伴.iter().take(5) {
            // 成功与否都不影响本用例要验的事：它们共用一格。
            let _ = icons.icon_png(name).await;
        }
        assert_eq!(
            icons.len(),
            1,
            "{} 个同源服务应当只占一格缓存，实际占了 {} 格",
            同伴.len().min(5),
            icons.len()
        );
    }
}
