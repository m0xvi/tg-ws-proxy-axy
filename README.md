# TG WS Proxy Universal (Android & iOS)

Единый кроссплатформенный репозиторий локального MTProto/WebSocket-прокси для Telegram под **Android** и **iOS** с общим высокопроизводительным ядром на **Rust** (`libtgwsproxy`).

Проект объединяет наработки:
- [Flowseal/tg-ws-proxy](https://github.com/Flowseal/tg-ws-proxy) — оригинальная концепция локального MTProto → WebSocket прокси.
- [amurcanov/tg-ws-proxy-android](https://github.com/amurcanov/tg-ws-proxy-android) — Rust-ядро и нативное Android-приложение (Jetpack Compose / Material 3).
- [IMDelewer/tg-ws-proxy-ios](https://github.com/IMDelewer/tg-ws-proxy-ios) — нативное iOS-приложение (SwiftUI + Network Extension `PacketTunnel` + `CoreLocation` BackgroundKeeper).

---

## 📁 Структура единого репозитория

```text
tg-ws-proxy/
├── Cargo.toml                 # Единый манифест Rust-ядра (собирается в .so для Android и .a для iOS)
├── Cargo.lock
├── src/                       # Общее Rust-ядро (MTProto, WebSocket, Cloudflare, FakeTLS, DNS/DoH, балансировка)
│   ├── lib.rs                 # Единый C FFI интерфейс (совместим с Android JNA и iOS Bridging Header)
│   ├── proxy.rs               # Локальный TCP-сервер MTProto и пулы соединений
│   ├── ws.rs                  # WebSocket/TLS транспорт, SNI fronting, обход блокировок
│   ├── cfproxy.rs             # Проксирование через Cloudflare и DoH-резолвер
│   ├── faketls.rs             # Поддержка ee-секретов (FakeTLS)
│   ├── balancer.rs            # Балансировщик доменов по датацентрам Telegram (DC 1..5, 203)
│   ├── crypto.rs              # AES-CTR obfuscated2 и маскирование фреймов
│   └── config.rs              # Константы, атомарные флаги и настройки
├── app/                       # Android-приложение (Kotlin, Jetpack Compose, ForegroundService, Quick Settings Tile)
│   └── src/main/jniLibs/      # Скомпилированные libtgwsproxy.so (arm64-v8a, armeabi-v7a)
├── ios/                       # iOS-приложение (SwiftUI) и сетевое расширение
│   ├── TgWsProxy/             # Основное приложение + BackgroundKeeper + C Bridging Header
│   └── PacketTunnel/          # Network Extension (PacketTunnelProvider для фоновой работы)
├── TgWsProxy.xcodeproj/       # Проект Xcode (вызывает scripts/build-rust-ios.sh при сборке)
├── scripts/
│   ├── build-rust-android.sh  # Сборка Rust-ядра под Android (Linux/macOS/CI через cargo-ndk)
│   ├── build-rust-ios.sh      # Сборка Rust-ядра под iOS (aarch64-apple-ios / симулятор)
│   └── run-ffi-smoke.sh       # Сквозной смоук-тест C FFI контракта
├── build_so.bat               # Сборка Rust-ядра под Android на Windows
├── build_apk.bat              # Сборка APK на Windows
├── build.sh                   # Сборка IPA (TgWsProxy-free.ipa и TgWsProxy-vpn.ipa) на macOS
├── tests/
│   └── ffi_smoke.c            # Тест C FFI вызовов ядра
└── docs/                      # Документация по архитектуре
```

---

## 📶 Устойчивая работа в мобильных сетях (LTE / 5G / Wi-Fi)

Единое Rust-ядро включает комплекс доработок для стабильной работы на мобильных операторах (Билайн, МТС, МегаФон, T2, Yota и региональные сети):

1. **DPI-фрагментация без root и внешнего VPN (`DpiBypassStream`):**
   - **На TLS-соединениях (`wss://`, порты `443`, `8443`, `2053`):** первый пакет `TLS ClientHello` (`0x16 0x03 ...`) автоматически разбивается на 3 отдельных валидных TLS-рекорда и TCP-сегмента, причём разрез приходится ровно на середину SNI-домена.
   - **На HTTP-соединениях (`ws://`, порты `80`, `2052`, `8080`):** HTTP Upgrade-запрос сегментируется по TCP прямо посреди метода `GET` и имени хоста с обфускацией регистра заголовка (`hOsT:`).
2. **Исправление прямого подключения к серверам Telegram (`kws1..5.web.telegram.org`):**
   - Прямые подключения к датацентрам Telegram всегда идут по TLS (порт `443`) независимо от настройки `DISABLE_SECURE` для Cloudflare, с автоматическим перебором пула российских SNI для Domain Fronting (`sprinthost.ru`, `selectel.ru`, `reg.ru`, `timeweb.com`, `vk.com`, `ya.ru`).
3. **Мультипортовый фоллбэк и чистые Anycast IP Cloudflare (`IPv4` + `IPv6`):**
   - При блокировке оператором стандартного порта или подсети Cloudflare (`104.21.0.0/16`, `172.67.0.0/16`) ядро автоматически запускает параллельную гонку (`race_cf_mobile_endpoints`) по альтернативным портам Cloudflare (`80`, `2052`, `8080`, `443`, `8443`) и чистым Anycast IP (`104.16.x.x`, `162.159.x.x`, `172.64.x.x`, `2606:4700::6810:84e5`) с кешированием самого быстрого маршрута (`CF_FAST_ROUTE`).
4. **Устойчивый DoH-резолвер для мобильных сетей РФ:**
   - Параллельный опрос `https://common.dns.yandex.net/dns-query` (доступен даже при ограничениях внешних DNS/CDN), `https://8.8.8.8/dns-query`, Google, AdGuard, Cloudflare DoH и системного резолвера с поддержкой `A` и `AAAA` (`[ipv6]:port`).
5. **Универсальная поддержка своих доменов и Cloudflare Workers:**
   - В поле домена Cloudflare можно указать **любой** пользовательский домен (`.ru`, `.com`, `.xyz`), домен Cloudflare Worker (`*.workers.dev`, `*.pages.dev`), Yandex Cloud Serverless или полный URL — ядро автоматически очистит схему и определит режим работы (`kws1..5.{domain}` или `/apiws?dst=...&dc=...`).
6. **Мгновенная реакция на смену сети (`Wi-Fi ↔ Mobile Data`):**
   - На Android (`ConnectivityManager.NetworkCallback`) и iOS (`NWPathMonitor`) смена сетевого интерфейса вызывает `NotifyNetworkChanged()`, мгновенно сбрасывая устаревшие кеши DNS/маршрутов, кулдауны DC и пересоздавая пулы сокетов.

---

## 🛠 Сборка

### 1. Проверка единого Rust-ядра и FFI-контракта
```bash
cargo test --release --lib
./scripts/run-ffi-smoke.sh
```

### 2. Сборка под Android
**На Linux / macOS:**
```bash
./scripts/build-rust-android.sh
./gradlew assembleDebug
```
**На Windows:**
```bat
build_so.bat
build_apk.bat
```

### 3. Сборка под iOS (требуется macOS + Xcode)
```bash
rustup target add aarch64-apple-ios
./build.sh                  # dist/TgWsProxy-free.ipa (работа в фоне через CoreLocation, любой Apple ID)
./build.sh --vpn            # dist/TgWsProxy-vpn.ipa  (работа в фоне через Network Extension / TrollStore / Paid Dev)
./build.sh --sim --install  # сборка и запуск в iOS Simulator
```
