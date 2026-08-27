//! 认证与提权：challenge-response 协议（`docs/design.md` §5.2）。
//!
//! PAM 是对话式的（2FA 追问验证码、密码过期要求改密），所以协议从第一版就是多轮的，
//! MVP 只走一轮但结构留好：
//!
//! ```text
//! POST /api/v1/auth/start    { username }
//!   → 200 { session, prompts: [...] }
//! POST /api/v1/auth/respond  { session, responses: [ { id, value } ] }
//!   → 200 { status: "complete", token, user }       认证完成
//!   | 200 { status: "more", session, prompts: [..] } 继续追问
//!   | 401 ApiError                                   失败
//! ```
//!
//! 失败不是 `AuthOutcome` 的一个分支，而是 HTTP 401 + [`crate::ApiError`]，
//! 与其它所有错误走同一通道。
//!
//! 提权 `/api/v1/auth/elevate/{start,respond}` **复用完全相同的类型**，不另开一套。
//!
//! # 凭据安全（design.md §5.3，强制）
//!
//! [`PromptResponse::value`] 是从浏览器进来的**明文凭据**（密码 / OTP）。因此：
//!
//! 1. [`PromptResponse::value`] 的类型是 [`Zeroizing<String>`]，**drop 时自动擦除内存**。
//!    这一条由类型系统强制，不靠调用方自觉——它是三条硬约束里最容易被后来者破坏的一条。
//! 2. [`PromptResponse`] 与 [`AuthRespondReq`] **故意不实现 `Serialize`**——不实现就不可能
//!    被 `tracing` 的 `?field`、`serde_json::to_string` 或任何审计/回放路径带出去。
//!    这是本 crate 里唯一破坏「所有 DTO 都 Serialize」惯例的地方，是有意为之。
//! 3. `Debug` 手写为脱敏输出（`value: <redacted>`），deriving 会连 `Zeroizing` 里的明文一起打出来。
//! 4. 它们也不实现 `Clone`——每多一份拷贝就多一处需要擦除的内存，而拷贝出去的那份不受
//!    [`Zeroizing`] 保护。往下传时请 move，不要 clone。
//! 5. 绝不入库：`sessions` 表只存 token 的 hash。
//!
//! 反序列化路径同样安全：serde 先产出一个临时 `String`，随即被 [`Zeroizing`] 接管
//! （见 [`deserialize_zeroizing_string`]），不会留下第二份未受保护的拷贝。

use serde::{Deserialize, Deserializer, Serialize};
use utoipa::ToSchema;
use zeroize::Zeroizing;

/// PAM 提示的展示风格，对应 `PAM_PROMPT_ECHO_OFF` / `PAM_PROMPT_ECHO_ON` /
/// `PAM_TEXT_INFO` / `PAM_ERROR_MSG`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PromptStyle {
    /// 需要输入，且**不回显**（密码）。前端用 `<input type="password">`。
    Prompt,
    /// 需要输入，回显（用户名、OTP 序号之类）。
    PromptEcho,
    /// 纯信息，不需要输入（如「密码将在 3 天后过期」）。
    Info,
    /// 错误信息，不需要输入（如「上次登录失败」）。
    Error,
}

impl PromptStyle {
    /// 该风格是否需要用户回填 [`PromptResponse`]。
    /// `Info` / `Error` 只是展示，不要给它们造 response。
    pub const fn needs_input(self) -> bool {
        matches!(self, Self::Prompt | Self::PromptEcho)
    }
}

/// 一条 PAM 提示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Prompt {
    /// 本轮内唯一的序号，回应时按此 id 对应。**只在同一个 `session` 的同一轮内有效**，
    /// 下一轮 id 会重新编号。
    #[schema(example = 0)]
    pub id: u32,
    /// 展示风格，决定是否需要输入、是否回显。
    pub style: PromptStyle,
    /// PAM 给出的原文，可能是任意语言（取决于 PAM 模块与系统 locale），前端**原样展示**，
    /// 不要试图匹配内容做逻辑。
    #[schema(example = "Password: ")]
    pub text: String,
}

