//! Сквозная проверка туннеля: настоящий сервер, настоящий iroh, настоящий
//! WebSocket поверх него.
//!
//! Всё живёт одним сценарием на одном туннеле, и это не лень: каждый endpoint
//! поднимает свои сокеты и здоровается с координатором, поэтому четыре теста
//! рядом мешали друг другу и подключение переставало укладываться в срок. Один
//! хозяин с одной трубой — ровно то, что происходит в жизни.
//!
//! Соединяемся по известным адресам, а не по тикету: тикет требует похода к
//! координатору в интернете, и тест зависел бы от сети. Сам тикет проверяет
//! отдельный тест, помеченный `ignore`.

use std::time::Duration;

use common::{ClientMessage, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Сколько ждём соединения через трубу. С запасом: под нагрузкой рукопожатие
/// QUIC и обмен адресами занимают заметное время.
const PATIENCE: Duration = Duration::from_secs(60);

/// Сколько раз пробуем подключиться, прежде чем считать это поломкой.
///
/// Рукопожатие QUIC — настоящая сетевая работа, и когда рядом идёт вся
/// остальная сборка, одна попытка может не успеть. Три подряд не успеют только
/// если сломано на самом деле: молча проглотить настоящую поломку это не даёт.
const ATTEMPTS: usize = 3;

/// Подключается к туннелю, переживая единичную неудачу под нагрузкой.
///
/// Сначала дожидается прямого адреса. Сразу после запуска его может не быть —
/// в `addr()` лежит только релей, и соединение даже с самим собой пошло бы
/// через интернет: медленно, а под нагрузкой и вовсе мимо.
async fn dial(tunnel: &null_terminal::tunnel::Tunnel) -> null_terminal::tunnel::Duplex {
    let waited = timeout(PATIENCE, async {
        while tunnel.direct_addrs() == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "прямой адрес так и не появился");

    let mut last = String::new();
    for attempt in 1..=ATTEMPTS {
        match timeout(PATIENCE, null_terminal::tunnel::connect_to(tunnel.addr())).await {
            Ok(Ok(duplex)) => return duplex,
            Ok(Err(reason)) => last = reason,
            Err(_) => last = "не уложились в срок".to_string(),
        }
        eprintln!("попытка {attempt} из {ATTEMPTS} не удалась: {last}");
    }
    panic!("не удалось подключиться через туннель за {ATTEMPTS} попытки: {last}");
}

/// Поднимает настоящий сервер чата на свободном порту.
async fn spawn_server() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, server::app()).await.unwrap();
    });
    port
}

