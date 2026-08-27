//! Сервер: комнаты, ники, рассылка событий входа и выхода.
//!
//! Соединение живёт в двух фазах. До `Join` клиент не состоит ни в какой комнате
//! и не получает рассылок — он только пытается назваться. После успешного `Join`
//! он полноправный участник ровно одной комнаты.

pub mod media;
pub mod tls;

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Path as UrlPath, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use common::{
    ChatMessage, ClientMessage, ErrorCode, REPLY_EXCERPT_CHARS, ReplyPreview, ServerMessage,
    UserInfo, validate,
};
use futures_util::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use media::MediaStore;
use serde::Deserialize;
use tokio::{
    sync::mpsc,
    time::{Instant, timeout_at},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

/// Лимиты сервера. Вынесены в структуру, чтобы тесты могли проверять поведение
/// на границе, не дожидаясь по десять секунд и не открывая по пятьсот сокетов.
#[derive(Debug, Clone)]
pub struct HubConfig {
    /// Сколько ждать `Join` после установки соединения.
    ///
    /// Без этого таймаута клиент, зависший на этапе рукопожатия, держит сокет
    /// вечно, и такие «мёртвые» соединения копятся до упора в лимит.
    pub join_timeout: Duration,
    /// Потолок одновременных соединений, включая ещё не представившиеся.
    pub max_sockets: usize,
    pub max_rooms: usize,
    pub max_room_users: usize,
    /// Сколько последних реплик комнаты отдавать вошедшему.
    ///
    /// Ради этого и заводился id у сообщения: после обрыва связи история
    /// накладывается на уже показанное, и клиент отбрасывает дубли по id.
    pub history_limit: usize,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            join_timeout: Duration::from_secs(10),
            max_sockets: 512,
            max_rooms: 64,
            max_room_users: 64,
            history_limit: 50,
        }
    }
}

/// Канал в сторону конкретного клиента.
///
/// Именно per-user mpsc, а не `tokio::sync::broadcast`: с ним личные сообщения
/// и адресные ошибки не потребуют переделки рассылки.
type Outbox = mpsc::UnboundedSender<ServerMessage>;

struct Peer {
    user: UserInfo,
    outbox: Outbox,
    /// Когда от этого участника последний раз приходило «печатаю».
    /// Без учёта клиент с ошибкой залил бы комнату этими сообщениями.
    last_typing: Option<std::time::Instant>,
}

#[derive(Default)]
struct Room {
    peers: HashMap<Uuid, Peer>,
    /// Последние реплики. Живут, пока в комнате есть хоть кто-то: пустая
    /// комната удаляется целиком, иначе история копилась бы вечно.
    history: VecDeque<ChatMessage>,
}

impl Room {
    /// Собирает цитату по идентификатору сообщения.
    ///
    /// Если исходное сообщение уже вытеснено из истории, ответ уходит без
    /// цитаты: терять сообщение целиком из-за этого было бы обидно.
    fn quote(&self, id: Uuid) -> Option<ReplyPreview> {
        let message = self.history.iter().find(|message| message.id == id)?;
        let excerpt = if message.text.is_empty() {
            match &message.attachment {
                Some(attachment) => attachment.name.clone(),
                None => String::new(),
            }
        } else {
            message.text.chars().take(REPLY_EXCERPT_CHARS).collect()
        };
        Some(ReplyPreview {
            id: message.id,
            nickname: message.from.nickname.clone(),
            excerpt,
        })
    }

    /// Запоминает реплику и рассылает её. Одним движением, чтобы история и то,
    /// что видят участники, не могли разойтись.
    fn post(&mut self, message: ChatMessage, limit: usize) {
        if limit > 0 {
            if self.history.len() >= limit {
                self.history.pop_front();
            }
            self.history.push_back(message.clone());
        }
        self.broadcast(&ServerMessage::Chat(message));
    }

