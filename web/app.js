"use strict";

// Веб-клиент говорит на том же протоколе, что и терминальный: те же типы
// сообщений, те же коды ошибок, тот же keepalive. Сервер про фронтенды не знает.

const PING_EVERY = 30000;
const PONG_TIMEOUT = 10000;
const FIRST_BACKOFF = 1000;
const MAX_BACKOFF = 30000;
/// Тот же потолок, что и на сервере: понятная ошибка лучше, чем отказ в ответ
/// на пять мегабайт, уже уехавших по сети.
const MAX_UPLOAD_BYTES = 5 * 1024 * 1024;
/// Две минуты записи с запасом влезают в лимит загрузки, а заодно спасают от
/// забытой включённой кнопки.
const MAX_RECORD_MS = 120000;
/// Как часто сообщаем, что печатаем, и сколько показываем чужое «печатает».
const TYPING_EVERY = 2000;
const TYPING_TTL = 4000;

/// Ошибки, которые не лечатся переподключением: спрашиваем ник заново.
const FATAL = new Set(["nickname_taken", "invalid_nickname", "invalid_room"]);

const el = (id) => document.getElementById(id);

const ui = {
  join: el("join"),
  nickname: el("nickname"),
  room: el("room"),
  joinError: el("join-error"),
  rooms: el("rooms"),
  roomsList: el("rooms-list"),
  chat: el("chat"),
  roomName: el("room-name"),
  people: el("people"),
  attach: el("attach"),
  file: el("file"),
  record: el("record"),
  alerts: el("alerts"),
  users: el("users"),
  status: el("status"),
  typing: el("typing"),
  reply: el("reply"),
  replyText: el("reply-text"),
  replyCancel: el("reply-cancel"),
  messages: el("messages"),
  composer: el("composer"),
  text: el("text"),
};

const state = {
  socket: null,
  nickname: "",
  room: "",
  me: null,
  joined: false,
  users: [],
  // id уже показанных реплик: история комнаты после переподключения
  // накладывается на уже увиденное, и дубли надо отбрасывать.
  seen: new Set(),
  unread: 0,
  /// Сообщение, на которое готовится ответ.
  replyTo: null,
  /// Кто печатает: id -> { nickname, at }.
  typing: new Map(),
  typingSent: 0,
  attempt: 0,
  closing: false,
  timers: { reconnect: null, ping: null, pong: null },
  alerts: false,
};

const recorder = {
  media: null,
  stream: null,
  chunks: [],
  startedAt: 0,
  timer: null,
};

