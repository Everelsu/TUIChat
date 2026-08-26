//! Сетевая задача клиента: подключение, переподключение и keepalive.
//!
//! Живёт отдельно от интерфейса и общается с ним двумя каналами: наружу уходят
//! `NetEvent`, внутрь приходят `ClientMessage`. Благодаря этому обрыв связи —
//! обычное событие в очереди, а не паника посреди отрисовки.

use std::{
    future,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use common::{ClientMessage, ErrorCode, ServerMessage, validate};
use futures_util::{SinkExt, StreamExt, stream::SplitSink};
use tokio::{
    net::TcpStream,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
    task::JoinHandle,
    time::{Instant, sleep_until},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Error as WsError, Message as WsMessage, protocol::WebSocketConfig},
};

use crate::app::{Action, NetEvent};

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub url: String,
    pub nickname: String,
    pub room: String,
    /// Как часто слать прикладной ping.
    ///
    /// Простаивающее соединение рвут NAT и роутеры, причём молча: без
    /// keepalive клиент узнаёт об этом только когда попробует что-то отправить.
    pub ping_every: Duration,
    pub pong_timeout: Duration,
    pub first_backoff: Duration,
    pub max_backoff: Duration,
}

impl NetConfig {
    pub fn new(
        url: impl Into<String>,
        nickname: impl Into<String>,
        room: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            nickname: nickname.into(),
            room: room.into(),
            ping_every: Duration::from_secs(30),
            pong_timeout: Duration::from_secs(10),
            first_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
        }
    }
}

/// http-адрес сервера, выведенный из адреса WebSocket: по нему клиент строит
/// ссылки на вложения, не спрашивая их отдельным параметром.
pub fn media_base(ws_url: &str) -> String {
    let base = ws_url.trim_end_matches("/ws");
    if let Some(rest) = base.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = base.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        base.to_string()
    }
}

enum Outgoing {
    Msg(ClientMessage),
    Shutdown,
}

pub struct NetHandle {
    tx: UnboundedSender<Outgoing>,
    task: JoinHandle<()>,
    alive: Arc<AtomicBool>,
}

impl NetHandle {
    pub fn send(&self, msg: ClientMessage) {
        let _ = self.tx.send(Outgoing::Msg(msg));
    }

    /// Просит задачу попрощаться и уйти, не дожидаясь её.
    ///
    /// Нужно при смене комнаты: главный цикл не может замереть на секунды,
    /// пока старое соединение закрывается. Флаг снимается сразу, поэтому
    /// уходящая задача уже не подмешает свои события в новый экран.
    pub fn close(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Outgoing::Shutdown);
    }

    /// Прощается с сервером и дожидается завершения задачи.
    ///
    /// Ждём с потолком: если сервер уже недоступен, выход из программы не должен
    /// из-за этого зависать.
    pub async fn shutdown(self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.tx.send(Outgoing::Shutdown);
        let _ = tokio::time::timeout(Duration::from_secs(2), self.task).await;
    }
}

/// Канал событий наружу, который умолкает, как только соединение объявлено
/// устаревшим.
struct Events {
    tx: UnboundedSender<Action>,
    alive: Arc<AtomicBool>,
}

impl Events {
    fn send(&self, event: NetEvent) -> bool {
        self.alive.load(Ordering::Relaxed) && self.tx.send(Action::Net(event)).is_ok()
    }
}

pub fn spawn(config: NetConfig, actions: UnboundedSender<Action>) -> NetHandle {
    let (tx, rx) = unbounded_channel();
    let alive = Arc::new(AtomicBool::new(true));
    let events = Events {
        tx: actions,
        alive: Arc::clone(&alive),
    };
    let task = tokio::spawn(run(config, events, rx));
    NetHandle { tx, task, alive }
}

enum Outcome {
    Shutdown,
    /// Переподключение не поможет — например, ник занят.
    Fatal(String),
    Lost {
        reason: String,
        /// Успели ли войти в комнату: неудача на самом входе не должна
        /// обнулять счётчик попыток, иначе backoff перестанет расти.
        joined: bool,
    },
}

async fn run(config: NetConfig, events: Events, mut outgoing: UnboundedReceiver<Outgoing>) {
    let mut attempt: u32 = 0;

    loop {
        events.send(NetEvent::Connecting { attempt });

        let outcome = match connect(&config).await {
            Ok(socket) => session(socket, &config, &events, &mut outgoing).await,
            Err(err) => Outcome::Lost {
                reason: err.to_string(),
                joined: false,
            },
        };

        let (reason, joined) = match outcome {
            Outcome::Shutdown => return,
            Outcome::Fatal(reason) => {
                events.send(NetEvent::Fatal { reason });
                return;
            }
            Outcome::Lost { reason, joined } => (reason, joined),
        };

        let wait = backoff(&config, attempt);
        attempt = if joined { 0 } else { attempt.saturating_add(1) };
        events.send(NetEvent::Disconnected {
            reason,
            retry_at: std::time::Instant::now() + wait,
        });

        if !wait_before_retry(wait, &mut outgoing).await {
            return;
        }
    }
}

/// 1, 2, 4, 8… секунд с потолком.
///
/// Без роста задержки клиент долбится в упавший сервер несколько раз в секунду
/// и мешает ему подняться.
fn backoff(config: &NetConfig, attempt: u32) -> Duration {
    let factor = 1u32 << attempt.min(16);
    config
        .first_backoff
        .saturating_mul(factor)
        .min(config.max_backoff)
}

