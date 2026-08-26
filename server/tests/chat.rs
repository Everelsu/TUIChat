//! Проверка комнат, ников и событий входа/выхода через настоящее соединение.
//!
//! Клиенты здесь поднимаются на tokio-tungstenite — то есть тот же путь, что у
//! websocat: сервер тестируется снаружи, а не через внутренние вызовы.

use std::{sync::Arc, time::Duration};

use common::{ChatMessage, ClientMessage, ErrorCode, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use server::{Hub, HubConfig};
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message as WsMessage};

type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn spawn_with(config: HubConfig) -> (String, Arc<Hub>) {
    let hub = Arc::new(Hub::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = server::app_with_hub(Arc::clone(&hub));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("ws://{addr}/ws"), hub)
}

async fn spawn() -> String {
    spawn_with(HubConfig::default()).await.0
}

async fn connect(url: &str) -> Client {
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

async fn send(client: &mut Client, msg: &ClientMessage) {
    let json = serde_json::to_string(msg).unwrap();
    client.send(WsMessage::text(json)).await.unwrap();
}

async fn send_raw(client: &mut Client, text: &str) {
    client.send(WsMessage::text(text)).await.unwrap();
}

/// Ждёт следующее прикладное сообщение, пропуская служебные ping/pong.
async fn recv(client: &mut Client) -> ServerMessage {
    loop {
        let frame = timeout(Duration::from_secs(2), client.next())
            .await
            .expect("сервер не ответил за 2 секунды")
            .expect("соединение закрылось")
            .unwrap();
        match frame {
            WsMessage::Text(text) => return serde_json::from_str(text.as_str()).unwrap(),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            other => panic!("неожиданный кадр: {other:?}"),
        }
    }
}

async fn try_join(client: &mut Client, nickname: &str, room: &str) -> ServerMessage {
    send(
        client,
        &ClientMessage::Join {
            nickname: nickname.into(),
            room: room.into(),
        },
    )
    .await;
    recv(client).await
}

/// Подключается и входит в комнату, требуя успеха.
async fn joined(url: &str, nickname: &str, room: &str) -> Client {
    let mut client = connect(url).await;
    let msg = try_join(&mut client, nickname, room).await;
    assert!(
        matches!(msg, ServerMessage::Welcome { .. }),
        "{nickname} не смог войти: {msg:?}"
    );
    client
}

async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("условие не выполнилось за 2 секунды");
}

#[tokio::test]
async fn welcome_lists_those_who_were_already_there() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let mut bob = connect(&url).await;

    let welcome = try_join(&mut bob, "bob", "general").await;
    let ServerMessage::Welcome {
        room,
        nickname,
        users,
        ..
    } = welcome
    else {
        panic!("ожидался welcome, пришло {welcome:?}");
    };
    assert_eq!((room.as_str(), nickname.as_str()), ("general", "bob"));
    // Себя в списке быть не должно — только те, кто уже сидел в комнате.
    let names: Vec<_> = users.iter().map(|u| u.nickname.as_str()).collect();
    assert_eq!(names, ["alice"]);

    let event = recv(&mut alice).await;
    let ServerMessage::UserJoined { user } = event else {
        panic!("alice не получила user_joined, пришло {event:?}");
    };
    assert_eq!(user.nickname, "bob");
}

#[tokio::test]
async fn nickname_and_room_are_normalized() {
    let url = spawn().await;
    let mut client = connect(&url).await;

    let welcome = try_join(&mut client, "  alice  ", "  General ").await;
    let ServerMessage::Welcome { room, nickname, .. } = welcome else {
        panic!("ожидался welcome, пришло {welcome:?}");
    };
    // Клиент должен показывать то, что вернул сервер, а не собственный ввод.
    assert_eq!((room.as_str(), nickname.as_str()), ("general", "alice"));
}

