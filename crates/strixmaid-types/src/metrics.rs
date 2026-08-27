//! 指标查询（`docs/design.md` §7、§9.1「指标」组）。
//!
//! # 为什么桶里是 cnt/min/max/sum/med
//!
//! 见 `docs/design.md` §7.2、§7.3：
//!
//! - 存 **`sum` 而不是 `avg`**，是为了让逐级聚合完全精确（`sum = SUM(sum)`，无浮点累积误差）。
//!   展示时前端自己算 `avg = sum / cnt`。
//! - 存 **`med` 而不是 `p95`**：1 分钟桶按 2s 采集只有 30 个点，p95 落在「第二大的值」上，
//!   与 `max` 几乎重合、不提供独立信息；而 **`avg` 与 `med` 的差值本身就是偏斜度指标**
//!   （`avg ≈ med` → 负载平稳；`avg > med` → 平时空闲但有尖峰），这是 min/max/avg 答不出的问题。
//! - `med` 在 `m_1m` 层是真中位数，粗粒度层是 median of medians（近似，误差无方向性）。
//!
//! 展示形式是 uPlot band 系列：min–max 画半透明区间带，avg 画实线，med 画虚线。

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// 落盘的聚合层。`step` 与桶宽的对应关系见 `docs/design.md` §7.2。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
pub enum MetricLayer {
    /// 内存环形缓冲：默认 2s 一点，保留 1 小时。**不落盘**，查询近 1 小时的高分辨率曲线走这一层。
    #[serde(rename = "live")]
    Live,
    /// 桶宽 60s。保留 6h（Less）/ 1d（Normal）。
    #[serde(rename = "m_1m")]
    M1m,
    /// 桶宽 300s。保留 3d / 7d。
    #[serde(rename = "m_5m")]
    M5m,
    /// 桶宽 900s。保留 14d / 30d。
    #[serde(rename = "m_15m")]
    M15m,
    /// 桶宽 43200s。保留 90d / 90d。
    #[serde(rename = "m_12h")]
    M12h,
    /// 桶宽 86400s。保留 1y / 1y。
    #[serde(rename = "m_1d")]
    M1d,
}

impl MetricLayer {
    /// 落盘的五层，按由细到粗排列。
    ///
    /// 供 `strixmaid-core` 遍历（建表、逐级聚合、清理过期数据），
    /// 免得各处再各自抄一遍层列表。**不含** [`Self::Live`]——它只在内存里。
    pub const PERSISTED: [Self; 5] = [Self::M1m, Self::M5m, Self::M15m, Self::M12h, Self::M1d];

    /// 线格式名称，与 serde 序列化结果一致；落盘层的名称同时就是**表名**
    /// （`m_1m` / `m_5m` / …），见 `docs/design.md` §8。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::M1m => "m_1m",
            Self::M5m => "m_5m",
            Self::M15m => "m_15m",
            Self::M12h => "m_12h",
            Self::M1d => "m_1d",
        }
    }

    /// 是否落盘。[`Self::Live`] 为 `false`（内存环形缓冲，进程重启即失）。
    pub const fn is_persisted(self) -> bool {
        !matches!(self, Self::Live)
    }

    /// 对应的 SQLite 表名。[`Self::Live`] 无表，返回 `None`。
    pub const fn table_name(self) -> Option<&'static str> {
        if self.is_persisted() {
            Some(self.as_str())
        } else {
            None
        }
    }

    /// 该层的桶宽，秒。[`Self::Live`] 返回 `None`——它的间隔是可配置的采集间隔（1–60s）。
    pub const fn bucket_secs(self) -> Option<u32> {
        Some(match self {
            Self::Live => return None,
            Self::M1m => 60,
            Self::M5m => 300,
            Self::M15m => 900,
            Self::M12h => 43_200,
            Self::M1d => 86_400,
        })
    }
}

impl std::fmt::Display for MetricLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MetricLayer {
    type Err = crate::ApiError;

