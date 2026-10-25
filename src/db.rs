use anyhow::Result;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

pub async fn connect(url: &str) -> Result<SqlitePool> {
    let in_memory = url.contains(":memory:");
    if !in_memory {
        if let Some(path) = url
            .strip_prefix("sqlite://")
            .or_else(|| url.strip_prefix("sqlite:"))
        {
            let path = path.split('?').next().unwrap_or(path);
            if let Some(dir) = std::path::Path::new(path).parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
        }
    }
    let mut opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    if !in_memory {
        opts = opts.journal_mode(SqliteJournalMode::Wal);
    }
    let pool = SqlitePoolOptions::new()
        // An in-memory database only exists inside one connection.
        .max_connections(if in_memory { 1 } else { 8 })
        .connect_with(opts)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