#[tokio::test]
async fn chat_reaches_only_its_own_room() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "one").await;
    let mut carol = joined(&url, "carol", "one").await;
    let mut bob = joined(&url, "bob", "two").await;

    // alice уже получила user_joined про carol — сначала разбираем его.
    assert!(matches!(
        recv(&mut alice).await,
        ServerMessage::UserJoined { .. }
    ));

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "  привет\nмир  ".into(),
            attachment: None,
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Chat(ChatMessage {
        id, from, text, ts, ..
    }) = recv(&mut alice).await
    else {
        panic!("отправитель не получил собственное сообщение");
    };
    assert_eq!(text, "привет мир");
    assert!(ts > 1_577_836_800_000, "сервер обязан проставить время");

    // Сосед по комнате видит ровно то же самое сообщение.
    let ServerMessage::Chat(ChatMessage {
        id: carol_id,
        text: carol_text,
        ..
    }) = recv(&mut carol).await
    else {
        panic!("carol не получила сообщение");
    };
    assert_eq!((carol_id, carol_text), (id, text));
    assert_eq!(from.nickname, "alice");

    // А в соседнюю комнату не долетело ничего: первым ответом bob будет pong.
    send(&mut bob, &ClientMessage::Ping).await;
    assert!(matches!(recv(&mut bob).await, ServerMessage::Pong));
}

#[tokio::test]
async fn taken_nickname_can_be_retried_on_the_same_connection() {
    let url = spawn().await;
    let _alice = joined(&url, "alice", "general").await;
    let mut other = connect(&url).await;

    let msg = try_join(&mut other, "alice", "general").await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::NicknameTaken,
                ..
            }
        ),
        "пришло {msg:?}"
    );

    // Отказ не рвёт соединение: человек вводит другой ник и заходит.
    let msg = try_join(&mut other, "alice2", "general").await;
    assert!(
        matches!(msg, ServerMessage::Welcome { .. }),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn nickname_uniqueness_ignores_case() {
    let url = spawn().await;
    let _alice = joined(&url, "alice", "general").await;
    let mut other = connect(&url).await;

    let msg = try_join(&mut other, "ALICE", "general").await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::NicknameTaken,
                ..
            }
        ),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn same_nickname_in_different_rooms_is_fine() {
    let url = spawn().await;
    let _alice = joined(&url, "alice", "one").await;
    let mut other = connect(&url).await;

    let msg = try_join(&mut other, "alice", "two").await;
    assert!(
        matches!(msg, ServerMessage::Welcome { .. }),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn invalid_nickname_and_room_are_rejected_separately() {
    let url = spawn().await;
    let mut client = connect(&url).await;

    let msg = try_join(&mut client, "a b", "general").await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::InvalidNickname,
                ..
            }
        ),
        "пришло {msg:?}"
    );

    let msg = try_join(&mut client, "alice", "общая").await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::InvalidRoom,
                ..
            }
        ),
        "пришло {msg:?}"
    );

    let msg = try_join(&mut client, "alice", "general").await;
    assert!(
        matches!(msg, ServerMessage::Welcome { .. }),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn chat_before_join_is_rejected() {
    let url = spawn().await;
    let mut client = connect(&url).await;

    send(
        &mut client,
        &ClientMessage::Chat {
            text: "привет".into(),
            attachment: None,
            reply_to: None,
        },
    )
    .await;

    let msg = recv(&mut client).await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::NotJoined,
                ..
            }
        ),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn second_join_is_rejected() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;

    let msg = try_join(&mut alice, "alice", "another").await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::AlreadyJoined,
                ..
            }
        ),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn leave_notifies_the_rest_of_the_room() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let mut bob = joined(&url, "bob", "general").await;
    assert!(matches!(
        recv(&mut alice).await,
        ServerMessage::UserJoined { .. }
    ));

    send(&mut bob, &ClientMessage::Leave).await;

    let ServerMessage::UserLeft { user } = recv(&mut alice).await else {
        panic!("alice не получила user_left");
    };
    assert_eq!(user.nickname, "bob");
}

#[tokio::test]
async fn dropped_connection_notifies_the_rest_of_the_room() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let bob = joined(&url, "bob", "general").await;
    assert!(matches!(
        recv(&mut alice).await,
        ServerMessage::UserJoined { .. }
    ));

    // Обрыв без close-кадра — как если бы у телефона пропал Wi-Fi.
    drop(bob);

    let ServerMessage::UserLeft { user } = recv(&mut alice).await else {
        panic!("alice не получила user_left");
    };
    assert_eq!(user.nickname, "bob");
}