    /// Рассылает сообщение всем в комнате и заодно выкидывает тех, чья задача
    /// уже умерла — иначе `HashMap` копил бы мусор при долгой работе.
    ///
    /// Вызывается под захваченным мьютексом: `unbounded_send` не блокирует,
    /// так что держать лок на время рассылки безопасно.
    fn broadcast(&mut self, msg: &ServerMessage) {
        self.peers
            .retain(|_, peer| peer.outbox.send(msg.clone()).is_ok());
    }
}

/// Захватывает мьютекс, переживая отравление.
///
/// Стандартный `lock().unwrap()` после паники в любой задаче под этим локом
/// роняет каждое следующее обращение — сервер уходит в лавину падений и
/// перестаёт принимать подключения. Для чата это неверный размен: данные
/// внутри — просто список комнат, продолжать с ним безопаснее, чем умереть.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Все комнаты сервера.
pub struct Hub {
    config: HubConfig,
    /// Открытые сокеты, в том числе не прошедшие `Join`. Отдельно от `rooms`,
    /// потому что до `Join` соединение ещё не принадлежит ни одной комнате.
    sockets: AtomicUsize,
    rooms: Mutex<HashMap<String, Room>>,
}

/// Занятый слот соединения. Освобождается при уничтожении — так счётчик не
/// разъедется ни на одном из путей выхода из обработчика.
pub struct SocketSlot(Arc<Hub>);

impl Drop for SocketSlot {
    fn drop(&mut self) {
        self.0.sockets.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new(HubConfig::default())
    }
}

impl Hub {
    pub fn new(config: HubConfig) -> Self {
        Self {
            config,
            sockets: AtomicUsize::new(0),
            rooms: Mutex::new(HashMap::new()),
        }
    }

    /// Занимает слот соединения. `None` — сервер полон.
    ///
    /// `fetch_update` вместо «прочитать, сравнить, увеличить»: иначе десяток
    /// одновременных подключений проскочит мимо лимита.
    fn reserve_socket(self: &Arc<Self>) -> Option<SocketSlot> {
        let max = self.config.max_sockets;
        self.sockets
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(SocketSlot(Arc::clone(self)))
    }

    /// Заводит пользователя в комнату и возвращает тех, кто там уже был.
    ///
    /// Проверка ника, рассылка `UserJoined` и вставка идут под одним захватом
    /// мьютекса. Разорвать их нельзя дважды: два одинаковых ника проскочили бы
    /// проверку одновременно, а новичок мог бы пропустить чужой `UserJoined`,
    /// пришедший между снимком списка и собственной регистрацией.
    fn join(
        &self,
        room_name: &str,
        user: UserInfo,
        outbox: Outbox,
    ) -> Result<Joined, (ErrorCode, String)> {
        let mut rooms = lock(&self.rooms);

        match rooms.get(room_name) {
            Some(room) => {
                if room.peers.len() >= self.config.max_room_users {
                    return Err((
                        ErrorCode::RoomFull,
                        format!("в комнате уже {} человек", room.peers.len()),
                    ));
                }
                // Сравнение регистронезависимое: иначе Alice и alice — разные
                // ники, и подменить собеседника становится тривиально.
                let key = validate::nickname_key(&user.nickname);
                let taken = room
                    .peers
                    .values()
                    .any(|peer| validate::nickname_key(&peer.user.nickname) == key);
                if taken {
                    return Err((
                        ErrorCode::NicknameTaken,
                        format!("ник {} в этой комнате уже занят", user.nickname),
                    ));
                }
            }
            None if rooms.len() >= self.config.max_rooms => {
                return Err((ErrorCode::ServerFull, "слишком много комнат".to_string()));
            }
            None => {}
        }

        let room = rooms.entry(room_name.to_string()).or_default();
        // Сначала уведомляем старожилов, потом снимаем список: так новичок не
        // получит UserJoined про самого себя и не увидит в списке того, чьё
        // соединение только что отвалилось.
        room.broadcast(&ServerMessage::UserJoined { user: user.clone() });
        let others = room.peers.values().map(|peer| peer.user.clone()).collect();
        let history = room.history.iter().cloned().collect();
        room.peers.insert(
            user.id,
            Peer {
                user,
                outbox,
                last_typing: None,
            },
        );
        Ok(Joined { others, history })
    }

