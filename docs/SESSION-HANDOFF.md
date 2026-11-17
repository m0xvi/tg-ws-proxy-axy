# Session handoff (2026-11-11)

> Саммари сессии для передачи следующему агенту. Пользователь — владелец репозитория,
> общается на русском. Ключевая задача: массовое распространение iOS-приложения (KiiXY)
> клиентам **без ПК**, «перманентно» и с **обновлениями без кабеля**.

## Контекст проекта

- Репозиторий: `m0xvi/tg-ws-proxy-axy` — **KiiXY**: локальный MTProto/WS-прокси для
  Telegram под Android (Kotlin/Compose) и iOS (SwiftUI) с общим Rust-ядром `libtgwsproxy`.
- Bundle ID iOS: `com.m0xvi.kiixy` (tunnel: `com.m0xvi.kiixy.tunnel`).
- Сборки iOS (`./build.sh`, **unsigned**, `CODE_SIGNING_ALLOWED=NO`):
  - `TgWsProxy-free.ipa` — обычный app, фон через CoreLocation («Всегда»); ставится любым
    бесплатным способом.
  - `TgWsProxy-vpn.ipa` — `--vpn`, `TGWS_TUNNEL_AVAILABLE`, PacketTunnel.appex; нужен
    платный Apple Developer или TrollStore (entitlement Network Extension).
- CI (`.github/workflows/build.yml`): на push в `main` / тег `v*` / workflow_dispatch
  собирает Rust, APK, IPA (macos-15) и **автоматически создаёт GitHub Release**
  с тегом `v2.0.<github.run_number>` (именно поэтому тег релиза ≠ внутренней версии
  приложения — это штатный процесс).
- Внутренняя версия приложения (CFBundleShortVersionString) в `ios/TgWsProxy/Info.plist`
  и `ios/PacketTunnel/Info.plist` — после этой сессии: **2.0.14** (build 214).
  Текущий latest-релиз на момент хендоффа: **v2.0.16**.

## Что сделано в этой сессии (слито в main, PR #2, коммит `43ba448`)

Задача пользователя: приложение, установленное через Sideloadly (Windows, кабель,
бесплатный Apple ID), «умирало» через 7 дней и обновлялось только по кабелю.
Объяснение: 7 дней — политика Apple для бесплатного Apple ID, «вечно» бесплатно
сделать нельзя; решается авто-ресайном (SideStore/AltStore/Sideloadly) или TrollStore.

Реализовано в коде:

1. **Встроенная проверка обновлений** (в README/CI она обещалась, но в коде не было):
   - `ios/TgWsProxy/Updates/UpdateChecker.swift` — `@MainActor` ObservableObject
     (`UpdateChecker.shared`, паттерн как у `LogStore.shared`):
     - GET `https://api.github.com/repos/m0xvi/tg-ws-proxy-axy/releases/latest`,
       ETag-кэш (304 → снапшот из UserDefaults), таймаут 12 с, троттлинг автопроверки
       раз в час (ручная — всегда).
     - Численное сравнение версий (`isNewer`), выбор ассета под сборку:
       `#if TGWS_TUNNEL_AVAILABLE` → `TgWsProxy-vpn.ipa`, иначе `TgWsProxy-free.ipa`
       (фолбэк — любой `.ipa`).
     - Состояния: idle/checking/upToDate/available(Release)/failed; «Позже» = snooze
       на версию (UserDefaults).
   - `ios/TgWsProxy/UI/ContentView.swift` — карточка «Доступна новая версия» на
     главном экране (иконка, версия, подсказка «Скачайте новый .ipa… без кабеля»,
     кнопки «Скачать новую версию» → `openURL(ipaURL ?? htmlURL)`, «Позже»);
     запуск проверки в `.task` и при `scenePhase == .active` (не блокирует
     `resumeIfNeeded`).
   - `ios/TgWsProxy/UI/InfoView.swift` — блок «Обновления»: статус + время проверки,
     «Проверить обновления» (force), «Все релизы», кнопка скачивания.
   - `Localizable.xcstrings` — en-переводы 11 новых строк (формат Xcode: пробелы
     вокруг `:`; новые ключи дописывать в конец).
   - `project.pbxproj` — файл зарегистрирован (PBXBuildFile `100000000000000000000142`,
     PBXFileReference `100000000000000000000242`, путь `Updates/UpdateChecker.swift`).
   - Версия 2.0.10 → 2.0.14 в обоих Info.plist.
2. **Документация** — разделы «Перманентная установка и обновления без кабеля»:
   корневой `README.md` (RU), `docs/README.md` (EN), `docs/ARCHITECTURE.ru.md` (RU).
   Смысл: Sideloadly Automatic App Refresh (Wi-Fi) / AltStore / SideStore / TrollStore /
   платный аккаунт ($99/год — 1 год на подпись). Починены устаревшие ссылки
   `reekeer/tg-ws-proxy-ios` → `m0xvi/tg-ws-proxy-axy`.
3. PR #2 слит в main; CI зелёный (Rust, Android, iOS free+vpn); **релиз v2.0.16
   опубликован автоматически** (в нём уже есть проверка обновлений).

## Дистрибуция: решение и рекомендации (консультационная часть)

Пользователю нужны **бесплатные** варианты, без ПК, на разные версии iOS, много клиентов.

