//! D-Bus 错误到 API 错误的映射。
use strixmaid_types::{ApiError, ErrorCode};
/// zbus 错误 → [`ApiError`]。`unit` 只用于拼消息。
pub fn map_zbus_error(e: zbus::Error, unit: &str) -> ApiError {
    match &e {
        zbus::Error::MethodError(name, msg, _) => {
            let detail = msg.clone().unwrap_or_default();
            map_error_name(name.as_str(), detail, unit)
        }
        zbus::Error::FDO(fdo) => match fdo.as_ref() {
            zbus::fdo::Error::AccessDenied(m)
            | zbus::fdo::Error::AuthFailed(m)
            | zbus::fdo::Error::InteractiveAuthorizationRequired(m) => denied(unit, m.clone()),
            zbus::fdo::Error::ServiceUnknown(m) | zbus::fdo::Error::NameHasNoOwner(m) => {
                ApiError::new(ErrorCode::Unavailable, "systemd 不在 bus 上").with_detail(m.clone())
            }
            zbus::fdo::Error::NoReply(m) | zbus::fdo::Error::Timeout(m) => {
                ApiError::new(ErrorCode::Timeout, "systemd 无响应").with_detail(m.clone())
            }
            other => ApiError::internal(format!("systemd 调用失败（{unit}）"))
                .with_detail(other.to_string()),
        },
        zbus::Error::InputOutput(_) | zbus::Error::Connection(..) | zbus::Error::Handshake(_) => {
            ApiError::new(ErrorCode::Unavailable, "systemd bus 连接中断").with_detail(e.to_string())
        }
        _ => ApiError::internal(format!("systemd 调用失败（{unit}）")).with_detail(e.to_string()),
    }
}

/// 按 D-Bus 错误名分类。
pub(super) fn map_error_name(name: &str, detail: String, unit: &str) -> ApiError {
    match name {
        "org.freedesktop.systemd1.NoSuchUnit" | "org.freedesktop.DBus.Error.FileNotFound" => {
            ApiError::not_found(format!("unit {unit} 不存在")).with_detail(detail)
        }
        "org.freedesktop.DBus.Error.AccessDenied"
        | "org.freedesktop.DBus.Error.AuthFailed"
        | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired" => denied(unit, detail),
        "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" => {
            ApiError::new(ErrorCode::Timeout, "systemd 无响应").with_detail(detail)
        }
        "org.freedesktop.DBus.Error.ServiceUnknown"
        | "org.freedesktop.DBus.Error.NameHasNoOwner" => {
            ApiError::new(ErrorCode::Unavailable, "systemd 不在 bus 上").with_detail(detail)
        }
        "org.freedesktop.systemd1.UnitMasked"
        | "org.freedesktop.systemd1.NoSuchJob"
        | "org.freedesktop.systemd1.JobTypeNotApplicable"
        | "org.freedesktop.systemd1.UnitExists"
        | "org.freedesktop.systemd1.OnlyByDependency"
        | "org.freedesktop.systemd1.LoadFailed"
        | "org.freedesktop.systemd1.BadUnitSetting"
        | "org.freedesktop.systemd1.ShuttingDown"
        | "org.freedesktop.systemd1.TransactionIsDestructive"
        | "org.freedesktop.DBus.Error.FileExists" => ApiError::new(
            ErrorCode::Conflict,
            format!("systemd 拒绝了对 {unit} 的操作"),
        )
        .with_detail(format!("{name}: {detail}")),
        _ => ApiError::internal(format!("systemd 调用失败（{unit}）"))
            .with_detail(format!("{name}: {detail}")),
    }
}

fn denied(unit: &str, detail: String) -> ApiError {
    ApiError::permission_denied(format!("需要管理访问：polkit 拒绝了对 {unit} 的操作"))
        .with_detail(detail)
        .retry_elevated()
}
