use std::{net::SocketAddr, path::PathBuf};

use axum_server::{Handle, tls_rustls::RustlsConfig};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // По умолчанию 0.0.0.0 — чтобы телефон из той же Wi-Fi сети мог достучаться
    // до компьютера (план, п.6). Переопределяется переменной CHAT_ADDR.
    let addr: SocketAddr = std::env::var("CHAT_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;

    match tls_dir() {
        Some(dir) => serve_https(addr, dir).await,
        None => serve_http(addr).await,
    }
}

/// Каталог с сертификатом, если включён HTTPS.
///
/// Микрофон, камеру и уведомления браузеры отдают только в защищённом
/// контексте, поэтому для телефона HTTPS не роскошь, а условие работы.
fn tls_dir() -> Option<PathBuf> {
    let enabled = std::env::var("CHAT_TLS").is_ok_and(|value| !value.is_empty() && value != "0");
    enabled.then(|| {
        std::env::var("CHAT_TLS_DIR")
            .unwrap_or_else(|_| "tls".to_string())
            .into()
    })
}

async fn serve_http(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    announce("http", listener.local_addr()?);

    axum::serve(listener, server::app())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("получен Ctrl+C, останавливаюсь");
        })
        .await?;
    Ok(())
}

async fn serve_https(addr: SocketAddr, dir: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // Провайдер выбираем явно: сборка собрана без умолчательного, иначе rustls
    // притащил бы второй криптобэкенд рядом с тем, что уже использует QUIC.
    // Ошибку глотаем — она означает лишь, что провайдер уже установлен.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certificate = server::tls::ensure_certificate(&dir)?;
    let config = RustlsConfig::from_pem_file(&certificate.cert, &certificate.key).await?;
    announce("https", addr);
    tracing::info!(
        "сертификат самоподписанный: браузер один раз спросит подтверждение. \
         Файлы в {}, удалите их, чтобы выпустить заново",
        dir.display()
    );

    let handle = Handle::new();
    tokio::spawn({
        let handle = handle.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("получен Ctrl+C, останавливаюсь");
            handle.graceful_shutdown(Some(std::time::Duration::from_secs(2)));
        }
    });

    axum_server::bind_rustls(addr, config)
        .handle(handle)
        .serve(server::app().into_make_service())
        .await?;
    Ok(())
}

/// Печатает оба адреса сразу: локальный и сетевой.
///
/// Иначе первое, что делает человек с телефоном в руках, — идёт выяснять свой
/// IP через `ipconfig`.
fn announce(scheme: &str, addr: SocketAddr) {
    tracing::info!("слушаю {scheme}://localhost:{}", addr.port());
    for ip in server::tls::local_ips() {
        tracing::info!("с телефона: {scheme}://{ip}:{}", addr.port());
    }
}
