use std::{net::SocketAddr, time::Duration};

use anyhow::Context;
use axum::{
    error_handling::HandleErrorLayer,
    extract::FromRef,
    http::{HeaderValue, Method, StatusCode},
    routing::{delete, get, patch, post},
    serve, BoxError, Router,
};
use axum_extra::extract::cookie::Key;
use getset::Getters;
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::{net::TcpListener, signal};
use tower::{buffer::BufferLayer, limit::RateLimitLayer, ServiceBuilder};
use tower_http::{cors::CorsLayer, timeout::TimeoutLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    controllers::{
        delete_me, delete_user, get_all_users, get_me, get_user, health_check, login, logout,
        refresh_session_by_body, refresh_session_by_cookie, register, revoke_all_sessions,
        revoke_my_session, revoke_user_session, update_me, update_user,
    },
    utils::{AppConfig, AppResult, DatabaseConfig},
};

pub async fn run_application(config: AppConfig) -> AppResult<()> {
    init_tracing()?;

    let db_pool = create_connection_pool(config.database()).await?;

    let app = create_router(db_pool, config.clone());

    let address = SocketAddr::new(config.server().host().parse()?, *config.server().port());

    let listener = TcpListener::bind(address).await?;

    tracing::info!("Listening on {}:{}", address.ip(), address.port());
    serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("Failed to start server")
}

fn init_tracing() -> AppResult<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        format!(
            "{}=info,tower_http=info,axum=debug",
            env!("CARGO_CRATE_NAME")
        )
        .into()
    });

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_timer(tracing_subscriber::fmt::time::UtcTime::rfc_3339());

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .try_init()
        .context("Failed to initialize tracing")
}

pub async fn create_connection_pool(config: &DatabaseConfig) -> AppResult<PgPool> {
    PgPoolOptions::new()
        .max_connections(*config.get_max_connections())
        .min_connections(*config.get_min_connections())
        .acquire_timeout(Duration::from_secs(*config.get_acquire_timeout_secs()))
        .connect_with(config.to_pg_connect_options())
        .await
        .context("Failed to create database connection pool")
}

#[derive(Debug, Clone, Getters)]
pub struct AppState {
    #[getset(get = "pub with_prefix")]
    db_pool: PgPool,
    #[getset(get = "pub with_prefix")]
    config: AppConfig,
    #[getset(get = "pub with_prefix")]
    key: Key,
}

impl FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Self {
        state.key.to_owned()
    }
}

pub fn create_router(db_pool: PgPool, config: AppConfig) -> Router {
    let key = Key::from(config.server().get_cookie_secret().as_bytes());
    let state = AppState {
        db_pool,
        config,
        key,
    };
    let timeout = Duration::from_secs(*state.config.server().get_timeout_in_secs());
    let origins: Vec<HeaderValue> = state
        .config
        .server()
        .get_origins()
        .split(',')
        .map(str::trim)
        .filter_map(|s| s.parse::<HeaderValue>().ok())
        .collect();

    let cors_layer = CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::DELETE,
            Method::PUT,
            Method::PATCH,
        ])
        .allow_credentials(true);

    let trace_layer = TraceLayer::new_for_http();

    let timeout_layer = TimeoutLayer::new(timeout);

    // See https://github.com/tokio-rs/axum/discussions/987
    let rate_limit_layer = ServiceBuilder::new()
        .layer(HandleErrorLayer::new(|err: BoxError| async move {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Unhandled error: {}", err),
            )
        }))
        .layer(BufferLayer::new(1024))
        .layer(RateLimitLayer::new(
            *state.config.server().get_rate_limit_burst(),
            Duration::from_secs(*state.config.server().get_rate_limit_per_secs()),
        ));

    let users_router = Router::new()
        .route("/register", post(register))
        .route("/", get(get_all_users))
        .route("/me", get(get_me))
        .route("/:id", get(get_user))
        .route("/:id", patch(update_user))
        .route("/me", patch(update_me))
        .route("/:id", delete(delete_user))
        .route("/me", delete(delete_me));

    let auth_router = Router::new()
        .route("/login", post(login))
        .route("/logout", post(logout));

    let session_router = Router::new()
        .route("/refresh-cookie", post(refresh_session_by_cookie))
        .route("/refresh", post(refresh_session_by_body))
        .route("/current", patch(revoke_my_session))
        .route("/:id", patch(revoke_user_session))
        .route("/", patch(revoke_all_sessions));

    Router::new()
        .route("/", get(health_check))
        .nest("/users", users_router)
        .nest("/auth", auth_router)
        .nest("/sessions", session_router)
        .layer(trace_layer)
        .layer(cors_layer)
        .layer(timeout_layer)
        .layer(rate_limit_layer)
        .with_state(state)
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C signal handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}