#[tokio::test]
async fn empty_room_is_dropped() {
    let (url, hub) = spawn_with(HubConfig::default()).await;
    let alice = joined(&url, "alice", "temp").await;
    wait_until(|| hub.user_count("temp") == 1).await;

    drop(alice);

    // Иначе список комнат рос бы вечно при долгой работе сервера.
    wait_until(|| hub.room_count() == 0).await;
}

#[tokio::test]
async fn full_room_is_rejected() {
    let config = HubConfig {
        max_room_users: 1,
        ..HubConfig::default()
    };
    let (url, _hub) = spawn_with(config).await;
    let _alice = joined(&url, "alice", "general").await;
    let mut bob = connect(&url).await;

    let msg = try_join(&mut bob, "bob", "general").await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::RoomFull,
                ..
            }
        ),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn silent_connection_is_closed_by_join_timeout() {
    let config = HubConfig {
        join_timeout: Duration::from_millis(150),
        ..HubConfig::default()
    };
    let (url, hub) = spawn_with(config).await;
    let mut client = connect(&url).await;

    // Клиент молчит: такие «мёртвые» сокеты нельзя держать вечно.
    let msg = recv(&mut client).await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::NotJoined,
                ..
            }
        ),
        "пришло {msg:?}"
    );
    assert_eq!(hub.room_count(), 0);
}

#[tokio::test]
async fn ping_gets_pong() {
    let url = spawn().await;
    let mut client = joined(&url, "alice", "general").await;

    send(&mut client, &ClientMessage::Ping).await;

    assert!(matches!(recv(&mut client).await, ServerMessage::Pong));
}

#[tokio::test]
async fn broken_json_is_answered_with_error_and_keeps_connection() {
    let url = spawn().await;
    let mut client = joined(&url, "alice", "general").await;

    send_raw(&mut client, "не json вовсе").await;

    let msg = recv(&mut client).await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::InvalidMessage,
                ..
            }
        ),
        "пришло {msg:?}"
    );

    // Мусорный кадр не должен рвать соединение: клиент с багом продолжает работать.
    send(&mut client, &ClientMessage::Ping).await;
    assert!(matches!(recv(&mut client).await, ServerMessage::Pong));
}

#[tokio::test]
async fn blank_message_is_rejected_and_not_broadcast() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let mut bob = joined(&url, "bob", "general").await;
    assert!(matches!(
        recv(&mut alice).await,
        ServerMessage::UserJoined { .. }
    ));

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "   \n\t ".into(),
            attachment: None,
            reply_to: None,
        },
    )
    .await;

    let msg = recv(&mut alice).await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::InvalidMessage,
                ..
            }
        ),
        "пришло {msg:?}"
    );

    // Bob не должен увидеть ничего: первым ответом ему придёт pong.
    send(&mut bob, &ClientMessage::Ping).await;
    assert!(matches!(recv(&mut bob).await, ServerMessage::Pong));
}

#[tokio::test]
async fn leave_closes_the_connection() {
    let url = spawn().await;
    let mut client = joined(&url, "alice", "general").await;

    send(&mut client, &ClientMessage::Leave).await;

    let closed = timeout(Duration::from_secs(2), client.next())
        .await
        .expect("сервер не закрыл соединение за 2 секунды");
    assert!(
        matches!(closed, None | Some(Ok(WsMessage::Close(_))) | Some(Err(_))),
        "ожидалось закрытие, пришло {closed:?}"
    );
}

/// Та самая гонка: десяток клиентов одновременно занимает один ник.
/// Проверка и вставка идут под одним захватом мьютекса, поэтому победитель
/// обязан быть ровно один.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_one_of_the_racing_nicknames_wins() {
    let url = spawn().await;

    let racers: Vec<_> = (0..16)
        .map(|_| {
            let url = url.clone();
            tokio::spawn(async move {
                let mut client = connect(&url).await;
                let msg = try_join(&mut client, "alice", "general").await;
                // Соединение возвращаем наружу: если бросить его здесь,
                // победитель отвалится и освободит ник ещё до подсчёта.
                (matches!(msg, ServerMessage::Welcome { .. }), client)
            })
        })
        .collect();

    let mut winners = 0;
    let mut clients = Vec::new();
    for racer in racers {
        let (won, client) = racer.await.unwrap();
        winners += usize::from(won);
        clients.push(client);
    }

    assert_eq!(winners, 1, "ник достался более чем одному клиенту");
}

