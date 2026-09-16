//! 系统版本：直读 `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`。
//!
//! 这一个键就是 Windows 上 `/etc/os-release` 的等价物——`winver`、
//! `systeminfo`、WMI 的 `Win32_OperatingSystem` 取的都是它。直读省掉 WMI
//! （一个 COM 守护进程）与子进程，与 `design.md` §1「从内核接口直读」一致。
//!
//! # `ProductName` 在 Windows 11 上写的是 "Windows 10"
//!
//! 这是本模块**唯一必须做的修正**。微软从未更新过 Win11 上这一项的值：
//! 26100 版（Windows 11 24H2）的 `ProductName` 依旧是
//! `Windows 10 Pro`。官方判定 11 的方式是看**内部版本号**——
//! `CurrentBuildNumber >= 22000` 即 Windows 11
//! （22000 是 Win11 的首个 RTM 版本）。
//!
//! 因此 [`fix_product_name`] 把 build ≥ 22000 时开头的 `Windows 10` 换成
//! `Windows 11`，其余情况一字不改（Server 版本的 `ProductName` 本来就带
//! 正确的年份，如 `Windows Server 2022 Standard`，不能碰）。
//!
//! # 与另两个平台的字段对应
//!
//! | `OsInfo` 字段 | Windows 取值 | Linux | macOS |
//! |---|---|---|---|
//! | `id` | 固定 `"windows"` | `/etc/os-release` 的 `ID` | 固定 `"macos"` |
//! | `name` | `ProductName`（修正后） | `NAME` | `ProductName` |
//! | `version` | `DisplayVersion`（`23H2`），没有则退回内部版本号 | `VERSION_ID` | `ProductVersion` |
//! | `pretty_name` | `Windows 11 Pro 23H2 (26100.1234)` | `PRETTY_NAME` | `macOS 26.5.2 (25F84)` |
//!
//! `DisplayVersion` 是 Win10 20H2 才引入的值；更老的系统上是 `ReleaseId`
//! （`1909` 这种四位数字），两者都取不到时退回内部版本号，不编。

use strixmaid_types::system::OsInfo;

use crate::platform::windows::{HKLM, reg_dword, reg_string};

/// 系统版本所在的注册表键。
pub const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

/// Windows 11 的首个内部版本号。build 达到它即为 Windows 11，见模块文档。
pub const WIN11_MIN_BUILD: u32 = 22000;

/// 从注册表读出的系统版本。字段与注册表值一一对应，读不到的为 `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OsVersion {
    /// `ProductName`，**已按 build 修正过 Win10/Win11**。读不到时为 `None`。
    pub product_name: Option<String>,
    /// `DisplayVersion`（`23H2`）；老系统退回 `ReleaseId`（`1909`）。
    pub display_version: Option<String>,
    /// `CurrentBuildNumber`（`26100`）。这是 NT 的内部版本号。
    pub build: Option<u32>,
    /// `UBR`（Update Build Revision，即补丁号，`26100.1234` 里的 `1234`）。
    pub ubr: Option<u32>,
    /// `CurrentMajorVersionNumber`（Win10 起才有）。
    pub major: Option<u32>,
    /// `CurrentMinorVersionNumber`。
    pub minor: Option<u32>,
    /// `EditionID`（`Professional` / `ServerStandard` / `Core`）。
    pub edition: Option<String>,
}

impl OsVersion {
    /// 内部版本号串：`26100.1234`。
    ///
    /// 这是 [`SystemInfo::kernel`](strixmaid_types::system::SystemInfo::kernel)
    /// 的取值。Windows **没有与内核分开发布的版本号**——NT 内核与系统同版本、
    /// 同步发布，`ntoskrnl.exe` 的文件版本就是 `10.0.26100.1234`。因此这里给出
    /// `build.UBR`，它与 `winver` 对话框里「操作系统内部版本」那一行完全一致，
    /// 是 Windows 上与 `uname -r` 语义最接近的东西。
    ///
    /// 连 build 都读不到时返回 `"unknown"`，与 Linux / macOS 侧读不到
    /// `osrelease` 时的兜底一致。
    pub fn kernel(&self) -> String {
        match (self.build, self.ubr) {
            (Some(b), Some(u)) => format!("{b}.{u}"),
            (Some(b), None) => b.to_string(),
            _ => "unknown".to_owned(),
        }
    }

