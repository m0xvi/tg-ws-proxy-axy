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

## 🛠 Сборка

### 1. Проверка единого Rust-ядра и FFI-контракта
```bash
cargo check --locked
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