/// `POST /api/v1/auth/start` 与 `POST /api/v1/auth/elevate/start` 的请求体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthStartReq {
    /// 要认证的系统用户名。PAM 需要在会话开始时就知道它。
    ///
    /// 提权（elevate）时通常传当前会话的用户名（`sudo` 语义：用自己的密码提权）；
    /// 若要以别的账户提权（`su` 语义），传目标用户名。
    #[schema(example = "alice")]
    pub username: String,
}

/// `POST /api/v1/auth/start` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthStartResp {
    /// 短生命周期的**认证会话** id，用于把多轮 respond 串起来。
    ///
    /// 注意：这不是登录 token——它只代表「一次进行中的 PAM 对话」，认证成功后即作废。
    /// 服务端应给它一个短超时（分钟级），超时后返回 [`crate::ErrorCode::NotFound`]。
    #[schema(example = "0f3c1a9e7b2d4f60")]
    pub session: String,
    /// 本轮需要用户处理的提示，可能为空数组（PAM 模块无需交互时）。
    #[serde(default)]
    pub prompts: Vec<Prompt>,
}

/// 把 JSON 字符串反序列化成自动擦除的 [`Zeroizing<String>`]。
///
/// serde 必然要先产出一个 `String`，这里立刻把它 move 进 [`Zeroizing`]——
/// 临时值本身没有被拷贝，因此不存在第二份不受保护的明文。
fn deserialize_zeroizing_string<'de, D>(de: D) -> Result<Zeroizing<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(de).map(Zeroizing::new)
}

/// 一条提示的回应。
///
/// # 安全
///
/// **不实现 `Serialize` / `Clone`**，`Debug` 脱敏，`value` 由 [`Zeroizing`] 保护。见模块文档。
///
/// 下面这段**必须编译不过**——它是「凭据不可被序列化」这条约束的可执行证明：
///
/// ```compile_fail
/// use strixmaid_types::auth::PromptResponse;
/// fn leak(r: &PromptResponse) -> String {
///     serde_json::to_string(r).unwrap()
/// }
/// ```
#[derive(Deserialize, ToSchema)]
pub struct PromptResponse {
    /// 对应的 [`Prompt::id`]。
    #[schema(example = 0)]
    pub id: u32,
    /// 用户输入的原文，**drop 时自动 zeroize**。
    ///
    /// # ⚠️ 敏感数据
    ///
    /// 当 [`Prompt::style`] 为 [`PromptStyle::Prompt`] 时这就是**明文密码**。
    ///
    /// - **禁止进日志**，包括 `debug` / `trace` 级——注意 `Zeroizing` 自己的 `Debug`
    ///   会原样打印内层明文，所以外层结构的 `Debug` 必须手写脱敏；
    /// - **禁止入库**、禁止进审计记录、禁止出现在错误 `message` / `detail` 里；
    /// - 往下传时 **move**，不要 `clone()` 出一份不受保护的拷贝；
    /// - 不要 `to_string()` / `as_str().to_owned()`——那会复制出一份逃出 `Zeroizing` 的明文。
    ///
    /// JSON 上它就是普通字符串，因此 schema 标注为 `String`。
    #[serde(deserialize_with = "deserialize_zeroizing_string")]
    #[schema(value_type = String, example = "<sensitive>")]
    pub value: Zeroizing<String>,
}