    /// 转成 API 的 [`OsInfo`]。
    ///
    /// `id` 固定 `"windows"`，与 macOS 侧固定 `"macos"`、Linux 侧取
    /// `/etc/os-release` 的 `ID=` 同级。
    pub fn to_os_info(&self) -> OsInfo {
        let name = self
            .product_name
            .clone()
            .unwrap_or_else(|| "Windows".to_owned());
        // DisplayVersion 是给人看的版本（23H2）；老系统没有它，退回内部版本号——
        // 那至少是真的，比留空更有用。
        let version = self
            .display_version
            .clone()
            .or_else(|| self.build.map(|b| b.to_string()));
        OsInfo {
            id: "windows".to_owned(),
            name: name.clone(),
            version,
            pretty_name: pretty_name(&name, self.display_version.as_deref(), &self.kernel()),
        }
    }
}

/// 组装可读串：`Windows 11 专业版 23H2 (26100.1234)`。
///
/// 三段都可能缺：没有 `DisplayVersion` 就不写那一段，内部版本号读不到
/// （`kernel` 为 `"unknown"`）就不写括号那一段。
fn pretty_name(name: &str, display_version: Option<&str>, kernel: &str) -> String {
    let mut out = name.to_owned();
    if let Some(v) = display_version.filter(|v| !v.is_empty()) {
        out.push(' ');
        out.push_str(v);
    }
    if kernel != "unknown" {
        out.push_str(&format!(" ({kernel})"));
    }
    out
}

/// 按内部版本号修正 `ProductName`，见模块文档。
///
/// 纯函数，可用固定输入单测。
pub fn fix_product_name(raw: &str, build: Option<u32>) -> String {
    let is_win11 = build.is_some_and(|b| b >= WIN11_MIN_BUILD);
    match raw.strip_prefix("Windows 10") {
        // 只换开头那一处，后缀（`Pro for Workstations` 等）原样保留。
        Some(rest) if is_win11 => format!("Windows 11{rest}"),
        _ => raw.to_owned(),
    }
}