    /// Убирает пользователя из комнаты и сообщает об этом остальным.
    fn leave(&self, room_name: &str, id: Uuid) {
        let mut rooms = lock(&self.rooms);
        let Some(room) = rooms.get_mut(room_name) else {
            return;
        };
        let Some(peer) = room.peers.remove(&id) else {
            return;
        };
        room.broadcast(&ServerMessage::UserLeft { user: peer.user });
        // Опустевшая комната удаляется сразу, иначе список комнат растёт
        // до бесконечности при долгой работе сервера.
        if room.peers.is_empty() {
            rooms.remove(room_name);
        }
    }

    /// Раздаёт «печатает» остальным в комнате.
    ///
    /// Слишком частые сообщения отбрасываются: показывать это чаще раза в
    /// секунду всё равно нечего, а нагрузку клиент с ошибкой создать может.
    fn typing(&self, room_name: &str, user: &UserInfo) {
        const MIN_GAP: Duration = Duration::from_secs(1);

        let mut rooms = lock(&self.rooms);
        let Some(room) = rooms.get_mut(room_name) else {
            return;
        };
        let Some(peer) = room.peers.get_mut(&user.id) else {
            return;
        };

        let now = std::time::Instant::now();
        if peer
            .last_typing
            .is_some_and(|last| now.duration_since(last) < MIN_GAP)
        {
            return;
        }
        peer.last_typing = Some(now);

        // Самому себе «печатает» не нужно.
        let message = ServerMessage::Typing { user: user.clone() };
        room.peers
            .retain(|id, peer| *id == user.id || peer.outbox.send(message.clone()).is_ok());
    }

    /// Кладёт реплику в комнату, доставая цитату из её же истории.
    fn post(&self, room_name: &str, mut message: ChatMessage, reply_to: Option<Uuid>) {
        let mut rooms = lock(&self.rooms);
        if let Some(room) = rooms.get_mut(room_name) {
            // Цитату собираем на сервере: клиент присылает только id, иначе
            // ему ничего не стоило бы приписать чужие слова.
            message.reply = reply_to.and_then(|id| room.quote(id));
            room.post(message, self.config.history_limit);
        }
    }

    pub fn room_count(&self) -> usize {
        lock(&self.rooms).len()
    }

    /// Снимок списка комнат для экрана входа: имя и сколько человек внутри.
    ///
    /// Пустых комнат тут не бывает — они удаляются вместе с последним ушедшим,
    /// так что список показывает ровно то, где сейчас кто-то есть. Сортировка
    /// по имени, чтобы порядок не прыгал между запросами.
    pub fn rooms_summary(&self) -> Vec<common::RoomSummary> {
        let rooms = lock(&self.rooms);
        let mut list: Vec<common::RoomSummary> = rooms
            .iter()
            .map(|(name, room)| common::RoomSummary {
                name: name.clone(),
                users: room.peers.len(),
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    pub fn user_count(&self, room_name: &str) -> usize {
        lock(&self.rooms)
            .get(room_name)
            .map_or(0, |room| room.peers.len())
    }
}

/// Что вошедший узнаёт о комнате в момент входа.
struct Joined {
    others: Vec<UserInfo>,
    history: Vec<ChatMessage>,
}

/// Участник после успешного `Join`.
struct Session {
    user: UserInfo,
    room: String,
}

/// Состояние роутера: комнаты и загруженные файлы.
#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub media: Arc<MediaStore>,
}

pub fn app() -> Router {
    app_with_hub(Arc::new(Hub::default()))
}

/// Тот же роутер, но с внешним `Hub` — удобно в тестах заглядывать в состояние.
pub fn app_with_hub(hub: Arc<Hub>) -> Router {
    app_with_state(AppState {
        hub,
        media: Arc::new(MediaStore::default()),
    })
}

pub fn app_with_state(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(script))
        .route("/style.css", get(styles))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.svg", get(icon))
        .route("/sw.js", get(service_worker))
        .route("/ws", get(ws_handler))
        .route("/rooms", get(rooms))
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/upload",
            // Свой потолок на тело: по умолчанию axum рубит запросы на 2 МБ,
            // и картинка с телефона в него не влезет.
            post(upload).layer(DefaultBodyLimit::max(validate::MAX_UPLOAD_BYTES)),
        )
        .route("/media/{id}", get(media_file))
        .with_state(state)
}