/// Вошедший видит последние реплики, а не пустой экран.
#[tokio::test]
async fn newcomer_gets_the_room_history() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    for text in ["первое", "второе"] {
        send(
            &mut alice,
            &ClientMessage::Chat {
                text: text.into(),
                attachment: None,
                reply_to: None,
            },
        )
        .await;
        assert!(matches!(recv(&mut alice).await, ServerMessage::Chat(_)));
    }

    let mut bob = connect(&url).await;
    let welcome = try_join(&mut bob, "bob", "general").await;

    let ServerMessage::Welcome { history, .. } = welcome else {
        panic!("ожидался welcome, пришло {welcome:?}");
    };
    let texts: Vec<_> = history.iter().map(|msg| msg.text.as_str()).collect();
    assert_eq!(texts, ["первое", "второе"], "порядок должен сохраняться");
}

#[tokio::test]
async fn history_keeps_only_the_last_messages() {
    let config = HubConfig {
        history_limit: 2,
        ..HubConfig::default()
    };
    let (url, _hub) = spawn_with(config).await;
    let mut alice = joined(&url, "alice", "general").await;
    for text in ["раз", "два", "три"] {
        send(
            &mut alice,
            &ClientMessage::Chat {
                text: text.into(),
                attachment: None,
                reply_to: None,
            },
        )
        .await;
        assert!(matches!(recv(&mut alice).await, ServerMessage::Chat(_)));
    }

    let mut bob = connect(&url).await;
    let welcome = try_join(&mut bob, "bob", "general").await;

    let ServerMessage::Welcome { history, .. } = welcome else {
        panic!("ожидался welcome, пришло {welcome:?}");
    };
    let texts: Vec<_> = history.iter().map(|msg| msg.text.as_str()).collect();
    assert_eq!(texts, ["два", "три"]);
}

/// Одно и то же сообщение в истории и в рассылке имеет один id: только так
/// клиент сможет отбросить дубль, когда после обрыва история наложится на
/// уже показанное.
#[tokio::test]
async fn history_repeats_message_ids_so_clients_can_deduplicate() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "привет".into(),
            attachment: None,
            reply_to: None,
        },
    )
    .await;
    let ServerMessage::Chat(sent) = recv(&mut alice).await else {
        panic!("alice не получила собственное сообщение");
    };

    let mut bob = connect(&url).await;
    let welcome = try_join(&mut bob, "bob", "general").await;

    let ServerMessage::Welcome { history, .. } = welcome else {
        panic!("ожидался welcome, пришло {welcome:?}");
    };
    assert_eq!(history, [sent]);
}

/// История живёт вместе с комнатой: когда все вышли, комната удаляется, и
/// хранить её переписку было бы той самой утечкой, ради которой комнаты чистятся.
#[tokio::test]
async fn history_dies_with_the_empty_room() {
    let (url, hub) = spawn_with(HubConfig::default()).await;
    let mut alice = joined(&url, "alice", "temp").await;
    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "было".into(),
            attachment: None,
            reply_to: None,
        },
    )
    .await;
    assert!(matches!(recv(&mut alice).await, ServerMessage::Chat(_)));

    drop(alice);
    wait_until(|| hub.room_count() == 0).await;

    let mut bob = connect(&url).await;
    let welcome = try_join(&mut bob, "bob", "temp").await;

    let ServerMessage::Welcome { history, .. } = welcome else {
        panic!("ожидался welcome, пришло {welcome:?}");
    };
    assert!(history.is_empty(), "история пережила комнату");
}

// ── Вложения ────────────────────────────────────────────────────────────────

/// Минимальный корректный PNG: сервер смотрит на сигнатуру, а не на имя.
const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";

