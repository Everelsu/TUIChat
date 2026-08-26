"use strict";

// Service worker нужен ради установки на домашний экран и ради того, чтобы
// приложение открывалось, когда сервера нет: тогда оно честно покажет «нет
// связи» вместо ошибки браузера.

const CACHE = "chat-v1";
const SHELL = ["/", "/app.js", "/style.css", "/icon.svg", "/manifest.webmanifest"];

self.addEventListener("install", (event) => {
  event.waitUntil(caches.open(CACHE).then((cache) => cache.addAll(SHELL)));
  self.skipWaiting();
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) =>
        Promise.all(keys.filter((key) => key !== CACHE).map((key) => caches.delete(key))),
      ),
  );
  self.clients.claim();
});

self.addEventListener("fetch", (event) => {
  if (event.request.method !== "GET") return;

  const url = new URL(event.request.url);
  if (url.origin !== location.origin) return;
  // Вложения не трогаем: они неизменяемы и уже кэшируются обычным заголовком,
  // а класть картинки в кэш приложения — верный способ его переполнить.
  if (!SHELL.includes(url.pathname)) return;

  // Сеть важнее кэша: правка вёрстки должна доезжать сразу, а кэш остаётся
  // страховкой на случай, когда сервер недоступен.
  event.respondWith(
    fetch(event.request)
      .then((response) => {
        const copy = response.clone();
        caches.open(CACHE).then((cache) => cache.put(event.request, copy));
        return response;
      })
      .catch(() => caches.match(event.request)),
  );
});
