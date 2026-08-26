//! Проверка, что сервер отдаёт веб-клиент.
//!
//! Запрос идёт по сырому HTTP: так проверяется реальный ответ целиком, вместе
//! со статусом и заголовками, а не только содержимое файлов.

use std::{net::SocketAddr, sync::Arc};

use server::{Hub, HubConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

async fn spawn() -> SocketAddr {
    let hub = Arc::new(Hub::new(HubConfig::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::app_with_hub(hub);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn get(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn index_page_is_served() {
    let addr = spawn().await;

    let response = get(addr, "/").await;

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:.60}");
    assert!(response.contains("text/html"));
    // Страница должна тянуть свои же файлы с того же адреса — тогда телефон,
    // открывший http://192.168.x.x:8080, получит рабочий клиент без правок.
    assert!(response.contains(r#"href="/style.css""#));
    assert!(response.contains(r#"src="/app.js""#));
}

#[tokio::test]
async fn script_and_styles_have_correct_content_types() {
    let addr = spawn().await;

    let script = get(addr, "/app.js").await;
    assert!(script.starts_with("HTTP/1.1 200 OK"), "{script:.60}");
    // С неверным типом браузер откажется исполнять файл.
    assert!(script.contains("text/javascript"), "неверный content-type");
    assert!(script.contains("WebSocket"));

    let styles = get(addr, "/style.css").await;
    assert!(styles.starts_with("HTTP/1.1 200 OK"), "{styles:.60}");
    assert!(styles.contains("text/css"), "неверный content-type");
}

#[tokio::test]
async fn unknown_path_is_not_found() {
    let addr = spawn().await;

    let response = get(addr, "/../secrets").await;

    assert!(response.starts_with("HTTP/1.1 404"), "{response:.60}");
}

#[tokio::test]
async fn app_can_be_installed_on_a_phone() {
    let addr = spawn().await;

    let manifest = get(addr, "/manifest.webmanifest").await;
    assert!(manifest.starts_with("HTTP/1.1 200 OK"), "{manifest:.60}");
    assert!(manifest.contains("application/manifest+json"));
    assert!(manifest.contains("\"display\": \"standalone\""));

    let icon = get(addr, "/icon.svg").await;
    assert!(icon.contains("image/svg+xml"), "{icon:.120}");

    let worker = get(addr, "/sw.js").await;
    assert!(worker.contains("text/javascript"), "{worker:.120}");
    // Без этого заголовка обработчик управляет только своим каталогом,
    // и установка на домашний экран не предлагается.
    assert!(worker.to_lowercase().contains("service-worker-allowed: /"));

    let page = get(addr, "/").await;
    assert!(page.contains(r#"rel="manifest""#));
}
