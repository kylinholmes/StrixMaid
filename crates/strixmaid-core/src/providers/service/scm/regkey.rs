//! 把服务的注册表键渲染成可读文本——Windows 上与 systemd unit 文件等价的东西。
//!
//! # 为什么是注册表而不是某个文件
//!
//! systemd 的 unit 文件是「服务定义的唯一事实来源」：`ExecStart`、`User=`、
//! `Requires=`、`Restart=` 全在里面，`systemctl cat` 把它原样吐出来。
//! Windows 上这份事实存在 `HKLM\SYSTEM\CurrentControlSet\Services\<名字>` 下——
//! `ImagePath`（≈ `ExecStart`）、`ObjectName`（≈ `User=`）、`DependOnService`
//! （≈ `Requires=`）、`Start`（≈ `[Install]` 的结果）、`FailureActions`（≈ `Restart=`）。
//! 这个键就是 unit 文件的对应物，所以 `unit_file` 渲染它。
//!
//! # 格式
//!
//! 照 `reg export` 的风格：`[键路径]` 一行，之后每个值一行 `"名字"=类型:数据`，
//! 默认值写成 `@=`。**但对文本类值做了一处刻意的偏离**：
//!
//! | 类型 | `reg export` 的写法 | 本模块的写法 | 为什么 |
//! |---|---|---|---|
//! | `REG_SZ` | `"ImagePath"="C:\\x.exe"` | 同左 | |
//! | `REG_EXPAND_SZ` | `"ImagePath"=hex(2):43,00,3a,00,…` | `"ImagePath"=expand_sz:"%SystemRoot%\\x.exe"` | ImagePath 绝大多数是这个类型，照 reg export 写就是一屏十六进制，读不出服务跑的是哪个程序 |
//! | `REG_MULTI_SZ` | `"DependOnService"=hex(7):52,00,…` | `"DependOnService"=multi_sz:"RpcSs";"http"` | 同上，依赖表是这里最该看清的东西之一 |
//! | `REG_DWORD` | `"Start"=dword:00000002` | 同左 | |
//! | `REG_QWORD` | `"x"=hex(b):…` | `"x"=qword:0000000000000002` | 同理 |
//! | 其余（`REG_BINARY` / `REG_NONE` / 资源表…） | `hex(3):` 全量 | `hex(3):` **前 32 字节 + 截断说明** | `FailureActions` 这类动辄几百字节，全量打印会把真正有用的几行挤掉 |
//!
//! 这不是「把原文改写了」——类型标签一个不少，只是把本来就是文本的东西按文本显示。
//! 需要逐字节核对的场景应该用 `reg export` 本身，而不是一个观测面板。
//!
//! # 范围
//!
//! 渲染主键与 `Parameters` 子键。后者是服务自己的配置（`ServiceDll`、监听端口…），
//! 与主键同属「这个服务是怎么定义的」。更深的子键（`Security` 是二进制 ACL、
//! `Enum` 是 PnP 运行时状态）不进——它们不是定义，是运行时产物。

use std::io;

use windows_sys::Win32::System::Registry::{
    REG_BINARY, REG_DWORD, REG_DWORD_BIG_ENDIAN, REG_EXPAND_SZ, REG_MULTI_SZ, REG_QWORD, REG_SZ,
    RegEnumValueW, RegQueryInfoKeyW,
};

use crate::platform::windows::registry::{HKLM, Key, open_read};
use crate::platform::windows::wide::{from_wide, from_wide_multi, from_wide_nul};

use super::map::{service_key_display, service_subkey};

/// 二进制值最多展示多少字节。再多对排障没有帮助，只会淹没同一个键里的文本值。
pub const BINARY_PREVIEW_BYTES: usize = 32;