impl std::fmt::Debug for PromptResponse {
    /// 手写实现：绝不打印 `value`。derive 会把密码写进日志。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptResponse")
            .field("id", &self.id)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// `POST /api/v1/auth/respond` 与 `POST /api/v1/auth/elevate/respond` 的请求体。
///
/// # 安全
///
/// 内含明文凭据，**不实现 `Serialize` / `Clone`**。`Debug` 因 [`PromptResponse`] 已脱敏而安全，
/// 但仍不建议整体打日志。`responses` 里每一项的 `value` 都会在 drop 时自动擦除。
/// 见模块文档。
#[derive(Debug, Deserialize, ToSchema)]
pub struct AuthRespondReq {
    /// [`AuthStartResp::session`] 或上一轮 [`AuthOutcome::MorePrompts`] 给出的会话 id。
    #[schema(example = "0f3c1a9e7b2d4f60")]
    pub session: String,
    /// 对本轮 `prompts` 中所有 [`PromptStyle::needs_input`] 项的回应，顺序无关。
    #[serde(default)]
    pub responses: Vec<PromptResponse>,
}

/// 认证成功后返回的用户身份。
///
/// 字段与 helper 的 `AuthOk { uid, gid, username, groups }`（`docs/design.md` §10）一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthUser {
    /// 系统 uid。
    #[schema(example = 1000)]
    pub uid: u32,
    /// 主组 gid。
    #[schema(example = 1000)]
    pub gid: u32,
    /// 系统用户名。
    #[schema(example = "alice")]
    pub username: String,
    /// 所属组名列表（含主组）。用于前端判断「不在 `systemd-journal` / `adm` 组」这类提示。
    #[serde(default)]
    pub groups: Vec<String>,
}

/// `POST /api/v1/auth/respond` 与 `POST /api/v1/auth/elevate/respond` 的**成功**响应体。
///
/// 用内部判别字段 `status`（而不是 untagged）：
///
/// ```jsonc
/// { "status": "complete", "token": "...", "user": { ... } }
/// { "status": "more",     "session": "...", "prompts": [ ... ] }
/// ```
///
/// 理由有两条，都指向 OpenAPI 这个 P0 硬要求：
///
/// 1. untagged 生成的 `oneOf` 里各分支没有任何可判别的标记，代码生成器只能猜；
/// 2. untagged 靠「字段集互不相交」区分分支，将来任一分支加字段都可能**静默**匹配到错分支。
///
/// 生成的 schema 里每个分支都把 `status` 声明为**必填的单值枚举**
/// （`{"status": {"type": "string", "enum": ["complete"]}}`），这是 JSON Schema 原生的
/// 判别方式，各主流生成器都认。这里没有用 OpenAPI 的 `discriminator` 关键字：
/// utoipa 5.5 只在 `#[serde(untagged)]` + 单字段 `$ref` 变体上才生成它，
/// 而那恰好又回到了上面两个问题里。单值枚举比 `discriminator` 更严格——
/// 校验器会真的拒绝 `status` 与内容不匹配的响应，`discriminator` 只是个提示。
///
/// **失败不在这个 union 里**：认证失败是 HTTP 401 + [`crate::ApiError`]（`code` 为
/// [`crate::ErrorCode::Unauthenticated`]），走与其它错误完全相同的通道。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuthOutcome {
    /// 认证完成。HTTP 200。
    Complete {
        /// 登录 token（Bearer）。
        ///
        /// # ⚠️ 敏感
        ///
        /// 服务端**只存它的 hash**（`sessions.id`），绝不落明文；同样禁止进日志。
        /// 提权成功时该字段回传原 token 或轮换后的新 token，由服务端决定。
        #[schema(example = "smt_9d1f...")]
        token: String,
        /// 认证到的系统身份。
        user: AuthUser,
    },
    /// PAM 还要继续追问（2FA、改密码……）。HTTP 200，用同一个 `session` 再次 respond。
    More {
        /// 继续使用的认证会话 id（可能与上一轮相同）。
        session: String,
        /// 新一轮提示，`id` 重新编号。
        prompts: Vec<Prompt>,
    },
}

/// `GET /api/v1/auth/session` 的响应体：当前浏览器会话在**当前节点**上的认证状态。
///
/// 字段对应 `docs/design.md` §8 的 `sessions` + `node_sessions` 两张表。
/// MVP 里 `node_sessions` 永远只有 `local` 一行，但类型从第一天起就带 `node`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SessionInfo {
    /// 节点 id。MVP 恒为 `"local"`。
    #[schema(example = "local")]
    pub node: String,
    /// 已认证的系统 uid。
    #[schema(example = 1000)]
    pub uid: u32,
    /// 已认证的系统用户名。
    #[schema(example = "alice")]
    pub username: String,
    /// 所属组名列表。
    #[serde(default)]
    pub groups: Vec<String>,
    /// 是否已启用管理访问（admin worker 已就绪）。
    pub elevated: bool,
    /// 提权发生的时刻；`elevated == false` 时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elevated_ts: Option<i64>,
    /// 本节点上完成认证的时刻。
    #[schema(example = 1_756_252_800_i64)]
    pub authed_ts: i64,
    /// 浏览器会话的创建时刻（`sessions.created_at`）。
    pub created_ts: i64,
    /// 最近一次活跃时刻，用于空闲超时。
    pub last_active_ts: i64,
    /// 登录时记录的 User-Agent；未采集到则为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    /// 登录时记录的来源地址；经反向代理时可能不准或为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
}