    /// 解析层名（`"live"` / `"m_1m"` / …）。未知取值返回
    /// [`crate::ErrorCode::InvalidRequest`]。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "live" => Self::Live,
            "m_1m" => Self::M1m,
            "m_5m" => Self::M5m,
            "m_15m" => Self::M15m,
            "m_12h" => Self::M12h,
            "m_1d" => Self::M1d,
            other => {
                return Err(crate::ApiError::invalid_request(format!(
                    "未知的指标层 {other}"
                )));
            }
        })
    }
}

/// 保留期预设（`docs/design.md` §7.2、§7.4）。只提供两套，不做逐层自定义。
///
/// **本枚举是 API 契约的一部分，归 `strixmaid-types` 所有**；
/// 「哪一层在哪个预设下留多久」那张元数据表归 `strixmaid-core::store`，并以本枚举
/// 与 [`MetricLayer`] 作为键——保留期表只存在一份，避免三处定义各自漂移。
/// `strixmaid-core::config` 直接 import 本类型，不另行定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RetentionPreset {
    /// 省空间：约 35MB / 200 series。`m_1m` 只留 6h，`m_1d` 留 1y。
    Less,
    /// **默认**。约 100MB / 200 series（含索引）。`m_1m` 留 1d，`m_1d` 留 1y。
    #[default]
    Normal,
}

impl RetentionPreset {
    /// 线格式名称，与 serde 序列化结果一致。也用于 TOML 配置与 `STRIXMAID_*` 环境变量。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Less => "less",
            Self::Normal => "normal",
        }
    }
}

impl std::fmt::Display for RetentionPreset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RetentionPreset {
    type Err = crate::ApiError;

    /// 解析预设名，大小写不敏感（配置文件与命令行都可能写成 `Normal`）。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "less" => Self::Less,
            "normal" => Self::Normal,
            other => {
                return Err(crate::ApiError::invalid_request(format!(
                    "未知的保留期预设 {other}，可选 less / normal"
                )));
            }
        })
    }
}

/// 一条时间序列的元信息，对应 `docs/design.md` §8 的 `series` 表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SeriesMeta {
    /// `series.id`，数值主键。查询时用它比用 `metric+labels` 拼字符串快。
    #[schema(example = 42_i64)]
    pub id: i64,
    /// 节点 id：`"local"` 或 Agent 的 uuid。
    #[schema(example = "local")]
    pub node: String,
    /// 指标名，点分层级。
    #[schema(example = "disk.read_bytes")]
    pub metric: String,
    /// 标签，形如 `k=v` 并**按键排序后以 `,` 拼接**；无标签时为空串（不是 `null`）。
    /// 这是 `series` 表 `UNIQUE(node, metric, labels)` 的一部分，拼法必须稳定。
    #[schema(example = "dev=sda")]
    pub labels: String,
    /// 单位，供前端选择格式化方式：`"bytes"` / `"bytes/s"` / `"percent"` / `"count"` /
    /// `"seconds"` / `"iops"`。未标注时为 `None`，按裸数值展示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "bytes/s")]
    pub unit: Option<String>,
}

/// 一个聚合桶。五个字段的语义与 `docs/design.md` §7.2 的表结构逐一对应。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MetricPoint {
    /// **桶起始**时刻，unix 秒（不是桶中点，也不是结束时刻）。
    #[schema(example = 1_756_252_800_i64)]
    pub ts: i64,
    /// 该桶内的实际采样点数。**用于缺失检测**：明显小于 `bucket_secs / 采集间隔` 说明有丢采。
    /// 也是算 `avg = sum / cnt` 的分母。
    #[schema(example = 30)]
    pub cnt: u32,
    /// 桶内最小值。
    #[schema(example = 1.2)]
    pub min: f64,
    /// 桶内最大值。
    #[schema(example = 88.4)]
    pub max: f64,
    /// 桶内**总和**，不是平均值。`avg = sum / cnt`，由前端计算。
    #[schema(example = 620.5)]
    pub sum: f64,
    /// 桶内中位数。`m_1m` 层为真中位数，更粗的层为 median of medians（近似值）。
    #[schema(example = 18.7)]
    pub med: f64,
}