- **Основной бесплатный трек — SideStore** (официальные доки:
  `SideStore/SideStore-Docs` на GitHub, актуально 2026: установка через **iloader**
  (iloader.app) + **LocalDevVPN** из App Store; iOS 15.0+; только Wi-Fi).
  - Важный нюанс: **первичная установка SideStore — один раз с ПК** (iloader, USB);
    дальше (ресайн раз в 7 дней, установки, обновления) — **на устройстве без ПК**,
    при подключённом LocalDevVPN.
  - Паринг-файл может «сыпаться» после обновления/сброса iOS (или спонтанно) →
    re-pair через iloader: Delete Stored Pairing → Trust → Manage Pairing File → Place.
  - Российские операторы могут резать `gsa.apple.com` (anisette) → в iloader есть
    «Use custom Anisette server»; комьюнити-альтернатива ClashMi
    (tom-snow/Sidestore-ClashMi).
- ESign (Safari, без ПК) — только с платным UDID-сертификатом (~$3–5/год/устройство)
  надёжен; бесплатные публичные сертификаты Apple ревакает.
- AltStore Classic 2.3+ (Remote AltServer) — без ПК, но только iOS 17.4+.
- TrollStore — единственный «вечный» вариант, только уязвимые iOS (≤ 16.6.1).
- TestFlight ($99/год, публичная ссылка, до 10 000 тестеров, 90 дней на билд) —
  пользователем отклонён (хочет бесплатное), оставлен как опция.
- **Клиентам раздавать `TgWsProxy-free.ipa`**; vpn-сборка бесплатным путём не
  подписывается.
- Стабильная ссылка на свежий IPA (для SideStore/инструкций):
  `https://github.com/m0xvi/tg-ws-proxy-axy/releases/latest/download/TgWsProxy-free.ipa`

## Живой траблшутинг на момент хендоффа (НЕ закрывать)

Пользователь настраивает SideStore на своём iPhone (Windows, **iloader 2.4.1**,
Apple ID `info@ageevgroup.ru`; на этом же ПК стоит Sideloadly — источник конфликтов).

1. ✅ iloader: SideStore установлен (Download / Sign & Install / Place Pairing File — OK).
2. ❌ SideStore в приложении: **«Unable to fetch News — the request timed out»**.
   Дано: подключить **LocalDevVPN** (App Store, id6755608044) — обязательное условие;
   только Wi-Fi; «News» некритична — сразу пробовать рефреш (My Apps → тап по
   «7 DAYS»); при персистентных таймаутах — кастомный Anisette-сервер в iloader /
   ClashMi (RU-операторы); ре-паринг.
3. ❌ Далее в iloader: **«Failed to select device — Failed to enable wifi debugging:
   unknown error "MissingValue" returned from device»**. Дано (по порядку):
   телефон разблокирован + принять «Trust This Computer»; **убить Sideloadly Daemon**
   из трая и закрыть iTunes; другой USB-порт/кабель; полная перезагрузка iPhone;
   в iloader: Reset Anisette State → Delete Stored Pairing → Refresh Devices;
   обновить iloader/iTunes (версия Apple, не MS Store); другой ПК; View Logs (Ctrl+L);
   поддержка iloader: discord.gg/mACqxMxP3X.
   **Ожидается ответ пользователя, какой шаг сработал.**

## Задачи на следующего агента (по приоритету)

1. **Довести настройку SideStore пользователя** (пункт 3 выше) — ждать отчёт.
2. **AltSource для SideStore-дистрибуции** (согласован с пользователем как «Вариант Б»):
   - добавить `apps.json` (формат AltStore source, см. faq.altstore.io/developers/
     make-a-source) на KiiXY с url на latest-ассет GitHub;
   - авто-обновлять `apps.json` в CI при создании релиза (новый release → новый
     version/build/url в манифесте → коммит в main или публичный raw-URL);
   - тогда клиент добавляет источник одним тапом
     `sidestore://source?url=<url apps.json>` (URL-schema SideStore), и KiiXY
     обновляется в My Apps одним тапом.
   - Осторожно с AltStudio: он генерирует `marketplaceID`/`Build`-поля, которые
     SideStore считает «notarized source» и не даёт добавить (см. app-sources.mdx).
3. **Раздел «Установка без ПК (SideStore)»** в `docs/README.md` / `docs/ARCHITECTURE.ru.md`
   — пошагово для клиента (ссылки: sidestore.io, iloader.app, LocalDevVPN из App Store,
   стабильный latest-URL IPA, troubleshooting: pairing file, gsa.apple.com, лимит 3
   приложения, Wi-Fi only).
4. Опционально (только если пользователь согласится на $99/год): CI-задача TestFlight
   (macos runner + App Store Connect API key, авто-загрузка билда, публичная ссылка).

## Механика сессии / веток

- Рабочая ветка на момент хендоффа: `arena/cdef361d-tg-ws-proxy-axy` (от
  `main@43ba448`, т.е. уже содержит всё из PR #2).
- Паттерн Arena: каждая сессия — своя ветка `arena/*`; merge в main — отдельным PR;
  после merge ветка переходит в `pr_merged` (read-only) — новые правки только в новой
  ветке (кнопка Continue).
- Релиз «в один клик»: Actions → Run workflow → `release_tag=vX.Y.Z`, либо тег `git push`;
  либо просто push в main (релиз с тегом `v2.0.<run_number>`).
- Локально Swift не компилируется (нет Xcode) — проверка только CI на macOS-раннере
  (job «iOS IPA (free + vpn)», ~5–7 мин).
