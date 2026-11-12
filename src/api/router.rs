//! Router assembly: public probes, metrics, `/api/v1` (auth + rate limit)
//! and `/internal/*` (api-key).

use axum::extract::DefaultBodyLimit;
use axum::middleware;
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::catch_panic::CatchPanicLayer;

use crate::api::handlers;
use crate::api::middleware as mw;
use crate::config;
use crate::state::AppState;

pub fn build_router(state: AppState) -> Router {
    // /api/v1/auth/* — public endpoints.
    let auth_routes = Router::new()
        .route("/auth/login", post(handlers::auth::login))
        .route("/auth/refresh", post(handlers::auth::refresh))
        .route("/auth/logout", post(handlers::auth::logout));

    // /api/v1/* — authenticated endpoints.
    let protected = Router::new()
        .route(
            "/files",
            post(handlers::upload::upload_file).get(handlers::list::list_files),
        )
        .route(
            "/files/:id",
            get(handlers::download::download_file).delete(handlers::delete::delete_file),
        )
        .route("/files/:id/meta", get(handlers::metadata::file_meta))
        .route("/files/:id/range", get(handlers::download::download_range))
        .route(
            "/tenants",
            post(handlers::tenant::create_tenant).get(handlers::tenant::list_tenants),
        )
        .route("/audit", get(handlers::admin::list_audit))
        .route("/admin/gc", post(handlers::admin::trigger_gc))
        .route("/admin/resync/:id", post(handlers::admin::resync_file))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            mw::auth::auth_middleware,
        ));

    let api_v1 = auth_routes.merge(protected);

    // Rate limiting on the whole /api/v1 subtree when enabled.
    let api_v1 = match mw::rate_limit::build_governor(&state.0.cfg.rate_limit) {
        Some(layer) => api_v1.layer(layer),
        None => api_v1,
    };

    // /internal/* — cluster replication endpoints.
    let internal = Router::new()
        .route("/chunk", post(handlers::internal::internal_put_chunk))
        .route("/chunk/:hash", get(handlers::internal::internal_get_chunk))
        .route("/manifest", post(handlers::internal::internal_put_manifest))
        .route(
            "/manifest/:id",
            delete(handlers::internal::internal_purge_manifest),
        )
        .route("/batch", post(handlers::internal::internal_batch))
        .route("/file/:id", get(handlers::internal::internal_get_file))
        .route("/file/:id", post(handlers::internal::internal_apply_file))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            mw::auth::api_key_middleware,
        ));

    let (set_request_id, propagate_request_id) = mw::request_id::build_request_id_layers();

    // H-05/M-17: /metrics exposure is an explicit, default-safe choice.
    // Loopback (default): only loopback peers. ApiKey: constant-time check.
    // Public: today's behavior + startup warning (logged at boot).
    let metrics_route = match state.0.cfg.metrics.exposure {
        config::MetricsExposure::Loopback => get(handlers::metrics::metrics).route_layer(
            middleware::from_fn_with_state(state.clone(), mw::metrics_guard::loopback_only),
        ),
        config::MetricsExposure::ApiKey => get(handlers::metrics::metrics).route_layer(
            middleware::from_fn_with_state(state.clone(), mw::metrics_guard::api_key_only),
        ),
        config::MetricsExposure::Public => {
            tracing::warn!(
                "metrics.exposure=public: /metrics is served WITHOUT authentication;                  this exposes operational metadata (tenant scale, routes, codes)"
            );
            get(handlers::metrics::metrics)
        }
    };

    Router::new()
        .route("/health", get(handlers::health::health))
        .route("/ready", get(handlers::health::ready))
        .route("/metrics", metrics_route)
        .nest("/internal", internal)
        .nest("/api/v1", api_v1)
        .layer(middleware::from_fn(mw::request_id::metrics_middleware))
        .layer(propagate_request_id)
        .layer(set_request_id)
        // H-04: a handler panic yields the standard error envelope (500,
        // code internal_error) instead of a connection reset; the panic
        // payload is never leaked.
        .layer(CatchPanicLayer::custom(mw::request_id::panic_to_500))
        .layer(DefaultBodyLimit::max(state.0.cfg.limits.max_body))
        .with_state(state)
}