// ===========================================================================
// 提权资格
// ===========================================================================

/// 允许提权的默认组（`session.elevate_groups` 的缺省值）。
///
/// Debian 系是 `sudo`，RHEL / Arch 系是 `wheel`，老 Ubuntu 与 macOS 是 `admin`。
/// 三者都列上，一份默认值覆盖常见发行版与开发机。
pub const DEFAULT_ELEVATE_GROUPS: &[&str] = &["sudo", "wheel", "admin"];

/// 这个用户是否有资格提权（成为 admin worker 的属主）。
///
/// # 为什么放在 types 里
///
/// `roadmap/01-worker-execution.md` §4.8 要求这条规则在**两处**执行：
/// helper 内部（权威，它才是持有 root 的组件）与 `elevate_start`（提前拒绝，
/// 省掉一次 helper spawn 与一轮 PAM 对话）。helper 是独立二进制、只依赖本 crate，
/// 因此判断逻辑必须落在这里——两边 `use` 同一个函数，才不会出现
/// 「UI 说你能提权、helper 说不行」这种最难查的不一致。
///
/// # 规则
///
/// uid 0 本来就是 root，无条件放行；其余看是否属于 `elevate_groups` 之一。
///
/// 这**不是**自建 RBAC（`design.md` §5.1 明确不做）：它把「谁能成为 root」
/// 交给系统的组策略，与 `sudo` 的默认配置、与 Cockpit 的管理访问模型一致。
/// 更严格的做法是在 worker 内 `sudo -n -v` 去问 sudoers（能覆盖非基于组的规则），
/// 那依赖 `sudo` 存在，留作后续选项。
pub fn may_elevate(uid: u32, groups: &[String], elevate_groups: &[String]) -> bool {
    uid == 0 || groups.iter().any(|g| elevate_groups.contains(g))
}

#[cfg(test)]
mod elevate_tests {
    use super::*;

    fn g(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn 按组放行() {
        let allow = g(DEFAULT_ELEVATE_GROUPS);
        assert!(may_elevate(1000, &g(&["alice", "sudo"]), &allow));
        assert!(may_elevate(1000, &g(&["alice", "wheel"]), &allow));
        assert!(may_elevate(501, &g(&["staff", "admin"]), &allow), "macOS 的 admin 组");
        assert!(!may_elevate(1000, &g(&["alice", "users"]), &allow));
        assert!(!may_elevate(1000, &[], &allow), "没有任何组");
    }

    #[test]
    fn root_无条件放行() {
        assert!(may_elevate(0, &[], &g(DEFAULT_ELEVATE_GROUPS)));
        assert!(may_elevate(0, &g(&["root"]), &[]), "组列表为空也放行");
    }

    #[test]
    fn 空的允许列表等于禁止提权() {
        // 配置成空列表是「谁都不许提权」的合法表达，不能被理解成「不限制」
        assert!(!may_elevate(1000, &g(&["sudo", "wheel", "admin"]), &[]));
    }

    #[test]
    fn 组名精确匹配不做前缀或大小写宽容() {
        let allow = g(&["sudo"]);
        assert!(!may_elevate(1000, &g(&["sudoers"]), &allow), "不能前缀匹配");
        assert!(!may_elevate(1000, &g(&["SUDO"]), &allow), "Unix 组名区分大小写");
        assert!(!may_elevate(1000, &g(&["nosudo"]), &allow));
    }
}