function wsUrl() {
  // Адрес берём из страницы: тогда телефон, открывший http://192.168.x.x:8080,
  // подключится к тому же серверу без единой правки в коде.
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${location.host}/ws`;
}

function send(message) {
  if (state.socket && state.socket.readyState === WebSocket.OPEN) {
    state.socket.send(JSON.stringify(message));
    return true;
  }
  return false;
}

function clearTimers() {
  for (const name of ["reconnect", "ping", "pong"]) {
    clearTimeout(state.timers[name]);
    clearInterval(state.timers[name]);
    state.timers[name] = null;
  }
}

function startKeepalive() {
  // Простаивающее соединение молча рвут роутеры и NAT: без ping клиент узнает
  // об этом только при попытке что-то отправить.
  state.timers.ping = setInterval(() => {
    if (!send({ type: "ping" })) return;
    clearTimeout(state.timers.pong);
    state.timers.pong = setTimeout(() => {
      if (state.socket) state.socket.close();
    }, PONG_TIMEOUT);
  }, PING_EVERY);
}

function connect() {
  clearTimers();
  setStatus(
    state.attempt === 0
      ? "подключение…"
      : `подключение, попытка ${state.attempt + 1}…`,
  );

  const socket = new WebSocket(wsUrl());
  state.socket = socket;

  socket.onopen = () => {
    send({ type: "join", nickname: state.nickname, room: state.room });
    startKeepalive();
  };

  socket.onmessage = (event) => {
    let message;
    try {
      message = JSON.parse(event.data);
    } catch {
      return; // непонятный кадр — не повод падать
    }
    handle(message);
  };

  socket.onclose = () => {
    if (state.closing) return;
    scheduleReconnect();
  };

  // onerror всегда сопровождается onclose — там и переподключаемся.
  socket.onerror = () => {};
}

function scheduleReconnect() {
  clearTimers();
  const wait = Math.min(FIRST_BACKOFF * 2 ** state.attempt, MAX_BACKOFF);
  state.attempt += 1;
  if (state.joined) {
    state.joined = false;
    addSystem("соединение потеряно", true);
  }
  // Список участников устарел, а показывать устаревший хуже, чем пустой.
  state.users = [];
  showUsers();
  setStatus(`нет связи · повтор через ${Math.round(wait / 1000)}с`);
  state.timers.reconnect = setTimeout(connect, wait);
}

function handle(message) {
  switch (message.type) {
    case "welcome": {
      const reconnected = state.me !== null;
      state.me = message.your_id;
      state.room = message.room;
      state.nickname = message.nickname;
      state.joined = true;
      state.attempt = 0;
      state.users = [...message.users, { id: message.your_id, nickname: message.nickname }];
      remember();
      ui.roomName.textContent = `#${message.room}`;
      showUsers();
      setStatus(null);

      // История комнаты: при первом входе заполняет пустой экран, при
      // переподключении возвращает пропущенное. Дубли отсекаются по id.
      for (const item of message.history) addChat(item);

      addSystem(
        reconnected
          ? "соединение восстановлено"
          : `вы вошли как ${message.nickname}`,
      );
      break;
    }
    case "user_joined":
      state.users.push(message.user);
      showUsers();
      addSystem(`${message.user.nickname} вошёл в комнату`);
      break;
    case "user_left":
      state.users = state.users.filter((user) => user.id !== message.user.id);
      showUsers();
      addSystem(`${message.user.nickname} вышел`);
      break;
    case "typing":
      state.typing.set(message.user.id, {
        nickname: message.user.nickname,
        at: Date.now(),
      });
      showTyping();
      break;
    case "chat":
      addChat(message);
      break;
    case "error":
      if (!state.joined && FATAL.has(message.code)) {
        state.closing = true;
        clearTimers();
        if (state.socket) state.socket.close();
        showJoinScreen(message.message);
        return;
      }
      addSystem(message.message, true);
      break;
    case "pong":
      clearTimeout(state.timers.pong);
      break;
  }
}

function nearBottom() {
  const gap =
    ui.messages.scrollHeight - ui.messages.scrollTop - ui.messages.clientHeight;
  return gap < 60;
}

function append(node) {
  // Прокручиваем вниз только если человек и так внизу: иначе он не сможет
  // спокойно перечитать историю во время активной переписки.
  const stick = nearBottom();
  ui.messages.append(node);
  if (stick) ui.messages.scrollTop = ui.messages.scrollHeight;
}

/// Цвет ника считается тем же хешем, что в терминальном клиенте, поэтому один
/// и тот же человек выглядит одинаково и в терминале, и в браузере.
function nickColor(nickname) {
  const bytes = new TextEncoder().encode(nickname.toLowerCase());
  let hash = 0;
  for (const byte of bytes) hash = (Math.imul(hash, 31) + byte) >>> 0;
  return `var(--nick-${hash % 6})`;
}

function mentionsMe(text) {
  return (
    state.nickname !== "" &&
    text.toLowerCase().includes(state.nickname.toLowerCase())
  );
}

function addChat(message) {
  // Раз пришло сообщение, печатать человек закончил.
  if (state.typing.delete(message.from.id)) showTyping();
  // Уже показанную реплику пропускаем: так история после обрыва не двоится.
  if (state.seen.has(message.id)) return;
  state.seen.add(message.id);

  const item = document.createElement("li");
  const mine = message.from.id === state.me;
  if (mine) item.className = "mine";
  else if (mentionsMe(message.text)) item.className = "mention";

  const time = document.createElement("span");
  time.className = "time";
  // Сервер присылает UTC в миллисекундах, показываем в местном времени.
  time.textContent = new Date(message.ts).toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
  });

  const nick = document.createElement("span");
  nick.className = "nick";
  // Угловые скобки — привычная запись из IRC и терминальных клиентов.
  nick.textContent = `<${message.from.nickname}>`;
  nick.style.color = nickColor(message.from.nickname);
  // Ответ по клику именно на ник: клик по всему сообщению мешал бы выделять
  // текст мышью.
  nick.title = "Ответить";
  nick.addEventListener("click", () => setReply(message));

  const text = document.createElement("span");
  // Только textContent: с innerHTML чужое сообщение стало бы разметкой.
  text.textContent = message.text;

  // Цитата идёт первой строкой: сначала на что отвечают, потом что отвечают.
  if (message.reply) item.append(quoteNode(message.reply));
  item.append(time, nick, text);
  if (message.attachment) item.append(attachmentNode(message.attachment));
  append(item);
  if (!mine) {
    countUnread();
    if (item.className === "mention") notifyMention(message);
  }
}