/// 渲染一个服务的注册表定义：主键 + `Parameters` 子键。
///
/// 主键打不开（服务不存在，或该键被 ACL 保护）时返回 `Err`；
/// `Parameters` 不存在是常态，直接略过。
pub fn render_service(service: &str) -> io::Result<String> {
    let main = open_read(HKLM, &service_subkey(service))?;
    let mut out = render_key(&service_key_display(service), &main);

    let sub = format!(r"{}\Parameters", service_subkey(service));
    if let Ok(params) = open_read(HKLM, &sub) {
        out.push('\n');
        out.push_str(&render_key(
            &format!(r"{}\Parameters", service_key_display(service)),
            &params,
        ));
    }
    Ok(out)
}

/// 渲染一个已打开的键：`[路径]` + 每个值一行。
///
/// 值的顺序就是 `RegEnumValueW` 给的顺序（注册表内部顺序），不重排——
/// 与 `systemctl cat` 不重排 unit 文件里的行同理。
pub fn render_key(display_path: &str, key: &Key) -> String {
    let mut out = format!("[{display_path}]\n");
    for (name, ty, data) in enum_values(key) {
        out.push_str(&format_value(&name, ty, &data));
        out.push('\n');
    }
    out
}

/// 枚举一个键下的全部值：`(名字, 类型, 原始字节)`。
///
/// 先用 `RegQueryInfoKeyW` 问出值的条数与最长的名字 / 数据，再一次性按最大值
/// 申请缓冲——这样循环里不需要处理 `ERROR_MORE_DATA` 扩容。
fn enum_values(key: &Key) -> Vec<(String, u32, Vec<u8>)> {
    let mut count: u32 = 0;
    let mut max_name: u32 = 0;
    let mut max_data: u32 = 0;
    // SAFETY: 三个输出参数指向本栈帧上的可写变量；其余按文档允许为空。
    let rc = unsafe {
        RegQueryInfoKeyW(
            key.raw(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut count,
            &raw mut max_name,
            &raw mut max_data,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if !super::sys::reg_ok(rc) || count == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(count as usize);
    for index in 0..count {
        // max_name 是字符数，不含结尾 NUL。
        let mut name = vec![0u16; max_name as usize + 2];
        let mut name_len = name.len() as u32;
        // 空值（长度 0）也要给一个非悬垂的指针。
        let mut data = vec![0u8; (max_data as usize).max(1)];
        let mut data_len = data.len() as u32;
        let mut ty: u32 = 0;
        // SAFETY: name 有 name_len 个 u16 可写，data 有 data_len 字节可写，
        // 两个长度都如实描述缓冲；lpReserved 按文档必须为空。
        let rc = unsafe {
            RegEnumValueW(
                key.raw(),
                index,
                name.as_mut_ptr(),
                &raw mut name_len,
                std::ptr::null(),
                &raw mut ty,
                data.as_mut_ptr(),
                &raw mut data_len,
            )
        };
        if !super::sys::reg_ok(rc) {
            // 单个值读不出来（并发删除、ACL）不该毁掉整次渲染，跳过继续。
            continue;
        }
        data.truncate(data_len as usize);
        out.push((from_wide(&name[..name_len as usize]), ty, data));
    }
    out
}

/// 渲染一个值。纯函数，格式的全部规则都在这里，因此能用固定输入测。
pub fn format_value(name: &str, ty: u32, data: &[u8]) -> String {
    let left = if name.is_empty() {
        // reg export 用 @ 表示默认值。
        "@".to_owned()
    } else {
        format!("\"{}\"", escape(name))
    };
    format!("{left}={}", format_data(ty, data))
}

/// 值数据 → `类型:数据`。
fn format_data(ty: u32, data: &[u8]) -> String {
    match ty {
        REG_SZ => format!("\"{}\"", escape(&to_text(data))),
        REG_EXPAND_SZ => format!("expand_sz:\"{}\"", escape(&to_text(data))),
        REG_MULTI_SZ => {
            let items = from_wide_multi(&to_u16(data));
            let body = items
                .iter()
                .map(|s| format!("\"{}\"", escape(s)))
                .collect::<Vec<_>>()
                .join(";");
            format!("multi_sz:{body}")
        }
        REG_DWORD => match data.try_into() {
            Ok(b) => format!("dword:{:08x}", u32::from_le_bytes(b)),
            // 类型说是 DWORD 但长度不是 4：注册表里确实会有这种畸形值，
            // 按二进制原样展示，不猜。
            Err(_) => hex_dump(ty, data),
        },
        REG_DWORD_BIG_ENDIAN => match data.try_into() {
            Ok(b) => format!("dword_be:{:08x}", u32::from_be_bytes(b)),
            Err(_) => hex_dump(ty, data),
        },
        REG_QWORD => match data.try_into() {
            Ok(b) => format!("qword:{:016x}", u64::from_le_bytes(b)),
            Err(_) => hex_dump(ty, data),
        },
        // REG_BINARY / REG_NONE / REG_LINK / 资源表…
        _ => hex_dump(ty, data),
    }
}

/// `hex(<类型号>):xx,xx,…`，超过 [`BINARY_PREVIEW_BYTES`] 截断并注明总长。
fn hex_dump(ty: u32, data: &[u8]) -> String {
    let shown = data.len().min(BINARY_PREVIEW_BYTES);
    let body = data[..shown]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(",");
    // 注意 REG_BINARY 的类型号是 3，reg export 写作 hex(3)；这里对任何非文本
    // 类型都用同一写法，括号里就是注册表的类型号本身。
    let tag = if ty == REG_BINARY {
        "hex(3)".to_owned()
    } else {
        format!("hex({ty})")
    };
    if shown < data.len() {
        format!(
            "{tag}:{body},…（共 {} 字节，只显示前 {shown} 字节）",
            data.len()
        )
    } else {
        format!("{tag}:{body}")
    }
}

/// 原始字节 → UTF-16 → 文本（在第一个 NUL 处截断）。
///
/// 字节数为奇数时丢掉最后一个字节：注册表里确实存在这种被写坏的 `REG_SZ`，
/// 为它整次渲染失败不划算。
fn to_text(data: &[u8]) -> String {
    from_wide_nul(&to_u16(data))
}

/// 原始字节 → UTF-16 序列（小端，Windows 上恒定）。
fn to_u16(data: &[u8]) -> Vec<u16> {
    data.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect()
}

/// `reg export` 的转义：反斜杠与双引号。
fn escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一个 &str 编成 REG_SZ 的字节（含结尾 NUL）。
    fn sz(s: &str) -> Vec<u8> {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(u16::to_le_bytes)
            .collect()
    }

    /// 把若干 &str 编成 REG_MULTI_SZ 的字节（双 NUL 结尾）。
    fn multi(items: &[&str]) -> Vec<u8> {
        let mut v: Vec<u16> = Vec::new();
        for s in items {
            v.extend(s.encode_utf16());
            v.push(0);
        }
        v.push(0);
        v.into_iter().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn 文本值按文本渲染并转义反斜杠() {
        assert_eq!(
            format_value("ImagePath", REG_SZ, &sz(r"C:\Windows\System32\svchost.exe -k netsvcs")),
            r#""ImagePath"="C:\\Windows\\System32\\svchost.exe -k netsvcs""#
        );
        assert_eq!(
            format_value("ImagePath", REG_EXPAND_SZ, &sz(r"%SystemRoot%\system32\spoolsv.exe")),
            r#""ImagePath"=expand_sz:"%SystemRoot%\\system32\\spoolsv.exe""#
        );
        // 值名为空 = 默认值，reg export 写作 @
        assert_eq!(format_value("", REG_SZ, &sz("x")), r#"@="x""#);
        // 双引号也要转义
        assert_eq!(
            format_value("Q", REG_SZ, &sz(r#"say "hi""#)),
            r#""Q"="say \"hi\"""#
        );
    }

    #[test]
    fn 多串值展开成分号分隔的引号串() {
        assert_eq!(
            format_value("DependOnService", REG_MULTI_SZ, &multi(&["RPCSS", "http"])),
            r#""DependOnService"=multi_sz:"RPCSS";"http""#
        );
        // 空多串
        assert_eq!(
            format_value("DependOnService", REG_MULTI_SZ, &multi(&[])),
            r#""DependOnService"=multi_sz:"#
        );
    }

    #[test]
    fn 数值按固定宽度十六进制() {
        assert_eq!(
            format_value("Start", REG_DWORD, &2u32.to_le_bytes()),
            r#""Start"=dword:00000002"#
        );
        assert_eq!(
            format_value("T", REG_QWORD, &0x1234u64.to_le_bytes()),
            r#""T"=qword:0000000000001234"#
        );
        assert_eq!(
            format_value("B", REG_DWORD_BIG_ENDIAN, &2u32.to_be_bytes()),
            r#""B"=dword_be:00000002"#
        );
        // 类型说是 DWORD 但长度不对：退回二进制展示，不猜一个数出来
        assert_eq!(
            format_value("Bad", REG_DWORD, &[1u8, 2, 3]),
            r#""Bad"=hex(4):01,02,03"#
        );
    }

    #[test]
    fn 二进制值截断并注明总长() {
        let short = vec![0xaau8, 0xbb, 0xcc];
        assert_eq!(
            format_value("S", REG_BINARY, &short),
            r#""S"=hex(3):aa,bb,cc"#
        );

        let long: Vec<u8> = (0..100u8).collect();
        let line = format_value("FailureActions", REG_BINARY, &long);
        assert!(line.starts_with(r#""FailureActions"=hex(3):00,01,02"#), "{line}");
        assert!(
            line.contains(&format!("共 100 字节，只显示前 {BINARY_PREVIEW_BYTES} 字节")),
            "截断必须注明总长：{line}"
        );
        // 截断后逗号分隔的字节数正好是上限
        let body = line.split(':').nth(1).unwrap();
        let shown = body.split('，').next().unwrap().trim_end_matches(['(', '…', ',']);
        assert_eq!(shown.split(',').filter(|p| p.len() == 2).count(), BINARY_PREVIEW_BYTES);
    }

    #[test]
    fn 畸形文本值不会让渲染崩掉() {
        // 奇数字节数（被写坏的 REG_SZ）
        assert_eq!(format_value("X", REG_SZ, &[0x41, 0x00, 0x42]), r#""X"="A""#);
        // 空数据
        assert_eq!(format_value("X", REG_SZ, &[]), r#""X"="""#);
    }

    /// 本机实测：渲染一个真实服务的注册表键。
    ///
    /// 不指定具体服务——精简版 Windows 上 Spooler 之类可能不存在；
    /// 取枚举到的第一个能打开的服务即可。
    #[test]
    fn 本机能渲染服务注册表键() {
        use crate::platform::windows::registry::reg_subkeys;
        let names = reg_subkeys(HKLM, super::super::map::SERVICES_KEY);
        assert!(names.len() > 50, "只枚举到 {} 个服务键", names.len());

        let mut 渲染过 = 0usize;
        for name in names.iter() {
            let Ok(text) = render_service(name) else {
                continue;
            };
            assert!(
                text.starts_with(&format!("[HKEY_LOCAL_MACHINE\\{}", super::super::map::SERVICES_KEY)),
                "首行必须是键路径：{}",
                text.lines().next().unwrap_or_default()
            );
            if 渲染过 == 0 {
                eprintln!("--- {name} 的注册表定义 ---\n{text}");
            }
            渲染过 += 1;
            if 渲染过 >= 20 {
                break;
            }
        }
        assert!(渲染过 > 0, "一个服务键都渲染不出来");
    }
}
