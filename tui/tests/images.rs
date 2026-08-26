//! Показ картинки в терминале, от загрузки на сервер до пикселей.

use std::{io::Cursor, sync::Arc};

use server::{Hub, HubConfig};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn start_server() -> String {
    let hub = Arc::new(Hub::new(HubConfig::default()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::app_with_hub(hub);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x * 4) as u8, (y * 4) as u8, 128])
    });
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .unwrap();
    bytes
}

/// Загружает файл на сервер и возвращает его идентификатор.
async fn upload(base: &str, bytes: &[u8]) -> String {
    let authority = base.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(authority).await.unwrap();
    let head = format!(
        "POST /upload?name=кот.png HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        bytes.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(bytes).await.unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (_, body) = response.split_once("\r\n\r\n").unwrap();
    let attachment: serde_json::Value = serde_json::from_str(body).unwrap();
    attachment["id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn picture_travels_from_the_server_to_the_terminal() {
    let base = start_server().await;
    let id = upload(&base, &png(64, 32)).await;
    let url = format!("{base}/media/{id}");

    // Скачивание и разбор блокируют, поэтому живут в отдельном потоке —
    // ровно так же, как в самом клиенте.
    let image = tokio::task::spawn_blocking(move || {
        let bytes = tui::media::fetch(&url).expect("не удалось скачать");
        tui::media::decode(&bytes).expect("не удалось разобрать")
    })
    .await
    .unwrap();

    assert_eq!((image.width(), image.height()), (64, 32));

    // И превращается в строки полублоков: два ряда пикселей на строку.
    let lines = tui::media::to_lines(&image, 64, 32);
    assert_eq!(lines.len(), 16);
    assert_eq!(lines[0].spans.len(), 64);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_picture_is_reported_not_panicked() {
    let base = start_server().await;
    let url = format!("{base}/media/{}", uuid::Uuid::new_v4());

    let error = tokio::task::spawn_blocking(move || tui::media::fetch(&url).unwrap_err())
        .await
        .unwrap();

    // Файл мог быть вытеснен из хранилища — клиент обязан это пережить.
    assert!(error.contains("404"), "невнятная причина: {error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn picture_can_be_sent_from_the_terminal() {
    let base = start_server().await;
    let path = std::env::temp_dir().join(format!("chat-{}.png", uuid::Uuid::new_v4()));
    std::fs::write(&path, png(32, 16)).unwrap();

    let sent = path.clone();
    let attachment = tokio::task::spawn_blocking(move || tui::media::upload(&base, &sent))
        .await
        .unwrap()
        .expect("файл не загрузился");

    assert_eq!(attachment.mime, "image/png");
    assert_eq!(attachment.kind, common::AttachmentKind::Image);
    // Имя доезжает целиком: по нему человек и узнаёт файл в переписке.
    assert!(attachment.name.ends_with(".png"), "{}", attachment.name);
    std::fs::remove_file(&path).ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_file_is_refused_with_a_readable_reason() {
    let base = start_server().await;
    let path = std::env::temp_dir().join(format!("chat-{}.bin", uuid::Uuid::new_v4()));
    std::fs::write(&path, vec![0u8; common::MAX_UPLOAD_BYTES + 1]).unwrap();

    let sent = path.clone();
    let error = tokio::task::spawn_blocking(move || tui::media::upload(&base, &sent))
        .await
        .unwrap()
        .unwrap_err();

    // Отказ должен объяснять причину, а не показывать код ответа.
    assert!(error.contains("слишком большой"), "пришло: {error}");
    std::fs::remove_file(&path).ok();
}