#[derive(Deserialize)]
struct UploadQuery {
    #[serde(default)]
    name: String,
}

/// Приём файла: тело — сырые байты, имя в строке запроса.
///
/// Multipart был бы лишней зависимостью: у нас всегда один файл на запрос.
async fn upload(
    State(state): State<AppState>,
    Query(query): Query<UploadQuery>,
    bytes: Bytes,
) -> Response {
    match state.media.put(&query.name, bytes) {
        Ok(attachment) => Json(attachment).into_response(),
        Err(err @ media::MediaError::TooLarge { .. }) => {
            (StatusCode::PAYLOAD_TOO_LARGE, err.to_string()).into_response()
        }
        Err(err) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, err.to_string()).into_response(),
    }
}

async fn media_file(State(state): State<AppState>, UrlPath(id): UrlPath<Uuid>) -> Response {
    let Some(file) = state.media.get(id) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, file.mime),
            // Браузер не должен угадывать тип сам: иначе он может решить, что
            // перед ним разметка, и исполнить её на нашем же адресе.
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            // Имя файла в заголовок не кладём — оно пришло от человека, а в
            // интерфейсе и так показано.
            (header::CONTENT_DISPOSITION, "inline"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        file.bytes,
    )
        .into_response()
}

// Веб-клиент вшит в бинарь, а не читается с диска: сервер запускается из любого
// каталога и переносится одним файлом. Цена — правка вёрстки требует пересборки.

async fn index() -> impl IntoResponse {
    Html(include_str!("../../web/index.html"))
}

async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../../web/app.js"),
    )
}

async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../web/style.css"),
    )
}

async fn manifest() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/manifest+json; charset=utf-8",
        )],
        include_str!("../../web/manifest.webmanifest"),
    )
}

async fn icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        include_str!("../../web/icon.svg"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            // Обработчик должен управлять всем сайтом, а не только каталогом,
            // из которого отдан, — иначе установка на домашний экран не
            // предложится.
            (
                header::HeaderName::from_static("service-worker-allowed"),
                "/",
            ),
        ],
        include_str!("../../web/sw.js"),
    )
}

/// Список активных комнат — чтобы клиент показал их на экране входа, а человек
/// зашёл в нужную, ни у кого не спрашивая адрес. Пустых комнат тут нет: они на
/// сервере не живут.
async fn rooms(State(state): State<AppState>) -> Json<Vec<common::RoomSummary>> {
    Json(state.hub.rooms_summary())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    // Лимит на кадр — тот же, что знают клиенты (`common::MAX_FRAME_BYTES`):
    // без него один клиент кладёт сервер по памяти одним гигантским кадром.
    ws.max_message_size(validate::MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state))
}

type Sink = SplitSink<WebSocket, Message>;
type Stream = SplitStream<WebSocket>;

