//! 系统版本：直读 `/System/Library/CoreServices/SystemVersion.plist`。
//!
//! 这是 `sw_vers(1)` 自己的数据源，直读省掉一个子进程，也不受 `SYSTEM_VERSION_COMPAT`
//! 环境变量的影响（那个开关会让 `sw_vers` 对老程序谎报 10.16）。
//!
//! # 为什么手写解析而不是引 plist crate
//!
//! 这份文件是格式极其规整的 XML plist：一层 `<dict>`，全是
//! `<key>K</key><string>V</string>` 配对，没有嵌套、没有数组、没有二进制格式。
//! 为读三个字段引入一个 plist 依赖不划算——尤其它只在开发平台上用得到，
//! 却会进入所有平台的 `Cargo.lock`。解析器只认这一种形状，
//! 认不出就退回 `sysctl kern.osproductversion`。

use std::fs;

use strixmaid_types::system::OsInfo;

const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";

/// 系统版本三件套。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsVersion {
    /// `ProductName`，如 `macOS`。
    pub name: String,
    /// `ProductVersion`，如 `26.5.2`。
    pub version: Option<String>,
    /// `ProductBuildVersion`，如 `25F84`。
    pub build: Option<String>,
}

impl Default for OsVersion {
    fn default() -> Self {
        OsVersion {
            name: "macOS".to_owned(),
            version: None,
            build: None,
        }
    }
}

impl OsVersion {
    /// 转成 API 的 [`OsInfo`]。
    ///
    /// `id` 固定为 `macos`，与 Linux 侧取自 `/etc/os-release` 的 `ID=` 同级
    /// （那里是 `ubuntu` / `rocky` 这种小写标识）。
    pub fn to_os_info(&self) -> OsInfo {
        let pretty = match (&self.version, &self.build) {
            (Some(v), Some(b)) => format!("{} {v} ({b})", self.name),
            (Some(v), None) => format!("{} {v}", self.name),
            _ => self.name.clone(),
        };
        OsInfo {
            id: "macos".to_owned(),
            name: self.name.clone(),
            version: self.version.clone(),
            pretty_name: pretty,
        }
    }
}

/// 读本机版本。plist 读不到时退回 sysctl，再不行就只剩一个 `macOS`。
pub fn read() -> OsVersion {
    let from_plist = fs::read_to_string(SYSTEM_VERSION_PLIST)
        .ok()
        .and_then(|raw| parse_plist(&raw));
    if let Some(v) = from_plist
        && v.version.is_some()
    {
        return v;
    }
    OsVersion {
        name: "macOS".to_owned(),
        version: crate::platform::macos::sysctl_str("kern.osproductversion"),
        build: crate::platform::macos::sysctl_str("kern.osversion"),
    }
}

/// 解析 `SystemVersion.plist`。纯函数，可用固定文本单测。
///
/// 只认「`<key>` 紧跟 `<string>`」这一种形状，任何一项缺失都退化成 `None`，
/// 不 panic、不报错。
pub fn parse_plist(raw: &str) -> Option<OsVersion> {
    let get = |key: &str| -> Option<String> {
        let needle = format!("<key>{key}</key>");
        let after = raw.split_once(&needle)?.1;
        let after = after.split_once("<string>")?.1;
        let (value, _) = after.split_once("</string>")?;
        let value = value.trim();
        (!value.is_empty()).then(|| value.to_owned())
    };
    Some(OsVersion {
        name: get("ProductName").unwrap_or_else(|| "macOS".to_owned()),
        version: get("ProductVersion"),
        build: get("ProductBuildVersion"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>BuildID</key>
	<string>3FF138BC-7035-11F1-A491-5FE68ADA1E26</string>
	<key>ProductBuildVersion</key>
	<string>25F84</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductVersion</key>
	<string>26.5.2</string>
</dict>
</plist>
"#;

    #[test]
    fn 解析固定文本() {
        let v = parse_plist(SAMPLE).unwrap();
        assert_eq!(v.name, "macOS");
        assert_eq!(v.version.as_deref(), Some("26.5.2"));
        assert_eq!(v.build.as_deref(), Some("25F84"));
    }

    #[test]
    fn 缺字段时退化而不报错() {
        let v = parse_plist("<dict><key>ProductName</key><string>macOS</string></dict>").unwrap();
        assert_eq!(v.name, "macOS");
        assert_eq!(v.version, None);
        assert_eq!(v.build, None);
        // 完全不相干的内容：名字用兜底值，版本为空
        let v = parse_plist("not a plist at all").unwrap();
        assert_eq!(v.name, "macOS");
        assert_eq!(v.version, None);
    }

    #[test]
    fn 转_os_info() {
        let full = OsVersion {
            name: "macOS".into(),
            version: Some("26.5.2".into()),
            build: Some("25F84".into()),
        };
        let info = full.to_os_info();
        assert_eq!(info.id, "macos");
        assert_eq!(info.pretty_name, "macOS 26.5.2 (25F84)");

        let no_build = OsVersion {
            build: None,
            ..full
        };
        assert_eq!(no_build.to_os_info().pretty_name, "macOS 26.5.2");

        assert_eq!(OsVersion::default().to_os_info().pretty_name, "macOS");
    }

    #[test]
    fn 本机版本() {
        let v = read();
        assert_eq!(v.name, "macOS");
        assert!(v.version.is_some(), "本机必须读得出系统版本");
        eprintln!("本机系统：{}", v.to_os_info().pretty_name);
    }
}