impl MetricPoint {
    /// 平均值。`cnt == 0` 时返回 `None`（空桶不应出现，但反序列化的数据不可信）。
    pub fn avg(&self) -> Option<f64> {
        (self.cnt > 0).then(|| self.sum / f64::from(self.cnt))
    }
}

/// 一条序列的查询结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MetricSeries {
    /// 序列元信息。
    pub meta: SeriesMeta,
    /// 桶数组，按 `ts` 升序。
    ///
    /// **有洞就是真的没数据**（服务未启动 / 该期间已过保留期），服务端不做零填充，
    /// 前端应断线而不是连成直线。
    #[serde(default)]
    pub points: Vec<MetricPoint>,
}

/// `GET /api/v1/metrics/query` 的查询参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct MetricQuery {
    /// 要查的序列，**逗号分隔**。每项为 `series.id`（数字）或 `metric{labels}` 形式，
    /// 如 `cpu.usage` / `disk.read_bytes{dev=sda}`。
    ///
    /// 用逗号而非重复参数，是为了让 axum 的 `Query<T>` 无需额外依赖即可解析。
    #[param(example = "cpu.usage,mem.available")]
    pub series: String,
    /// 起始时刻（含），unix 秒。
    #[param(example = 1_756_249_200_i64)]
    pub from: i64,
    /// 结束时刻（含），unix 秒。必须 >= `from`，否则返回
    /// [`crate::ErrorCode::InvalidRequest`]。
    #[param(example = 1_756_252_800_i64)]
    pub to: i64,
    /// 期望的点间隔，秒。服务端据此**自动选层**：挑选桶宽 <= `step` 的最粗一层，
    /// 保证返回点数可控。缺省时按 `(to - from) / 目标点数` 推算。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = 60)]
    pub step: Option<u32>,
}

/// `GET /api/v1/metrics/query` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MetricQueryResp {
    /// 实际使用的起始时刻（可能被对齐到桶边界，因此不一定等于请求的 `from`）。
    pub from: i64,
    /// 实际使用的结束时刻。
    pub to: i64,
    /// 实际选中的层。前端应展示它——用户需要知道自己看的是 1 分钟还是 1 天粒度。
    pub layer: MetricLayer,
    /// 实际点间隔，秒，等于 `layer.bucket_secs()`（`live` 层为实际采集间隔）。
    #[schema(example = 60)]
    pub step: u32,
    /// 各序列结果。请求里存在但库中查无此序列的，**不会**出现在这里（不是返回空 points）。
    #[serde(default)]
    pub series: Vec<MetricSeries>,
}

/// `GET /api/v1/metrics/current` 的响应体，同时也是 WS `metrics.live` 频道推送的 payload。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MetricSnapshot {
    /// 采样时刻。
    pub ts: i64,
    /// 本次采样的全部瞬时值。
    #[serde(default)]
    pub values: Vec<MetricValue>,
}

/// 一个瞬时值。
///
/// 这里刻意**不带 `series.id`**：快照可能包含尚未落库、因而还没有 id 的序列。
/// 前端按 `metric` + `labels` 与 [`SeriesMeta`] 对齐。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct MetricValue {
    /// 指标名，同 [`SeriesMeta::metric`]。
    #[schema(example = "cpu.usage")]
    pub metric: String,
    /// 标签，拼法同 [`SeriesMeta::labels`]；无标签为空串。
    #[serde(default)]
    #[schema(example = "")]
    pub labels: String,
    /// 瞬时值。速率类指标（`*_bytes` / `*.iops`）已由服务端做过差分，
    /// 单位见 [`Self::unit`]，前端不要再自己求差。
    #[schema(example = 23.7)]
    pub value: f64,
    /// 单位，取值同 [`SeriesMeta::unit`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "percent")]
    pub unit: Option<String>,
}

/// `GET /api/v1/metrics/series` 的查询参数：可用序列列表的过滤条件。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SeriesListQuery {
    /// 按节点过滤。缺省返回全部节点（MVP 只有 `local`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "local")]
    pub node: Option<String>,
    /// 按指标名前缀过滤，如 `disk.` 会命中 `disk.read_bytes` 等。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "disk.")]
    pub prefix: Option<String>,
}