fn http_addr(ws_url: &str) -> String {
    ws_url
        .trim_start_matches("ws://")
        .trim_end_matches("/ws")
        .to_string()
}

async fn http_post(url: &str, path: &str, body: &[u8]) -> (String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(http_addr(url))
        .await
        .unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    let text = String::from_utf8_lossy(&response).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    (head.to_string(), body.to_string())
}

async fn http_get(url: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(http_addr(url))
        .await
        .unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response).to_string()
}

async fn upload_png(url: &str, name: &str) -> serde_json::Value {
    let (head, body) = http_post(url, &format!("/upload?name={name}"), PNG).await;
    assert!(head.starts_with("HTTP/1.1 200 OK"), "{head}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn uploaded_image_is_described_by_the_server() {
    let url = spawn().await;

    let attachment = upload_png(&url, "cat.png").await;

    assert_eq!(attachment["kind"], "image");
    assert_eq!(attachment["mime"], "image/png");
    assert_eq!(attachment["name"], "cat.png");
    assert_eq!(attachment["size"], PNG.len());
}

#[tokio::test]
async fn non_image_upload_is_rejected() {
    let url = spawn().await;

    // Скрипт под видом картинки: раздавали бы мы его со своего же адреса.
    let (head, _) = http_post(
        &url,
        "/upload?name=evil.svg",
        b"<svg onload=alert(1)></svg>",
    )
    .await;

    assert!(head.starts_with("HTTP/1.1 415"), "{head}");
}

#[tokio::test]
async fn media_is_served_with_headers_that_prevent_execution() {
    let url = spawn().await;
    let attachment = upload_png(&url, "cat.png").await;
    let id = attachment["id"].as_str().unwrap();

    let response = http_get(&url, &format!("/media/{id}")).await;

    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response:.60}");
    assert!(response.contains("image/png"));
    // Без nosniff браузер может решить, что перед ним разметка, и исполнить её.
    assert!(
        response
            .to_lowercase()
            .contains("x-content-type-options: nosniff")
    );
}

#[tokio::test]
async fn unknown_media_is_not_found() {
    let url = spawn().await;

    let response = http_get(&url, &format!("/media/{}", uuid::Uuid::new_v4())).await;

    assert!(response.starts_with("HTTP/1.1 404"), "{response:.60}");
}

#[tokio::test]
async fn message_with_an_attachment_reaches_the_room() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let mut bob = joined(&url, "bob", "general").await;
    assert!(matches!(
        recv(&mut alice).await,
        ServerMessage::UserJoined { .. }
    ));

    let uploaded = upload_png(&url, "кот.png").await;
    let id: uuid::Uuid = uploaded["id"].as_str().unwrap().parse().unwrap();
    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "смотри".into(),
            attachment: Some(id),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Chat(message) = recv(&mut bob).await else {
        panic!("bob не получил сообщение");
    };
    let attachment = message.attachment.expect("вложение потерялось");
    // Описание пришло из хранилища сервера, а не со слов клиента.
    assert_eq!(attachment.id, id);
    assert_eq!(attachment.name, "кот.png");
    assert_eq!(attachment.mime, "image/png");
    assert_eq!(message.text, "смотри");
}

#[tokio::test]
async fn attachment_without_caption_is_allowed() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let uploaded = upload_png(&url, "кот.png").await;
    let id: uuid::Uuid = uploaded["id"].as_str().unwrap().parse().unwrap();

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: String::new(),
            attachment: Some(id),
            reply_to: None,
        },
    )
    .await;

    let ServerMessage::Chat(message) = recv(&mut alice).await else {
        panic!("картинка без подписи не дошла");
    };
    assert!(message.text.is_empty());
    assert!(message.attachment.is_some());
}

#[tokio::test]
async fn unknown_attachment_id_is_rejected() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "смотри".into(),
            attachment: Some(uuid::Uuid::new_v4()),
            reply_to: None,
        },
    )
    .await;

    let msg = recv(&mut alice).await;
    assert!(
        matches!(
            msg,
            ServerMessage::Error {
                code: ErrorCode::InvalidMessage,
                ..
            }
        ),
        "пришло {msg:?}"
    );
}

