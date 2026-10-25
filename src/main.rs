use anyhow::{Result, bail};
use batchmail::{AppState, app, auth, config::Config, worker};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "Pemakaian:
  batchmail                          jalankan server + pengirim
  batchmail create-user EMAIL PASS   buat/ubah user dashboard";

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "batchmail=info,tower_http=warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cfg = Config::from_env()?;
    let state = AppState::new(cfg).await?;

    match args.first().map(String::as_str) {
        None | Some("serve") => serve(state).await,
        Some("create-user") => {
            let [_, email, password] = args.as_slice() else {
                bail!("{USAGE}")
            };
            auth::upsert_user(&state.db, email, password).await?;
            println!("User {email} siap dipakai.");
            Ok(())
        }
        Some(_) => bail!("{USAGE}"),
    }
}

async fn serve(state: AppState) -> Result<()> {
    if let (Some(email), Some(password)) = (&state.cfg.admin_email, &state.cfg.admin_password) {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users)")
            .fetch_one(&state.db)
            .await?;
        if !exists {
            auth::upsert_user(&state.db, email, password).await?;
            tracing::info!("admin {email} dibuat dari ADMIN_EMAIL/ADMIN_PASSWORD");
        }
    }
    match state.mailer.test_connection().await {
        Ok(()) => tracing::info!("terhubung ke Stalwart SMTP {}", state.mailer.description),
        Err(e) => tracing::warn!(
            "belum bisa terhubung ke Stalwart SMTP {}: {e}",
            state.mailer.description
        ),
    }

    tokio::spawn(worker::run(state.clone()));

    let listener = tokio::net::TcpListener::bind(&state.cfg.bind_addr).await?;
    tracing::info!("BatchMail berjalan di http://{}", state.cfg.bind_addr);
    axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