async fn handle_socket(socket: WebSocket, state: AppState) {
    let hub = &state.hub;
    let (mut sink, mut stream) = socket.split();

    let Some(_slot) = hub.reserve_socket() else {
        warn!("отказ: достигнут лимит соединений");
        let full =
            ServerMessage::error(ErrorCode::ServerFull, "сервер перегружен, попробуйте позже");
        let _ = send(&mut sink, &full).await;
        let _ = sink.close().await;
        return;
    };

    let id = Uuid::new_v4();
    let (outbox, mut inbox) = mpsc::unbounded_channel();

    let Some(session) = join_phase(hub, id, &outbox, &mut sink, &mut stream).await else {
        let _ = sink.close().await;
        return;
    };
    info!(
        %id,
        nickname = %session.user.nickname,
        room = %session.room,
        "вошёл в комнату",
    );

    loop {
        tokio::select! {
            // Всё исходящее идёт через собственный outbox — включая ответы об
            // ошибках. Прямая запись в sink могла бы обогнать рассылку, и клиент
            // увидел бы сообщения не в том порядке, в каком они возникли.
            outgoing = inbox.recv() => {
                let Some(msg) = outgoing else { break };
                if send(&mut sink, &msg).await.is_err() {
                    break;
                }
            }
            incoming = stream.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if !handle_text(&state, &session, &outbox, text.as_str()) {
                        break;
                    }
                }
                Some(Ok(Message::Binary(_))) => {
                    let err = ServerMessage::error(
                        ErrorCode::InvalidMessage,
                        "ожидается текстовый JSON-кадр",
                    );
                    let _ = outbox.send(err);
                }
                // Ping/Pong уровня самого WebSocket axum отвечает сам;
                // прикладной keepalive — это ClientMessage::Ping.
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    debug!(%id, %err, "соединение оборвалось");
                    break;
                }
                None => break,
            },
        }
    }

    hub.leave(&session.room, id);
    info!(%id, room = %session.room, "вышел");
}

/// Ждёт `Join` до истечения таймаута.
///
/// Неудачная попытка (занятый или кривой ник) соединение не рвёт: клиент может
/// переспросить ник у человека и попробовать снова, не переподключаясь.
async fn join_phase(
    hub: &Arc<Hub>,
    id: Uuid,
    outbox: &Outbox,
    sink: &mut Sink,
    stream: &mut Stream,
) -> Option<Session> {
    // Дедлайн на всю фазу, а не на отдельный кадр: иначе клиент может вечно
    // слать мусор, каждый раз сбрасывая таймер.
    let deadline = Instant::now() + hub.config.join_timeout;

    loop {
        let frame = match timeout_at(deadline, stream.next()).await {
            Err(_) => {
                debug!(%id, "join не пришёл вовремя, закрываю соединение");
                let err = ServerMessage::error(ErrorCode::NotJoined, "join не получен вовремя");
                let _ = send(sink, &err).await;
                return None;
            }
            Ok(None) | Ok(Some(Err(_))) => return None,
            Ok(Some(Ok(frame))) => frame,
        };

        let text = match frame {
            Message::Text(text) => text,
            Message::Close(_) => return None,
            Message::Binary(_) => {
                let err = ServerMessage::error(
                    ErrorCode::InvalidMessage,
                    "ожидается текстовый JSON-кадр",
                );
                send(sink, &err).await.ok()?;
                continue;
            }
            _ => continue,
        };

        let msg = match serde_json::from_str::<ClientMessage>(text.as_str()) {
            Ok(msg) => msg,
            Err(err) => {
                debug!(%id, %err, "нераспознанный кадр");
                let err = ServerMessage::error(
                    ErrorCode::InvalidMessage,
                    "не удалось разобрать сообщение",
                );
                send(sink, &err).await.ok()?;
                continue;
            }
        };

        let (nickname, room) = match msg {
            ClientMessage::Join { nickname, room } => (nickname, room),
            ClientMessage::Leave => return None,
            // До входа печатать некуда: комнаты ещё нет.
            ClientMessage::Typing => continue,
            ClientMessage::Ping => {
                send(sink, &ServerMessage::Pong).await.ok()?;
                continue;
            }
            ClientMessage::Chat { .. } => {
                let err =
                    ServerMessage::error(ErrorCode::NotJoined, "сначала нужно войти в комнату");
                send(sink, &err).await.ok()?;
                continue;
            }
        };

        let nickname = match validate::clean_nickname(&nickname) {
            Ok(nickname) => nickname,
            Err(err) => {
                let err = ServerMessage::error(ErrorCode::InvalidNickname, err.to_string());
                send(sink, &err).await.ok()?;
                continue;
            }
        };
        let room = match validate::clean_room(&room) {
            Ok(room) => room,
            Err(err) => {
                let err = ServerMessage::error(ErrorCode::InvalidRoom, err.to_string());
                send(sink, &err).await.ok()?;
                continue;
            }
        };

        let user = UserInfo {
            id,
            nickname: nickname.clone(),
        };
        let joined = match hub.join(&room, user.clone(), outbox.clone()) {
            Ok(joined) => joined,
            Err((code, message)) => {
                send(sink, &ServerMessage::error(code, message))
                    .await
                    .ok()?;
                continue;
            }
        };

        // Welcome пишется в sink напрямую и до того, как начнёт разбираться
        // inbox, — значит он гарантированно первый, а всё, что прилетело в
        // комнату сразу после join, придёт следом и в правильном порядке.
        let welcome = ServerMessage::Welcome {
            your_id: id,
            room: room.clone(),
            // Ник возвращаем очищенный: клиент должен показывать его, а не свой ввод.
            nickname,
            users: joined.others,
            history: joined.history,
        };
        if send(sink, &welcome).await.is_err() {
            hub.leave(&room, id);
            return None;
        }
        return Some(Session { user, room });
    }
}