#[tokio::test]
async fn a_whole_conversation_with_attachments_goes_through_the_tunnel() {
    let port = spawn_server().await;
    let tunnel = null_terminal::tunnel::serve(port)
        .await
        .expect("туннель не поднялся");

    // --- переписка ---

    // Адреса берём напрямую: в пределах машины координатор не нужен.
    let duplex = dial(&tunnel).await;

    // Поверх трубы — обычный WebSocket, тот же, что и по TCP.
    let (mut socket, _) = tokio_tungstenite::client_async("ws://localhost/ws", duplex)
        .await
        .expect("рукопожатие WebSocket через туннель не удалось");

    send(&mut socket, &join_as("alice")).await;
    let answer = next_message(&mut socket).await;
    assert!(
        matches!(answer, ServerMessage::Welcome { ref nickname, .. } if nickname == "alice"),
        "вместо welcome пришло {answer:?}"
    );

    // Реплика туда и обратно: труба должна держать разговор, а не только вход.
    send(&mut socket, &chat("привет через туннель")).await;
    let echoed = next_message(&mut socket).await;
    assert!(
        matches!(echoed, ServerMessage::Chat(ref message) if message.text == "привет через туннель"),
        "реплика не вернулась: {echoed:?}"
    );

    // Длинная реплика приходит несколькими кусками. Путайся при этом границы
    // кадров — разговор рвался бы ровно на длинных сообщениях.
    let long = "я".repeat(common::MAX_TEXT_CHARS - 1);
    send(&mut socket, &chat(&long)).await;
    let echoed = next_message(&mut socket).await;
    let ServerMessage::Chat(message) = echoed else {
        panic!("длинная реплика не вернулась: {echoed:?}");
    };
    assert_eq!(message.text, long, "длинная реплика пришла битой");

    // --- вложения ---

    // Вложения ходят не по сокету переписки, а отдельным HTTP-запросом. Через
    // трубу это самое хрупкое место: ответ дочитывается до закрытия потока, и
    // стоит потерять сигнал конца — картинка качалась бы вечно.
    let base = format!("iroh://{}", tunnel.ticket);
    let png = png_bytes();
    let attachment = timeout(
        PATIENCE,
        null_terminal::media::upload_any(
            base.clone(),
            "кот.png".to_string(),
            png.clone(),
            common::validate::MAX_UPLOAD_BYTES,
        ),
    )
    .await
    .expect("отправка не уложилась в срок")
    .expect("не удалось отправить вложение");

    assert_eq!(attachment.name, "кот.png");
    assert_eq!(attachment.size, png.len() as u64);

    let url = format!("{base}/media/{}", attachment.id);
    let downloaded = timeout(PATIENCE, null_terminal::media::fetch_any(url))
        .await
        .expect("скачивание не уложилось в срок")
        .expect("не удалось скачать вложение");
    assert_eq!(downloaded, png, "вложение вернулось не таким, каким ушло");

    // --- список комнат ---

    // Тоже отдельный запрос, и нужен он ровно тогда, когда человеку прислали
    // тикет: на экране входа, до всякого разговора.
    let bytes = timeout(
        PATIENCE,
        null_terminal::media::fetch_any(format!("{base}/rooms")),
    )
    .await
    .expect("запрос комнат не уложился в срок")
    .expect("не удалось спросить комнаты");
    let rooms: Vec<common::RoomSummary> =
        serde_json::from_slice(&bytes).expect("ответ — не список комнат");
    assert_eq!(rooms.len(), 1, "видно должно быть ровно одну комнату");
    assert_eq!(rooms[0].name, "general");
    assert_eq!(rooms[0].users, 1, "в комнате сидит alice");
}

/// Настоящий путь через интернет: подключение по одному лишь тикету, без
/// адресов. Нужен доступ к координатору, поэтому по умолчанию не запускается.
///
/// Запуск: `cargo test -p tui --test tunnel -- --ignored`
#[tokio::test]
#[ignore = "нужен интернет: тикет разрешается через координатор"]
async fn a_ticket_alone_is_enough_over_the_internet() {
    let port = spawn_server().await;
    let tunnel = null_terminal::tunnel::serve(port)
        .await
        .expect("туннель не поднялся");

    let duplex = timeout(PATIENCE, null_terminal::tunnel::connect(&tunnel.ticket))
        .await
        .expect("подключение по тикету не уложилось в срок")
        .expect("не удалось подключиться по тикету");
    let (mut socket, _) = tokio_tungstenite::client_async("ws://localhost/ws", duplex)
        .await
        .expect("рукопожатие WebSocket через туннель не удалось");

    send(&mut socket, &join_as("alice")).await;
    assert!(matches!(
        next_message(&mut socket).await,
        ServerMessage::Welcome { .. }
    ));
}

fn join_as(nickname: &str) -> ClientMessage {
    ClientMessage::Join {
        nickname: nickname.into(),
        room: "general".into(),
    }
}

fn chat(text: &str) -> ClientMessage {
    ClientMessage::Chat {
        text: text.into(),
        attachment: None,
        reply_to: None,
    }
}

async fn send<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>, message: &ClientMessage)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    socket
        .send(WsMessage::text(serde_json::to_string(message).unwrap()))
        .await
        .expect("не удалось отправить через туннель");
}

/// Ждёт следующее прикладное сообщение, пропуская служебные кадры.
async fn next_message<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>) -> ServerMessage
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = timeout(Duration::from_secs(20), socket.next())
            .await
            .expect("сервер молчит 20 секунд")
            .expect("соединение закрылось")
            .unwrap();
        match frame {
            WsMessage::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("неожиданный кадр: {other:?}"),
        }
    }
}

/// Мельчайший настоящий png: сервер определяет тип по содержимому, поэтому
/// подсунуть произвольные байты нельзя.
fn png_bytes() -> Vec<u8> {
    let image = image::RgbImage::from_pixel(2, 2, image::Rgb([200, 120, 80]));
    let mut bytes = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("png не собрался");
    bytes.into_inner()
}