/// 读本机版本。任何一项读不到都退化成 `None`，不失败。
pub fn read() -> OsVersion {
    // CurrentBuildNumber 是 REG_SZ（历史原因），不是 DWORD。
    let build = reg_string(HKLM, CURRENT_VERSION, "CurrentBuildNumber")
        .and_then(|s| s.trim().parse::<u32>().ok())
        // Win10 起还有一份 DWORD 形式的 CurrentBuildNumber 的近亲，
        // 但不是所有版本都有；串读不到时退回它。
        .or_else(|| reg_dword(HKLM, CURRENT_VERSION, "CurrentBuild"));

    OsVersion {
        product_name: reg_string(HKLM, CURRENT_VERSION, "ProductName")
            .map(|raw| fix_product_name(&raw, build)),
        display_version: reg_string(HKLM, CURRENT_VERSION, "DisplayVersion")
            // 20H2 之前叫 ReleaseId（`1909`）。
            .or_else(|| reg_string(HKLM, CURRENT_VERSION, "ReleaseId")),
        build,
        ubr: reg_dword(HKLM, CURRENT_VERSION, "UBR"),
        major: reg_dword(HKLM, CURRENT_VERSION, "CurrentMajorVersionNumber"),
        minor: reg_dword(HKLM, CURRENT_VERSION, "CurrentMinorVersionNumber"),
        edition: reg_string(HKLM, CURRENT_VERSION, "EditionID"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win11_的_product_name_要修正() {
        // Win11 24H2 的注册表里写的就是 "Windows 10 Pro"
        assert_eq!(
            fix_product_name("Windows 10 Pro", Some(26100)),
            "Windows 11 Pro"
        );
        assert_eq!(
            fix_product_name("Windows 10 Pro for Workstations", Some(22000)),
            "Windows 11 Pro for Workstations"
        );
        // 22000 是分界线，21996（Win11 泄露版）以下仍是 10
        assert_eq!(
            fix_product_name("Windows 10 Home", Some(21999)),
            "Windows 10 Home"
        );
        // 真的是 Windows 10
        assert_eq!(
            fix_product_name("Windows 10 Pro", Some(19045)),
            "Windows 10 Pro"
        );
        // build 读不到时不猜，原样返回
        assert_eq!(fix_product_name("Windows 10 Pro", None), "Windows 10 Pro");
    }

    #[test]
    fn server_版本不能被改() {
        // Server 的 ProductName 本来就带正确年份，build 再高也不能动
        assert_eq!(
            fix_product_name("Windows Server 2022 Standard", Some(26100)),
            "Windows Server 2022 Standard"
        );
        assert_eq!(
            fix_product_name("Windows Server 2025 Datacenter", Some(26100)),
            "Windows Server 2025 Datacenter"
        );
        // 不以 "Windows 10" 开头的一律不动
        assert_eq!(fix_product_name("Windows 8.1 Pro", Some(9600)), "Windows 8.1 Pro");
    }

    #[test]
    fn 内部版本号串() {
        let v = OsVersion {
            build: Some(26100),
            ubr: Some(1234),
            ..OsVersion::default()
        };
        assert_eq!(v.kernel(), "26100.1234");
        // 没有 UBR（少见，但不是不可能）时只给 build
        assert_eq!(
            OsVersion {
                build: Some(19044),
                ..OsVersion::default()
            }
            .kernel(),
            "19044"
        );
        // 什么都没有时给兜底值，不编
        assert_eq!(OsVersion::default().kernel(), "unknown");
    }

    #[test]
    fn 转_os_info() {
        let full = OsVersion {
            product_name: Some("Windows 11 Pro".into()),
            display_version: Some("23H2".into()),
            build: Some(26100),
            ubr: Some(1234),
            major: Some(10),
            minor: Some(0),
            edition: Some("Professional".into()),
        };
        let info = full.to_os_info();
        assert_eq!(info.id, "windows");
        assert_eq!(info.name, "Windows 11 Pro");
        assert_eq!(info.version.as_deref(), Some("23H2"));
        assert_eq!(info.pretty_name, "Windows 11 Pro 23H2 (26100.1234)");

        // 没有 DisplayVersion：version 退回内部版本号，pretty 少一段
        let no_display = OsVersion {
            display_version: None,
            ..full
        };
        let info = no_display.to_os_info();
        assert_eq!(info.version.as_deref(), Some("26100"));
        assert_eq!(info.pretty_name, "Windows 11 Pro (26100.1234)");

        // 什么都读不到：名字兜底，version 为 None（不是空串）
        let info = OsVersion::default().to_os_info();
        assert_eq!(info.name, "Windows");
        assert_eq!(info.version, None);
        assert_eq!(info.pretty_name, "Windows");
    }

    #[test]
    fn 本机版本() {
        let v = read();
        assert!(v.product_name.is_some(), "ProductName 在任何 Windows 上都存在");
        assert!(
            v.build.is_some_and(|b| b >= 7600),
            "内部版本号异常：{:?}（7600 是 Windows 7 RTM）",
            v.build
        );
        // 本机是 Win10 还是 Win11，名字必须与 build 自洽
        let name = v.product_name.clone().unwrap();
        if v.build.is_some_and(|b| b >= WIN11_MIN_BUILD) {
            assert!(
                !name.starts_with("Windows 10"),
                "build {:?} 已是 Windows 11，名字却还写着 {name}",
                v.build
            );
        }
        let info = v.to_os_info();
        assert_eq!(info.id, "windows");
        assert!(!info.pretty_name.is_empty());
        eprintln!("本机系统：{} / kernel={}", info.pretty_name, v.kernel());
        eprintln!("本机 OsVersion：{v:?}");
    }
}