/// Обрабатывает один входящий кадр участника. `false` — пора закрывать соединение.
fn handle_text(state: &AppState, session: &Session, outbox: &Outbox, text: &str) -> bool {
    let msg = match serde_json::from_str::<ClientMessage>(text) {
        Ok(msg) => msg,
        Err(err) => {
            debug!(id = %session.user.id, %err, "нераспознанный кадр");
            let err =
                ServerMessage::error(ErrorCode::InvalidMessage, "не удалось разобрать сообщение");
            let _ = outbox.send(err);
            return true;
        }
    };

    match msg {
        ClientMessage::Chat {
            text,
            attachment,
            reply_to,
        } => {
            // Клиент присылает только идентификатор файла: имя, размер и тип
            // берём из хранилища, иначе подпись под чужой картинкой подделал бы
            // кто угодно.
            let attachment = match attachment.map(|id| state.media.describe(id)) {
                Some(Some(found)) => Some(found),
                Some(None) => {
                    let _ = outbox.send(ServerMessage::error(
                        ErrorCode::InvalidMessage,
                        "вложение не найдено, загрузите файл заново",
                    ));
                    return true;
                }
                None => None,
            };

            let text = match validate::clean_text(&text) {
                Ok(text) => text,
                // Картинка без подписи — обычное дело, а вот пустое сообщение
                // без вложения смысла не имеет.
                Err(validate::ValidationError::Empty { .. }) if attachment.is_some() => {
                    String::new()
                }
                Err(err) => {
                    let _ = outbox.send(ServerMessage::error(
                        ErrorCode::InvalidMessage,
                        err.to_string(),
                    ));
                    return true;
                }
            };

            state.hub.post(
                &session.room,
                ChatMessage {
                    id: Uuid::new_v4(),
                    from: session.user.clone(),
                    text,
                    // Время ставит только сервер: часам клиентов доверия нет.
                    ts: common::now_ms(),
                    attachment,
                    reply: None,
                },
                reply_to,
            );
        }
        ClientMessage::Typing => state.hub.typing(&session.room, &session.user),
        ClientMessage::Ping => {
            let _ = outbox.send(ServerMessage::Pong);
        }
        ClientMessage::Leave => return false,
        ClientMessage::Join { .. } => {
            let err = ServerMessage::error(
                ErrorCode::AlreadyJoined,
                "смена комнаты в одном соединении не поддерживается",
            );
            let _ = outbox.send(err);
        }
    }
    true
}

async fn send(sink: &mut Sink, msg: &ServerMessage) -> Result<(), axum::Error> {
    let json = serde_json::to_string(msg).expect("ServerMessage сериализуется всегда");
    sink.send(Message::Text(json.into())).await
}