function quoteNode(reply) {
  const quote = document.createElement("span");
  quote.className = "quote";
  quote.textContent = `${reply.nickname}: ${reply.excerpt}`;
  return quote;
}

/// Взводит ответ: с сервером уйдёт только идентификатор, цитату он соберёт сам.
function setReply(message) {
  state.replyTo = message.id;
  ui.reply.hidden = false;
  const excerpt = message.text || message.attachment?.name || "";
  ui.replyText.textContent = `↩ ${message.from.nickname}: ${excerpt}`;
  ui.text.focus();
}

function clearReply() {
  state.replyTo = null;
  ui.reply.hidden = true;
  ui.replyText.textContent = "";
}

/// Размер файла по-человечески: «240 КБ» читается, «245760» — нет.
function humanSize(bytes) {
  if (!Number.isFinite(bytes)) return "";
  if (bytes < 1024) return `${bytes} Б`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} КБ`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} МБ`;
}

function attachmentNode(attachment) {
  const url = `/media/${attachment.id}`;
  if (attachment.kind === "audio") {
    const audio = document.createElement("audio");
    audio.src = url;
    audio.controls = true;
    // Метаданные нужны, чтобы плеер сразу показал длительность.
    audio.preload = "metadata";
    return audio;
  }
  if (attachment.kind !== "image") {
    // Обычный файл: показывать нечего, поэтому даём его скачать. Имя и размер
    // рядом — по ним видно, стоит ли качать вообще.
    const link = document.createElement("a");
    link.className = "file";
    link.href = url;
    // Сервер и так отдаёт такой файл с Content-Disposition: attachment, но
    // download просит браузер сохранить его под настоящим именем.
    link.download = attachment.name;
    link.textContent = `▤ ${attachment.name} · ${humanSize(attachment.size)}`;
    return link;
  }

  const image = document.createElement("img");
  image.src = url;
  // alt важен не только для доступности: если файл вытеснен из хранилища,
  // на его месте останется имя, а не пустой прямоугольник.
  image.alt = attachment.name;
  image.loading = "lazy";
  image.addEventListener("click", () => window.open(url, "_blank"));
  return image;
}

/// Загружает файл и сразу отправляет его в комнату, подписав тем, что набрано
/// в поле ввода.
async function uploadAndSend(file) {
  if (!file) return;
  if (file.size > MAX_UPLOAD_BYTES) {
    addSystem(
      `${file.name}: слишком большой файл, максимум ${MAX_UPLOAD_BYTES / 1024 / 1024} МБ`,
      true,
    );
    return;
  }
  if (!state.joined) {
    addSystem("нет соединения, файл не отправлен", true);
    return;
  }

  setStatus(`загружаю ${file.name}…`);
  ui.attach.disabled = true;
  try {
    const response = await fetch(
      `/upload?name=${encodeURIComponent(file.name)}`,
      { method: "POST", body: file },
    );
    if (!response.ok) {
      addSystem(await response.text(), true);
      return;
    }
    const attachment = await response.json();
    // Подпись берём из поля ввода: так картинка и комментарий к ней
    // оказываются одним сообщением, а не двумя.
    const text = ui.text.value.trim();
    if (send({ type: "chat", text, attachment: attachment.id, reply_to: state.replyTo })) {
      ui.text.value = "";
      clearReply();
    } else {
      addSystem("нет соединения, файл не отправлен", true);
    }
  } catch {
    addSystem("не удалось загрузить файл", true);
  } finally {
    ui.attach.disabled = false;
    setStatus(state.joined ? null : "нет связи");
  }
}

function addSystem(text, bad = false) {
  const item = document.createElement("li");
  item.className = bad ? "system bad" : "system";
  item.textContent = `-!- ${text}`;
  append(item);
}

function showUsers() {
  ui.people.textContent = `${state.users.length} в комнате`;
  ui.users.replaceChildren();
  for (const user of [...state.users].sort((a, b) =>
    a.nickname.localeCompare(b.nickname),
  )) {
    const item = document.createElement("li");
    const mine = user.id === state.me;
    if (mine) item.className = "me";
    item.textContent = mine ? `${user.nickname} (вы)` : user.nickname;
    if (!mine) item.style.color = nickColor(user.nickname);
    ui.users.append(item);
  }
}