/// Пауза перед следующей попыткой. `false` — пришла команда завершиться.
async fn wait_before_retry(wait: Duration, outgoing: &mut UnboundedReceiver<Outgoing>) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        tokio::select! {
            _ = sleep_until(deadline) => return true,
            cmd = outgoing.recv() => match cmd {
                None | Some(Outgoing::Shutdown) => return false,
                // Отправлять некуда: состояние клиента и так не даёт писать офлайн.
                Some(Outgoing::Msg(_)) => {}
            },
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Sink = SplitSink<Socket, WsMessage>;

async fn connect(config: &NetConfig) -> Result<Socket, WsError> {
    // Тот же потолок на кадр, что и у сервера: клиент тоже не должен падать
    // по памяти из-за одного гигантского сообщения.
    let ws_config = WebSocketConfig::default().max_message_size(Some(validate::MAX_FRAME_BYTES));
    let (socket, _) =
        tokio_tungstenite::connect_async_with_config(&config.url, Some(ws_config), false).await?;
    Ok(socket)
}

async fn session(
    socket: Socket,
    config: &NetConfig,
    events: &Events,
    outgoing: &mut UnboundedReceiver<Outgoing>,
) -> Outcome {
    let (mut sink, mut stream) = socket.split();

    let join = ClientMessage::Join {
        nickname: config.nickname.clone(),
        room: config.room.clone(),
    };
    if let Err(err) = send(&mut sink, &join).await {
        return Outcome::Lost {
            reason: err.to_string(),
            joined: false,
        };
    }

    let mut joined = false;
    let mut ping = tokio::time::interval(config.ping_every);
    ping.tick().await; // первый тик срабатывает мгновенно, он нам не нужен
    let mut pong_deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                if send(&mut sink, &ClientMessage::Ping).await.is_err() {
                    return Outcome::Lost { reason: "не удалось отправить ping".into(), joined };
                }
                pong_deadline.get_or_insert(Instant::now() + config.pong_timeout);
            }

            _ = elapsed(pong_deadline) => {
                // Молчащее соединение выглядит живым, пока в него не пишешь:
                // считаем его мёртвым сами и переподключаемся.
                return Outcome::Lost { reason: "сервер не отвечает на ping".into(), joined };
            }

            cmd = outgoing.recv() => match cmd {
                None | Some(Outgoing::Shutdown) => {
                    // Прощаемся явно, иначе остальные увидят наш уход только
                    // после серверного таймаута.
                    let _ = send(&mut sink, &ClientMessage::Leave).await;
                    let _ = sink.close().await;
                    return Outcome::Shutdown;
                }
                Some(Outgoing::Msg(msg)) => {
                    if send(&mut sink, &msg).await.is_err() {
                        return Outcome::Lost { reason: "не удалось отправить сообщение".into(), joined };
                    }
                }
            },

            frame = stream.next() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(err)) => return Outcome::Lost { reason: err.to_string(), joined },
                    None => return Outcome::Lost { reason: "сервер закрыл соединение".into(), joined },
                };

                let text = match frame {
                    WsMessage::Text(text) => text,
                    WsMessage::Close(_) => {
                        return Outcome::Lost { reason: "сервер закрыл соединение".into(), joined };
                    }
                    _ => continue,
                };

                let Ok(msg) = serde_json::from_str::<ServerMessage>(text.as_str()) else {
                    // Кадр, которого мы не понимаем, — повод не падать, а жить дальше:
                    // так старый клиент переживёт появление новых типов сообщений.
                    continue;
                };

                match &msg {
                    // Pong — забота сетевого слоя, интерфейсу он не нужен.
                    ServerMessage::Pong => {
                        pong_deadline = None;
                        continue;
                    }
                    ServerMessage::Welcome { .. } => joined = true,
                    ServerMessage::Error { code, message } if !joined && is_fatal(*code) => {
                        let _ = sink.close().await;
                        return Outcome::Fatal(message.clone());
                    }
                    _ => {}
                }

                if !events.send(NetEvent::Message(msg)) {
                    // Интерфейс закрылся или соединение устарело — уходим тихо.
                    let _ = sink.close().await;
                    return Outcome::Shutdown;
                }
            },
        }
    }
}

/// Ошибка на входе, которую не вылечит переподключение: с тем же ником
/// клиент будет получать её бесконечно.
fn is_fatal(code: ErrorCode) -> bool {
    matches!(
        code,
        ErrorCode::NicknameTaken | ErrorCode::InvalidNickname | ErrorCode::InvalidRoom
    )
}

/// Ждёт наступления момента; при `None` не срабатывает никогда.
async fn elapsed(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => future::pending().await,
    }
}

async fn send(sink: &mut Sink, msg: &ClientMessage) -> Result<(), WsError> {
    let json = serde_json::to_string(msg).expect("ClientMessage сериализуется всегда");
    sink.send(WsMessage::text(json)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_stops_at_the_ceiling() {
        let config = NetConfig::new("ws://localhost/ws", "alice", "general");

        let waits: Vec<_> = (0..8).map(|n| backoff(&config, n).as_secs()).collect();

        assert_eq!(waits, [1, 2, 4, 8, 16, 30, 30, 30]);
    }

    #[test]
    fn media_base_follows_the_websocket_address() {
        assert_eq!(
            media_base("ws://192.168.1.5:8080/ws"),
            "http://192.168.1.5:8080"
        );
        // По https адрес вложений тоже обязан быть защищённым, иначе браузер
        // и терминал получат предупреждение о смешанном содержимом.
        assert_eq!(media_base("wss://chat.example/ws"), "https://chat.example");
    }

    #[test]
    fn huge_attempt_counter_does_not_overflow() {
        let config = NetConfig::new("ws://localhost/ws", "alice", "general");

        assert_eq!(backoff(&config, u32::MAX), config.max_backoff);
    }
}
