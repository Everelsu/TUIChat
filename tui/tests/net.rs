//! Проверка сетевого слоя клиента против настоящего сервера.
//!
//! Тут ловятся ровно те ошибки, ради которых в плане был отдельный этап
//! «клиент без интерфейса»: рукопожатие, реконнект и прощание.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use common::{ClientMessage, ServerMessage};
use server::{Hub, HubConfig};
use tokio::{
    net::TcpListener,
    sync::mpsc::{UnboundedReceiver, unbounded_channel},
    time::timeout,
};
use tui::{
    app::{Action, NetEvent},
    net::{self, NetConfig, NetHandle},
};

/// Быстрые таймауты: иначе тест на реконнект шёл бы секундами.
fn config(addr: SocketAddr, nickname: &str) -> NetConfig {
    let mut config = NetConfig::new(format!("ws://{addr}/ws"), nickname, "general");
    config.ping_every = Duration::from_millis(200);
    config.pong_timeout = Duration::from_millis(500);
    config.first_backoff = Duration::from_millis(50);
    config.max_backoff = Duration::from_millis(200);
    config
}

async fn serve_on(listener: TcpListener, hub: Arc<Hub>) {
    let app = server::app_with_hub(hub);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
}

async fn start_server() -> (SocketAddr, Arc<Hub>) {
    let hub = Arc::new(Hub::new(HubConfig::default()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    serve_on(listener, Arc::clone(&hub)).await;
    (addr, hub)
}

/// Адрес, на котором заведомо никто не слушает.
async fn free_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

fn start_client(config: NetConfig) -> (NetHandle, UnboundedReceiver<Action>) {
    let (actions, incoming) = unbounded_channel();
    (net::spawn(config, actions), incoming)
}

async fn next_event(incoming: &mut UnboundedReceiver<Action>) -> NetEvent {
    let action = timeout(Duration::from_secs(5), incoming.recv())
        .await
        .expect("клиент молчит уже 5 секунд")
        .expect("канал закрыт");
    match action {
        Action::Net(event) => event,
        other => panic!("ожидалось сетевое событие, пришло {other:?}"),
    }
}

/// Ждёт сообщение от сервера, пропуская служебные события подключения.
async fn next_message(incoming: &mut UnboundedReceiver<Action>) -> ServerMessage {
    loop {
        match next_event(incoming).await {
            NetEvent::Message(msg) => return msg,
            NetEvent::Fatal { reason } => panic!("фатальная ошибка: {reason}"),
            _ => continue,
        }
    }
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
async fn client_joins_and_exchanges_messages() {
    let (addr, _hub) = start_server().await;
    let (client, mut incoming) = start_client(config(addr, "alice"));

    let welcome = next_message(&mut incoming).await;
    let ServerMessage::Welcome { nickname, room, .. } = welcome else {
        panic!("первым сообщением должен быть welcome, пришло {welcome:?}");
    };
    assert_eq!((nickname.as_str(), room.as_str()), ("alice", "general"));

    client.send(ClientMessage::Chat {
        text: "привет".into(),
        attachment: None,
        reply_to: None,
    });

    let ServerMessage::Chat(message) = next_message(&mut incoming).await else {
        panic!("сообщение не вернулось из комнаты");
    };
    assert_eq!(
        (message.from.nickname.as_str(), message.text.as_str()),
        ("alice", "привет")
    );
}

#[tokio::test]
async fn pong_is_swallowed_by_the_network_layer() {
    let (addr, _hub) = start_server().await;
    let (client, mut incoming) = start_client(config(addr, "alice"));
    assert!(matches!(
        next_message(&mut incoming).await,
        ServerMessage::Welcome { .. }
    ));

    // Keepalive настроен на 200 мс: за это время придёт несколько pong.
    tokio::time::sleep(Duration::from_millis(600)).await;
    client.send(ClientMessage::Chat {
        text: "живой".into(),
        attachment: None,
        reply_to: None,
    });

    // Интерфейс не должен видеть служебный обмен — сразу приходит чат.
    assert!(matches!(
        next_message(&mut incoming).await,
        ServerMessage::Chat(_)
    ));
}

#[tokio::test]
async fn taken_nickname_is_fatal_and_not_retried() {
    let (addr, _hub) = start_server().await;
    let (_alice, mut alice_incoming) = start_client(config(addr, "alice"));
    assert!(matches!(
        next_message(&mut alice_incoming).await,
        ServerMessage::Welcome { .. }
    ));

    let (_other, mut other_incoming) = start_client(config(addr, "alice"));

    // Переподключение с тем же ником даст ту же ошибку — клиент обязан сдаться,
    // а не долбиться в сервер по кругу.
    loop {
        match next_event(&mut other_incoming).await {
            NetEvent::Fatal { reason } => {
                assert!(reason.contains("занят"), "невнятная причина: {reason}");
                break;
            }
            NetEvent::Disconnected { .. } => panic!("клиент пытается переподключиться"),
            _ => continue,
        }
    }
}

#[tokio::test]
async fn client_reconnects_when_the_server_appears_later() {
    let addr = free_addr().await;
    let (_client, mut incoming) = start_client(config(addr, "alice"));

    // Сервера ещё нет: первая попытка обязана закончиться паузой, а не паникой.
    let mut attempts = 0;
    loop {
        match next_event(&mut incoming).await {
            NetEvent::Disconnected { .. } => {
                attempts += 1;
                if attempts == 2 {
                    break;
                }
            }
            NetEvent::Fatal { reason } => {
                panic!("недоступный сервер — не повод сдаваться: {reason}")
            }
            _ => continue,
        }
    }

    let hub = Arc::new(Hub::new(HubConfig::default()));
    let listener = TcpListener::bind(addr).await.unwrap();
    serve_on(listener, hub).await;

    assert!(matches!(
        next_message(&mut incoming).await,
        ServerMessage::Welcome { .. }
    ));
}

#[tokio::test]
async fn shutdown_says_goodbye_so_others_see_it_immediately() {
    let (addr, hub) = start_server().await;
    let (client, mut incoming) = start_client(config(addr, "alice"));
    assert!(matches!(
        next_message(&mut incoming).await,
        ServerMessage::Welcome { .. }
    ));
    wait_until(|| hub.user_count("general") == 1).await;

    client.shutdown().await;

    // Комната пустеет сразу, а не по таймауту: значит Leave и close дошли.
    wait_until(|| hub.room_count() == 0).await;
}