/// Показывает, кто печатает. Сигнал одноразовый и гаснет сам — отдельного
/// «я перестал печатать» в протоколе нет, и терять его нечего.
function showTyping() {
  const now = Date.now();
  for (const [id, entry] of state.typing) {
    if (now - entry.at > TYPING_TTL) state.typing.delete(id);
  }

  const names = [...state.typing.values()]
    .map((entry) => entry.nickname)
    .sort();
  if (names.length === 0) {
    ui.typing.hidden = true;
    ui.typing.textContent = "";
    return;
  }

  ui.typing.hidden = false;
  ui.typing.textContent =
    names.length === 1
      ? `${names[0]} печатает`
      : names.length === 2
        ? `${names[0]} и ${names[1]} печатают`
        : `${names.length} человек печатают`;
}

// Строка гаснет сама: без этого «печатает» висело бы, пока человек не напишет.
setInterval(showTyping, 1000);

function setStatus(text) {
  ui.status.hidden = text === null;
  ui.status.textContent = text || "";
}

/// Непрочитанные выносим в заголовок вкладки: с телефона чат почти всегда
/// лежит в фоне, и иначе о новом сообщении никто не узнает.
function countUnread() {
  if (!document.hidden) return;
  state.unread += 1;
  document.title = `(${state.unread}) Чат`;
}

document.addEventListener("visibilitychange", () => {
  if (document.hidden) return;
  state.unread = 0;
  document.title = "Чат";
});

ui.text.addEventListener("input", () => {
  if (!state.joined || ui.text.value === "") return;
  const now = Date.now();
  // Реже, чем раз в две секунды, слать незачем: у получателя строка живёт
  // дольше, а сервер лишние сообщения всё равно отбросит.
  if (now - state.typingSent < TYPING_EVERY) return;
  state.typingSent = now;
  send({ type: "typing" });
});

ui.replyCancel.addEventListener("click", clearReply);

ui.people.addEventListener("click", () => {
  const shown = !ui.users.hidden;
  ui.users.hidden = shown;
  ui.people.setAttribute("aria-expanded", String(!shown));
});

function showJoinScreen(error) {
  ui.chat.hidden = true;
  ui.join.hidden = false;
  ui.joinError.hidden = !error;
  ui.joinError.textContent = error || "";
  ui.nickname.value = state.nickname || ui.nickname.value;
  ui.room.value = state.room || ui.room.value;
  ui.nickname.focus();
  ui.nickname.select();
  // Вернулись на вход — освежаем список: за время в чате комнаты могли
  // появиться или опустеть.
  loadRooms();
}

/// Ник и комната запоминаются: на телефоне вводить их заново при каждом
/// заходе — самое раздражающее, что может быть.
function remember() {
  try {
    localStorage.setItem("nickname", state.nickname);
    localStorage.setItem("room", state.room);
  } catch {
    // Приватный режим может запретить хранилище — это не повод ломаться.
  }
  const url = new URL(location.href);
  url.searchParams.set("room", state.room);
  history.replaceState(null, "", url);
}

function restore() {
  let saved = {};
  try {
    saved = {
      nickname: localStorage.getItem("nickname"),
      room: localStorage.getItem("room"),
      alerts: localStorage.getItem("alerts"),
    };
  } catch {
    saved = {};
  }
  // Комната из ссылки важнее сохранённой: по такой ссылке зовут в конкретную.
  const fromUrl = new URLSearchParams(location.search).get("room");
  if (saved.nickname) ui.nickname.value = saved.nickname;
  if (saved.alerts) setAlerts(true);
  const room = fromUrl || saved.room;
  if (room) ui.room.value = room;
  loadRooms();
}

/// Тянет список живущих комнат и показывает их на экране входа. Список —
/// удобство: не вышло получить — молча прячем, вход это не ломает.
async function loadRooms() {
  if (!ui.rooms) return;
  try {
    const response = await fetch("/rooms", { cache: "no-store" });
    if (!response.ok) throw new Error(String(response.status));
    renderRooms(await response.json());
  } catch {
    ui.rooms.hidden = true;
  }
}

