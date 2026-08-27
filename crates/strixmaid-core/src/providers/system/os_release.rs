//! `/etc/os-release` 与 `/etc/machine-info` 的解析（二者是同一种 shell 风格的键值格式）。
//!
//! 从 Phase 0 的 `strixmaid-server/src/routes/system.rs` 迁入，逻辑不变。

use std::fs;
use std::path::Path;

use strixmaid_types::system::OsInfo;

/// 读取并解析 os-release。
///
/// 规范要求 `/etc/os-release` 是指向 `../usr/lib/os-release` 的软链，
/// 但只提供后者的系统（部分不可变发行版）也存在，两处都试。
pub fn read_os_release() -> Option<OsInfo> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .into_iter()
        .find_map(|p| fs::read_to_string(Path::new(p)).ok())
        .and_then(|raw| parse_os_release(&raw))
}

/// 解析 os-release 文本。
///
/// `ID` / `NAME` / `PRETTY_NAME` 在 [`OsInfo`] 里是必填的，缺任何一个就整体返回 `None`
/// ——半个发行版身份不如没有。
pub fn parse_os_release(raw: &str) -> Option<OsInfo> {
    let (mut id, mut name, mut version, mut pretty_name) = (None, None, None, None);
    for (key, value) in kv_lines(raw) {
        match key {
            "ID" => id = Some(value),
            "NAME" => name = Some(value),
            "VERSION_ID" => version = Some(value),
            "PRETTY_NAME" => pretty_name = Some(value),
            _ => {}
        }
    }
    Some(OsInfo {
        id: id?,
        name: name?,
        version,
        pretty_name: pretty_name?,
    })
}

/// `/etc/machine-info` 里的 `PRETTY_HOSTNAME`。多数机器没有这个文件。
pub fn read_pretty_hostname() -> Option<String> {
    let raw = fs::read_to_string("/etc/machine-info").ok()?;
    parse_pretty_hostname(&raw)
}

/// 从 machine-info 文本里取 `PRETTY_HOSTNAME`；为空串时视同没有。
pub fn parse_pretty_hostname(raw: &str) -> Option<String> {
    kv_lines(raw)
        .find(|(k, _)| *k == "PRETTY_HOSTNAME")
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// 逐行产出 `(KEY, 已去引号的值)`，跳过空行与注释。
fn kv_lines(raw: &str) -> impl Iterator<Item = (&str, String)> {
    raw.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((key.trim(), unquote_shell(value.trim())))
    })
}

/// os-release 的值是 shell 风格的：可能被单/双引号包裹，双引号内可有 `\` 转义。
pub(crate) fn unquote_shell(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some('"') => unescape_until(chars.as_str(), '"'),
        // 单引号内 shell 不做转义，原样取到收尾引号。
        Some('\'') => {
            let rest = chars.as_str();
            rest.strip_suffix('\'').unwrap_or(rest).to_owned()
        }
        _ => value.to_owned(),
    }
}

/// 处理双引号内的 `\x` 转义，并丢弃收尾引号。
fn unescape_until(rest: &str, quote: char) -> String {
    let mut out = String::with_capacity(rest.len());
    let mut it = rest.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => {
                if let Some(next) = it.next() {
                    out.push(next);
                }
            }
            c if c == quote => break,
            c => out.push(c),
        }
    }
    out
}

/// 把值编码成双引号包裹的 shell 字面量（写 `/etc/machine-info` 用）。
pub(crate) fn quote_shell(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const UBUNTU: &str = r#"PRETTY_NAME="Ubuntu 24.04.2 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
VERSION="24.04.2 LTS (Noble Numbat)"
ID=ubuntu
ID_LIKE=debian
# comment
HOME_URL="https://www.ubuntu.com/"
"#;

    #[test]
    fn 解析_ubuntu_os_release() {
        let os = parse_os_release(UBUNTU).unwrap();
        assert_eq!(os.id, "ubuntu");
        assert_eq!(os.name, "Ubuntu");
        assert_eq!(os.version.as_deref(), Some("24.04"));
        assert_eq!(os.pretty_name, "Ubuntu 24.04.2 LTS");
    }

    #[test]
    fn 滚动发行版没有_version_id() {
        let raw = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        let os = parse_os_release(raw).unwrap();
        assert_eq!(os.version, None);
    }

    #[test]
    fn 缺必填字段整体为_none() {
        assert!(parse_os_release("ID=foo\nNAME=Foo\n").is_none());
    }

    #[test]
    fn 引号与转义() {
        assert_eq!(unquote_shell(r#""a \"b\" c""#), r#"a "b" c"#);
        assert_eq!(unquote_shell("'it''s'"), "it''s");
        assert_eq!(unquote_shell("plain"), "plain");
        assert_eq!(quote_shell(r#"生产 "Web" $1"#), r#""生产 \"Web\" \$1""#);
        assert_eq!(unquote_shell(&quote_shell("a\"b\\c")), "a\"b\\c");
    }

    #[test]
    fn pretty_hostname() {
        assert_eq!(
            parse_pretty_hostname("PRETTY_HOSTNAME=\"生产 Web 节点 1\"\nICON_NAME=computer\n"),
            Some("生产 Web 节点 1".to_owned())
        );
        assert_eq!(parse_pretty_hostname("PRETTY_HOSTNAME=\"\"\n"), None);
        assert_eq!(parse_pretty_hostname("ICON_NAME=computer\n"), None);
    }
}
