//! `/api/v1/metrics/*`（design.md §9.1「指标」组）。
//!
//! 三个端点都是 [`strixmaid_core::metrics::MetricsEngine`] 的薄壳，DTO 全部来自
//! `strixmaid_types::metrics`。状态是独立的 [`MetricsState`]（而不是挂在 `AppState` 上），
//! 用 [`router`] 一次 `merge` 进 `/api/v1`——`router` 对目标 Router 的状态类型是泛型的，
//! 因此可以直接 `routes::api_v1().merge(metrics::router(state))`。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use strixmaid_core::metrics::MetricsEngine;
use strixmaid_types::ApiError;
use strixmaid_types::metrics::{
    MetricQuery, MetricQueryResp, MetricSnapshot, SeriesListQuery, SeriesMeta,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiResult;

/// 指标路由的状态：一个引擎句柄。
#[derive(Clone)]
pub struct MetricsState {
    engine: MetricsEngine,
}

impl MetricsState {
    /// 包一层。
    pub fn new(engine: MetricsEngine) -> Self {
        MetricsState { engine }
    }
}

/// `GET /metrics/series` / `/metrics/query` / `/metrics/current`，状态已注入。
///
/// 返回值对外层 Router 的状态类型 `S` 泛型，可 `merge` 进任何 `OpenApiRouter<S>`。
pub fn router<S>(state: Arc<MetricsState>) -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    OpenApiRouter::new()
        .routes(routes!(series))
        .routes(routes!(query))
        .routes(routes!(current))
        .with_state(state)
}

/// 可用 series 列表
///
/// 以 `series` 表为准：包含历史上出现过、现在已不再采集的序列（例如已拔掉的磁盘）。
/// 未配置落盘时来自内存环，此时 `id` 为 0。
#[utoipa::path(
    get,
    path = "/metrics/series",
    tag = "metrics",
    params(SeriesListQuery),
    responses(
        (status = 200, description = "序列列表，按 metric、labels 排序", body = Vec<SeriesMeta>),
        (status = 500, description = "读取 series 表失败", body = ApiError),
    ),
)]
pub async fn series(
    State(state): State<Arc<MetricsState>>,
    Query(q): Query<SeriesListQuery>,
) -> ApiResult<Json<Vec<SeriesMeta>>> {
    Ok(Json(
        state
            .engine
            .series_list(q.node.as_deref(), q.prefix.as_deref())
            .await?,
    ))
}

/// 查询时序（自动选层）
///
/// 跨度落在内存环覆盖范围内且 `step < 60` 时直接从环出（`layer = live`，`step` 取整到
/// 采集间隔的倍数）；否则按 design.md §7.2 的三条规则在五个落盘层里选最粗的合适层。
/// 区间左闭右开 `[from, to)`；请求里存在但查无此序列的项不会出现在结果里。
#[utoipa::path(
    get,
    path = "/metrics/query",
    tag = "metrics",
    params(MetricQuery),
    responses(
        (status = 200, description = "查询结果", body = MetricQueryResp),
        (status = 400, description = "参数不合法：to < from、series 为空或格式错误", body = ApiError),
        (status = 500, description = "数据库查询失败", body = ApiError),
    ),
)]
pub async fn query(
    State(state): State<Arc<MetricsState>>,
    Query(q): Query<MetricQuery>,
) -> ApiResult<Json<MetricQueryResp>> {
    Ok(Json(state.engine.query(&q).await?))
}

/// 实时快照
///
/// 最近一轮采集的全部瞬时值，与 WS `metrics.live` 频道推送的 payload 相同。
/// 进程刚启动、第一轮尚未完成时 `values` 为空。
#[utoipa::path(
    get,
    path = "/metrics/current",
    tag = "metrics",
    responses(
        (status = 200, description = "最新一轮快照", body = MetricSnapshot),
    ),
)]
pub async fn current(State(state): State<Arc<MetricsState>>) -> Json<MetricSnapshot> {
    Json((*state.engine.snapshot()).clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use strixmaid_core::metrics::{CollectError, Collector, Sample, SchedulerConfig};
    use tower::ServiceExt as _;

    struct ConstCollector;

    impl Collector for ConstCollector {
        fn name(&self) -> &'static str {
            "const"
        }

        fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
            Ok(vec![
                Sample::new("test.value", 42.0),
                Sample::labeled("test.dev", "dev", "sda", 1.0),
            ])
        }
    }

    async fn engine() -> MetricsEngine {
        let engine = MetricsEngine::start_with(
            SchedulerConfig {
                interval: Duration::from_millis(50),
                ring_secs: 3600,
                node: "local".into(),
                collect_timeout: Duration::from_secs(5),
            },
            None,
            vec![Box::new(ConstCollector)],
        );
        let mut rx = engine.subscribe();
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("5s 内应有第一轮")
            .unwrap();
        engine
    }

    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn 三个端点与_openapi_收集() {
        let engine = engine().await;
        let (app, openapi) =
            router::<()>(Arc::new(MetricsState::new(engine.clone()))).split_for_parts();
        for path in ["/metrics/series", "/metrics/query", "/metrics/current"] {
            assert!(openapi.paths.paths.contains_key(path), "OpenAPI 缺 {path}");
        }

        let (status, body) = get(&app, "/metrics/current").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["values"].as_array().unwrap().len(), 2);
        assert_eq!(body["values"][0]["metric"], "test.value");

        let (status, body) = get(&app, "/metrics/series?prefix=test.d").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["labels"], "dev=sda");

        let now = strixmaid_core::store::now_unix();
        let (status, body) = get(
            &app,
            &format!(
                "/metrics/query?series=test.value,test.dev{{dev=sda}}&from={}&to={}&step=1",
                now - 60,
                now + 1
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["layer"], "live");
        assert_eq!(body["series"].as_array().unwrap().len(), 2);
        assert!(!body["series"][0]["points"].as_array().unwrap().is_empty());

        let (status, body) = get(&app, "/metrics/query?series=test.value&from=10&to=5").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "invalid_request");
        engine.stop().await;
    }
}