#[tokio::test]
async fn attachments_survive_in_the_history() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let uploaded = upload_png(&url, "кот.png").await;
    let id: uuid::Uuid = uploaded["id"].as_str().unwrap().parse().unwrap();
    send(
        &mut alice,
        &ClientMessage::Chat {
            text: String::new(),
            attachment: Some(id),
            reply_to: None,
        },
    )
    .await;
    assert!(matches!(recv(&mut alice).await, ServerMessage::Chat(_)));

    let mut bob = connect(&url).await;
    let welcome = try_join(&mut bob, "bob", "general").await;

    let ServerMessage::Welcome { history, .. } = welcome else {
        panic!("ожидался welcome");
    };
    assert_eq!(history[0].attachment.as_ref().unwrap().id, id);
}

// ── Ответы ──────────────────────────────────────────────────────────────────

async fn say(client: &mut Client, text: &str) -> common::ChatMessage {
    send(
        client,
        &ClientMessage::Chat {
            text: text.into(),
            attachment: None,
            reply_to: None,
        },
    )
    .await;
    let ServerMessage::Chat(message) = recv(client).await else {
        panic!("сообщение не вернулось");
    };
    message
}

#[tokio::test]
async fn reply_carries_a_quote_built_by_the_server() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let original = say(&mut alice, "привет всем").await;

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "и тебе".into(),
            attachment: None,
            reply_to: Some(original.id),
        },
    )
    .await;

    let ServerMessage::Chat(answer) = recv(&mut alice).await else {
        panic!("ответ не дошёл");
    };
    let quote = answer.reply.expect("цитата потерялась");
    // Цитату собрал сервер из своей истории, клиент прислал только id.
    assert_eq!(quote.id, original.id);
    assert_eq!(quote.nickname, "alice");
    assert_eq!(quote.excerpt, "привет всем");
}

#[tokio::test]
async fn reply_to_a_forgotten_message_still_arrives() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "ответ в пустоту".into(),
            attachment: None,
            reply_to: Some(uuid::Uuid::new_v4()),
        },
    )
    .await;

    let ServerMessage::Chat(message) = recv(&mut alice).await else {
        panic!("сообщение потерялось");
    };
    // Исходное сообщение могло быть вытеснено из истории — терять из-за этого
    // сам ответ было бы обидно.
    assert_eq!(message.text, "ответ в пустоту");
    assert!(message.reply.is_none());
}

#[tokio::test]
async fn quote_of_a_picture_uses_its_name() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let uploaded = upload_png(&url, "кот.png").await;
    let id: uuid::Uuid = uploaded["id"].as_str().unwrap().parse().unwrap();
    send(
        &mut alice,
        &ClientMessage::Chat {
            text: String::new(),
            attachment: Some(id),
            reply_to: None,
        },
    )
    .await;
    let ServerMessage::Chat(picture) = recv(&mut alice).await else {
        panic!("картинка не дошла");
    };

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "красивый".into(),
            attachment: None,
            reply_to: Some(picture.id),
        },
    )
    .await;

    let ServerMessage::Chat(answer) = recv(&mut alice).await else {
        panic!("ответ не дошёл");
    };
    // У картинки без подписи цитировать нечего, кроме имени файла.
    assert_eq!(answer.reply.unwrap().excerpt, "кот.png");
}

#[tokio::test]
async fn long_quote_is_trimmed() {
    let url = spawn().await;
    let mut alice = joined(&url, "alice", "general").await;
    let original = say(&mut alice, &"я".repeat(200)).await;

    send(
        &mut alice,
        &ClientMessage::Chat {
            text: "ого".into(),
            attachment: None,
            reply_to: Some(original.id),
        },
    )
    .await;

    let ServerMessage::Chat(answer) = recv(&mut alice).await else {
        panic!("ответ не дошёл");
    };
    let excerpt = answer.reply.unwrap().excerpt;
    // Цитата на весь экран мешала бы читать сам ответ.
    assert_eq!(excerpt.chars().count(), common::REPLY_EXCERPT_CHARS);
}