function renderRooms(rooms) {
  ui.roomsList.replaceChildren();
  if (!Array.isArray(rooms) || rooms.length === 0) {
    ui.rooms.hidden = true;
    return;
  }
  for (const room of rooms) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "room-pick";

    const name = document.createElement("span");
    name.className = "room-pick-name";
    // textContent, не innerHTML: имя пришло с сервера, но пусть подделать
    // разметку через него будет нечем.
    name.textContent = room.name;

    const count = document.createElement("span");
    count.className = "room-pick-count";
    count.textContent = room.users === 1 ? "1 чел." : `${room.users} чел.`;

    button.append(name, count);
    // Тап подставляет комнату в поле — заходить решает человек кнопкой «Войти».
    button.addEventListener("click", () => {
      ui.room.value = room.name;
      ui.nickname.focus();
    });

    const item = document.createElement("li");
    item.append(button);
    ui.roomsList.append(item);
  }
  ui.rooms.hidden = false;
}

ui.join.addEventListener("submit", (event) => {
  event.preventDefault();
  state.nickname = ui.nickname.value.trim();
  state.room = ui.room.value.trim().toLowerCase();
  if (!state.nickname || !state.room) return;

  state.me = null;
  state.attempt = 0;
  state.closing = false;
  state.seen.clear();
  ui.join.hidden = true;
  ui.chat.hidden = false;
  ui.messages.replaceChildren();
  clearReply();
  ui.text.focus();
  connect();
});

ui.attach.addEventListener("click", () => ui.file.click());

ui.file.addEventListener("change", () => {
  const [file] = ui.file.files;
  // Сбрасываем значение, иначе тот же файл второй раз не выберется.
  ui.file.value = "";
  uploadAndSend(file);
});

// Скриншот из буфера — самый частый способ поделиться картинкой с компьютера.
document.addEventListener("paste", (event) => {
  if (ui.chat.hidden) return;
  const item = [...(event.clipboardData?.items || [])].find((entry) =>
    entry.type.startsWith("image/"),
  );
  if (!item) return;
  event.preventDefault();
  uploadAndSend(item.getAsFile());
});

for (const type of ["dragenter", "dragover"]) {
  ui.chat.addEventListener(type, (event) => {
    event.preventDefault();
    ui.chat.classList.add("dropping");
  });
}
for (const type of ["dragleave", "drop"]) {
  ui.chat.addEventListener(type, () => ui.chat.classList.remove("dropping"));
}
ui.chat.addEventListener("drop", (event) => {
  event.preventDefault();
  uploadAndSend(event.dataTransfer?.files?.[0]);
});

ui.composer.addEventListener("submit", (event) => {
  event.preventDefault();
  const text = ui.text.value.trim();
  if (!text) return;
  if (!send({ type: "chat", text, reply_to: state.replyTo })) {
    addSystem("нет соединения, сообщение не отправлено", true);
    return;
  }
  ui.text.value = "";
  state.typingSent = 0;
  clearReply();
});

// Уходя со страницы, прощаемся явно: иначе остальные увидят наш уход
// только после того, как сервер заметит разрыв.
window.addEventListener("pagehide", () => {
  state.closing = true;
  send({ type: "leave" });
  if (state.socket) state.socket.close();
});

// Экранная клавиатура на телефоне перекрывает поле ввода: visualViewport
// сообщает, сколько места она съела, и мы поднимаем содержимое на столько же.
if (window.visualViewport) {
  const fit = () => {
    const hidden = Math.max(
      0,
      window.innerHeight - visualViewport.height - visualViewport.offsetTop,
    );
    document.documentElement.style.setProperty("--keyboard", `${hidden}px`);
    if (nearBottom()) ui.messages.scrollTop = ui.messages.scrollHeight;
  };
  visualViewport.addEventListener("resize", fit);
  visualViewport.addEventListener("scroll", fit);
}


// ── Голосовые ───────────────────────────────────────────────────────────────

/// Расширение по типу, который выбрал браузер: Chrome пишет webm,
/// Firefox — ogg, Safari — mp4.
function audioExtension(mime) {
  if (mime.includes("ogg")) return "ogg";
  if (mime.includes("mp4")) return "m4a";
  return "webm";
}

function pickRecordingType() {
  const wanted = [
    "audio/webm;codecs=opus",
    "audio/webm",
    "audio/ogg;codecs=opus",
    "audio/mp4",
  ];
  return wanted.find((type) => MediaRecorder.isTypeSupported?.(type)) || "";
}

