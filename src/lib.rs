pub mod api;
pub mod auth;
pub mod batches;
pub mod config;
pub mod db;
pub mod dns;
pub mod domains;
pub mod error;
pub mod mailer;
pub mod stalwart;
pub mod template;
pub mod unsubscribe;
pub mod util;
pub mod web;
pub mod worker;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::SqlitePool,
    pub cfg: Arc<config::Config>,
    pub mailer: Arc<mailer::Mailer>,
    pub stalwart: Option<Arc<stalwart::Stalwart>>,
    pub dns: Arc<dns::Verifier>,
}

impl AppState {
    pub async fn new(cfg: config::Config) -> anyhow::Result<Self> {
        let db = db::connect(&cfg.database_url).await?;
        let mailer = mailer::Mailer::new(&cfg)?;
        let stalwart = match (&cfg.stalwart_api_url, &cfg.stalwart_api_token) {
            (Some(url), Some(token)) => Some(Arc::new(stalwart::Stalwart::new(url, token)?)),
            _ => None,
        };
        Ok(AppState {
            db,
            cfg: Arc::new(cfg),
            mailer: Arc::new(mailer),
            stalwart,
            dns: Arc::new(dns::Verifier::new()),
        })
    }
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .nest("/api/v1", api::router())
        .merge(web::router())
        .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests;