async function toggleRecording() {
  if (recorder.media) {
    stopRecording();
    return;
  }
  if (!state.joined) {
    addSystem("нет соединения, запись не начата", true);
    return;
  }
  // Микрофон браузеры дают только в защищённом контексте. По http это не
  // «запрет пользователя», а полное отсутствие API, и сказать об этом надо
  // прямо, иначе человек будет искать разрешение в настройках.
  if (!window.isSecureContext || !navigator.mediaDevices?.getUserMedia) {
    addSystem(
      "микрофон работает только по https — запустите сервер с CHAT_TLS=1",
      true,
    );
    return;
  }

  try {
    recorder.stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (error) {
    addSystem(
      error?.name === "NotAllowedError"
        ? "доступ к микрофону запрещён"
        : "микрофон недоступен",
      true,
    );
    return;
  }

  const type = pickRecordingType();
  recorder.chunks = [];
  recorder.media = new MediaRecorder(
    recorder.stream,
    type ? { mimeType: type } : undefined,
  );
  recorder.media.ondataavailable = (event) => {
    if (event.data.size) recorder.chunks.push(event.data);
  };
  recorder.media.onstop = finishRecording;
  recorder.media.start();

  recorder.startedAt = Date.now();
  ui.record.classList.add("recording");
  recorder.timer = setInterval(() => {
    const seconds = Math.floor((Date.now() - recorder.startedAt) / 1000);
    setStatus(`запись ${seconds} с · нажмите ещё раз, чтобы отправить`);
    if (Date.now() - recorder.startedAt > MAX_RECORD_MS) stopRecording();
  }, 250);
}

function stopRecording() {
  if (!recorder.media) return;
  recorder.media.stop();
  // Без этого индикатор записи в браузере продолжает гореть.
  recorder.stream?.getTracks().forEach((track) => track.stop());
  clearInterval(recorder.timer);
  recorder.timer = null;
  ui.record.classList.remove("recording");
  setStatus(state.joined ? null : "нет связи");
}

function finishRecording() {
  const type = recorder.media?.mimeType || "audio/webm";
  const blob = new Blob(recorder.chunks, { type });
  recorder.media = null;
  recorder.stream = null;
  recorder.chunks = [];

  // Совсем короткое нажатие — это промах по кнопке, а не сообщение.
  if (blob.size < 1024) {
    addSystem("слишком короткая запись", true);
    return;
  }
  uploadAndSend(new File([blob], `голосовое.${audioExtension(type)}`, { type }));
}

ui.record.addEventListener("click", toggleRecording);

// ── Уведомления ─────────────────────────────────────────────────────────────

let audioContext = null;

/// Короткий сигнал вместо звукового файла: он не грузится по сети и не портит
/// установку приложения лишним ресурсом.
function beep() {
  try {
    audioContext = audioContext || new (window.AudioContext || window.webkitAudioContext)();
    const oscillator = audioContext.createOscillator();
    const gain = audioContext.createGain();
    oscillator.frequency.value = 880;
    gain.gain.value = 0.04;
    oscillator.connect(gain);
    gain.connect(audioContext.destination);
    oscillator.start();
    oscillator.stop(audioContext.currentTime + 0.12);
  } catch {
    // Браузер может запретить звук до первого клика — это не повод падать.
  }
}

function notifyMention(message) {
  if (!state.alerts) return;
  beep();
  if (document.hidden && Notification.permission === "granted") {
    new Notification(`${message.from.nickname} упомянул вас`, {
      body: message.text,
      icon: "/icon.svg",
      tag: "mention",
    });
  }
}

function setAlerts(enabled) {
  state.alerts = enabled;
  ui.alerts.setAttribute("aria-pressed", String(enabled));
  try {
    localStorage.setItem("alerts", enabled ? "1" : "");
  } catch {
    // Приватный режим может запретить хранилище.
  }
}

ui.alerts.addEventListener("click", async () => {
  if (state.alerts) {
    setAlerts(false);
    return;
  }
  // Разрешение спрашиваем только по явному нажатию: непрошеный запрос
  // браузеры показывают один раз, и отказ потом не переиграть.
  if ("Notification" in window && Notification.permission === "default") {
    await Notification.requestPermission();
  }
  setAlerts(true);
  beep();
});

// ── Установка на домашний экран ─────────────────────────────────────────────

if ("serviceWorker" in navigator) {
  // Регистрация возможна только в защищённом контексте: по http телефон
  // предложить установку не сможет.
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch(() => {});
  });
}

restore();
