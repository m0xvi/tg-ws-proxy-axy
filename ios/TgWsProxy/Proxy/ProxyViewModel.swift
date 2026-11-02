import CoreLocation
import Foundation
import SwiftUI
import UIKit

@MainActor
final class ProxyViewModel: ObservableObject {
    enum State: Equatable {
        case stopped
        case starting
        case running
        case failed(String)
    }

    @Published private(set) var state: State = .stopped
    @Published private(set) var stats = ProxyStats.empty
    @Published private(set) var traffic: [TrafficSample] = []
    @Published private(set) var cloudflareDomains: [String] = []
    @Published var configuration: ProxyConfiguration {
        didSet { persistConfiguration() }
    }

    // Результат принудительных проверок, видимый пользователю в UI.
    @Published private(set) var lastHealthCheck: Date?
    @Published private(set) var healthMessage: String = "Проверка ещё не выполнялась".tgLoc
    @Published private(set) var healthIsOK = false
    @Published private(set) var recoveryCount = 0
    @Published private(set) var backgroundDiagnostics = ""

    private var lastSample: (up: Double, down: Double, at: Date)?
    private static let trafficWindow = 40

    private var statsTask: Task<Void, Never>?
    private var reconnectTask: Task<Void, Never>?
    private var eventTask: Task<Void, Never>?
    private var backgroundBurstTask: Task<Void, Never>?
    private var backgroundTaskID: UIBackgroundTaskIdentifier = .invalid
    private var startAttempt = 0
    private let engine: ProxyEngine = ProxyEngineFactory.make()
    private static let settingsKey = "proxy.configuration.v1"
    private static let maxReconnectAttempts = 3
    private static let maxRecoveryAttempts = 3

    private var healthFailures = 0
    private var recoveryStreak = 0
    private var lastRecoveryAt: Date = .distantPast
    private var lastForcedCheckAt: Date = .distantPast
    private var isCheckingHealth = false
    private var isRecovering = false

    private var reconnectEnabled: Bool {
        UserDefaults.standard.object(forKey: "app.reconnect") as? Bool ?? true
    }

    init() {
        if let data = UserDefaults.standard.data(forKey: Self.settingsKey),
           var value = try? JSONDecoder().decode(ProxyConfiguration.self, from: data) {
            value.normalize()
            configuration = value
        } else {
            configuration = ProxyConfiguration()
        }
        reloadCloudflareDomains()
        ProxyHealthEvents.start()
        startObservingEvents()
    }

    var isRunning: Bool {
        state == .running
    }

    var statusTitle: String {
        switch state {
        case .stopped: "Не подключено".tgLoc
        case .starting: "Запуск…".tgLoc
        case .running: "Прокси работает".tgLoc
        case .failed: "Ошибка запуска".tgLoc
        }
    }

    var statusDetail: String {
        switch state {
        case .stopped:
            "Локальный MTProto".tgLoc + " · \(configuration.bindAddress):\(configuration.port)"
        case .starting:
            "Поднимаем Rust-ядро и WebSocket-пул".tgLoc
        case .running:
            "Telegram → localhost → WSS → Telegram DC".tgLoc
        case .failed(let message):
            message
        }
    }

    var modeTitle: String { engine.mode.title }

    var diagnosticsContext: String {
        let c = configuration
        let s = stats
        return """
        Состояние: \(isRunning ? "запущен" : "остановлен")
        Режим: \(modeTitle)
        Адрес: \(c.bindAddress):\(c.port)
        Размер пула: \(c.poolSize)
        Cloudflare: \(c.cloudflareEnabled ? "вкл" : "выкл") \(c.cloudflareDomain)
        DC override: \(c.dcAddresses.isEmpty ? "—" : c.dcAddresses)
        Статистика: всего \(s.total), активных \(s.active), ws \(s.webSockets), \
        ↑\(s.uploaded) ↓\(s.downloaded), ошибок \(s.errors)
        Здоровье: \(healthIsOK ? "OK" : "нет ответа"), \(healthMessage)
        Восстановлений: \(recoveryCount)
        """
    }

    var backgroundWarning: String? {
        guard !engine.runsInBackground else { return nil }
        guard !BackgroundKeeper.shared.isAuthorizedForBackground else { return nil }
        return "Без доступа к геопозиции в режиме «Всегда» прокси остановится, как только вы свернёте приложение.".tgLoc
    }

    func toggle() {
        isRunning ? stop() : start()
    }

    func start(isRetry: Bool = false) {
        // Раньше `handleStartFailure()` оставлял state = .starting и повторные
        // попытки молча отбрасывались этим guard. Разрешаем ретрай.
        guard (isRetry || state != .starting), !isRunning else { return }
        if !isRetry { startAttempt = 0 }
        ProxyHealthSettings.setShouldBeRunning(true)
        reconnectTask?.cancel()
        state = .starting

        let configuration = configuration
        Task { await self.performStart(configuration) }
    }

    private func performStart(_ configuration: ProxyConfiguration) async {
        let result = await engine.start(configuration)
        if result == 0 {
            startAttempt = 0
            didStart()
        } else {
            handleStartFailure(result)
        }
    }

    func stop() {
        Task { await stopAndWait() }
    }

    func stopAndWait(notify: Bool = true) async {
        guard state != .stopped else {
            ProxyHealthSettings.setShouldBeRunning(false)
            BackgroundHealthTasks.cancel()
            return
        }
        let wasRunning = isRunning
        ProxyHealthSettings.setShouldBeRunning(false)
        BackgroundHealthTasks.cancel()
        endBackgroundBurst()
        reconnectTask?.cancel()
        reconnectTask = nil
        statsTask?.cancel()
        statsTask = nil
        startAttempt = 0
        recoveryStreak = 0
        healthFailures = 0
        state = .stopped
        await engine.stop()
        BackgroundKeeper.shared.deactivate()
        stats = .empty
        traffic = []
        lastSample = nil
        healthIsOK = false
        healthMessage = "Прокси остановлен".tgLoc
        if notify, wasRunning {
            NotificationManager.post(title: "TG WS Proxy", body: "Прокси остановлен")
        }
    }

    func applyConfiguration(
        _ newConfiguration: ProxyConfiguration,
        start requestedStart: Bool?
    ) async {
        let shouldResume = requestedStart ?? isRunning
        if state != .stopped {
            await stopAndWait(notify: false)
        }
        configuration = newConfiguration
        if shouldResume {
            start()
        }
    }

    /// Восстановление после того, как iOS убила приложение. `autoStart` больше
    /// не единственный источник истины: важно намерение пользователя.
    func resumeIfNeeded(autoStartRequested: Bool) async {
        if ProxyHealthSettings.shouldBeRunning {
            await forceHealthCheck(reason: "launch")
        } else if autoStartRequested {
            start()
        }
    }

    /// Принудительная проверка. Вызывается по таймеру, при смене сцены, сети,
    /// статуса VPN, из BGAppRefreshTask и по кнопке «Проверить и восстановить».
    func forceHealthCheck(reason: String) async {
        guard ProxyHealthSettings.isEnabled || reason == "manual" || reason == "launch" else {
            return
        }
        guard !isCheckingHealth else { return }
        let now = Date()
        if reason != "manual", now.timeIntervalSince(lastForcedCheckAt) < 2 { return }
        lastForcedCheckAt = now
        isCheckingHealth = true
        defer { isCheckingHealth = false }

        await updateBackgroundDiagnostics()

        // Пользователь остановил прокси, но процесс/туннель ещё жив — гасим.
        guard ProxyHealthSettings.shouldBeRunning else {
            if await engine.health(configuration) {
                await engine.stop()
            }
            healthIsOK = false
            healthMessage = "Прокси остановлен".tgLoc
            lastHealthCheck = Date()
            return
        }

        guard state != .starting else {
            lastHealthCheck = Date()
            return
        }

        let healthy = await engine.health(configuration)
        lastHealthCheck = Date()

        if healthy {
            healthFailures = 0
            healthIsOK = true
            healthMessage = "Прокси отвечает".tgLoc
            if Date().timeIntervalSince(lastRecoveryAt) > 60 {
                recoveryStreak = 0
            }
            // После перезапуска приложения туннель мог остаться живым:
            // поднимаем UI и статистику без перезапуска ядра.
            if state != .running {
                didStart(announce: false)
            }
            return
        }

        healthFailures += 1
        healthIsOK = false
        healthMessage = "Прокси не отвечает, восстанавливаем…".tgLoc
        fputs("TGWS health check failed (\(reason)), failures=\(healthFailures)\n", stderr)

        // Два провала подряд отсекают кратковременные сетевые «мигания»;
        // ручная проверка и проверка при запуске восстанавливают сразу.
        guard reason == "manual" || reason == "launch" || healthFailures >= 2 else {
            return
        }
        // Не спамим ошибками и уведомлениями, пока не пройдёт окно ретраев.
        if reason != "manual",
           recoveryStreak >= Self.maxRecoveryAttempts,
           Date().timeIntervalSince(lastRecoveryAt) < 600 {
            healthMessage = "Не удалось восстановить прокси".tgLoc
            return
        }
        if reason == "manual" { recoveryStreak = 0 }
        await recover(reason: reason)
    }

    private func recover(reason: String) async {
        guard !isRecovering else { return }
        isRecovering = true
        defer { isRecovering = false }

        // Через 10 минут даём автоматике новую серию попыток.
        if Date().timeIntervalSince(lastRecoveryAt) > 600 {
            recoveryStreak = 0
        }
        guard recoveryStreak < Self.maxRecoveryAttempts else {
            didFail("Не удалось восстановить прокси".tgLoc)
            return
        }

        recoveryStreak += 1
        lastRecoveryAt = Date()
        healthMessage = "Восстанавливаем прокси…".tgLoc
        fputs("TGWS: recovering proxy after \(reason) (attempt \(recoveryStreak))\n", stderr)
        NotificationManager.post(title: "TG WS Proxy", body: "Фоновая проверка: перезапуск прокси")

        statsTask?.cancel()
        statsTask = nil
        startAttempt = 0
        state = .starting
        await engine.stop()
        BackgroundKeeper.shared.deactivate()

        let result = await engine.start(configuration)
        if result == 0 {
            didStart(announce: false)
            recoveryCount += 1
            healthIsOK = true
            healthMessage = "Прокси восстановлен".tgLoc
            fputs("TGWS: proxy recovered after \(reason)\n", stderr)
            NotificationManager.post(title: "TG WS Proxy", body: "Прокси восстановлен после проверки")
        } else {
            healthMessage = "Не удалось восстановить прокси".tgLoc
            handleStartFailure(result)
        }
    }

    private func handleStartFailure(_ result: Int32) {
        let message = NativeProxyError.startFailed(result).localizedDescription
        if reconnectEnabled, startAttempt < Self.maxReconnectAttempts {
            startAttempt += 1
            state = .starting
            reconnectTask?.cancel()
            reconnectTask = Task { [weak self] in
                try? await Task.sleep(for: .seconds(3))
                guard !Task.isCancelled else { return }
                self?.start(isRetry: true)
            }
        } else {
            didFail(message)
        }
    }

    private func startObservingEvents() {
        eventTask = Task { [weak self] in
            let stream = NotificationCenter.default.notifications(named: .tgwsForceHealthCheck)
            for await _ in stream {
                guard let self else { return }
                await self.forceHealthCheck(reason: "event")
            }
        }
    }

    func handleScenePhase(_ phase: ScenePhase) {
        switch phase {
        case .active:
            endBackgroundBurst()
            Task { await self.forceHealthCheck(reason: "foreground") }
        case .background:
            beginBackgroundChecks()
        case .inactive:
            break
        @unknown default:
            break
        }
    }

    private func beginBackgroundChecks() {
        guard ProxyHealthSettings.shouldBeRunning else {
            BackgroundHealthTasks.cancel()
            return
        }
        BackgroundHealthTasks.scheduleNext()
        endBackgroundBurst()

        let id = UIApplication.shared.beginBackgroundTask(withName: "TgWsProxy.HealthCheck") { [weak self] in
            Task { @MainActor in self?.endBackgroundBurst() }
        }
        backgroundTaskID = id
        backgroundBurstTask = Task { [weak self] in
            for _ in 0..<5 {
                try? await Task.sleep(for: .seconds(5))
                guard !Task.isCancelled,
                      let self,
                      ProxyHealthSettings.shouldBeRunning else { break }
                await self.forceHealthCheck(reason: "background")
            }
            self?.endBackgroundBurst()
        }
    }

    private func endBackgroundBurst() {
        backgroundBurstTask?.cancel()
        backgroundBurstTask = nil
        if backgroundTaskID != .invalid {
            UIApplication.shared.endBackgroundTask(backgroundTaskID)
            backgroundTaskID = .invalid
        }
    }

    private func updateBackgroundDiagnostics() async {
        var lines: [String] = []
        switch UIApplication.shared.backgroundRefreshStatus {
        case .available: lines.append("Фоновое обновление: доступно")
        case .denied: lines.append("Фоновое обновление: запрещено")
        case .restricted: lines.append("Фоновое обновление: ограничено")
        @unknown default: lines.append("Фоновое обновление: неизвестно")
        }

        if engine.runsInBackground {
#if TGWS_TUNNEL_AVAILABLE
            let connected = await TunnelController.shared.isConnected()
            lines.append(connected ? "VPN: подключён" : "VPN: отключён")
#else
            lines.append("VPN: недоступен в этой сборке")
#endif
        } else {
            switch BackgroundKeeper.shared.authorizationStatus {
            case .authorizedAlways: lines.append("Геопозиция: всегда")
            case .authorizedWhenInUse: lines.append("Геопозиция: при использовании")
            case .denied, .restricted: lines.append("Геопозиция: запрещена")
            case .notDetermined: lines.append("Геопозиция: не запрошена")
            @unknown default: lines.append("Геопозиция: неизвестно")
            }
            lines.append(
                BackgroundKeeper.shared.isActive
                    ? "Удержание фона: включено"
                    : "Удержание фона: выключено"
            )
        }
        backgroundDiagnostics = lines.joined(separator: "\n")
    }

    private func didStart(announce: Bool = true) {
        state = .running
        if !engine.runsInBackground {
            BackgroundKeeper.shared.activate()
        }
        if ProxyHealthSettings.isEnabled {
            BackgroundHealthTasks.scheduleNext()
        } else {
            BackgroundHealthTasks.cancel()
        }
        lastSample = nil
        traffic = []
        if announce {
            Haptics.notify(.success)
            NotificationManager.post(title: "TG WS Proxy", body: "Прокси запущен")
        }

        statsTask?.cancel()
        let checkEvery = max(5, Int(ProxyHealthSettings.checkInterval))
        statsTask = Task { [weak self] in
            var tick = 0
            while !Task.isCancelled {
                guard let self else { return }
                let s = await self.engine.stats()
                self.stats = s
                self.recordTraffic(s)

                tick += 1
                if tick % checkEvery == 0,
                   ProxyHealthSettings.isEnabled,
                   self.isRunning {
                    await self.forceHealthCheck(reason: "timer")
                }
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }

    private func recordTraffic(_ s: ProxyStats) {
        let now = Date()
        if let last = lastSample {
            let dt = max(now.timeIntervalSince(last.at), 0.001)
            let up = max((s.uploadedBytes - last.up) / dt, 0)
            let down = max((s.downloadedBytes - last.down) / dt, 0)
            traffic.append(TrafficSample(time: now, up: up, down: down))
            if traffic.count > Self.trafficWindow {
                traffic.removeFirst(traffic.count - Self.trafficWindow)
            }
        }
        lastSample = (s.uploadedBytes, s.downloadedBytes, now)
    }

    private func didFail(_ message: String) {
        state = .failed(message)
        healthIsOK = false
        healthMessage = message
        BackgroundKeeper.shared.deactivate()
        Haptics.notify(.error)
        NotificationManager.post(title: "TG WS Proxy", body: "Ошибка: \(message)")
    }

    private func persistConfiguration() {
        guard let data = try? JSONEncoder().encode(configuration) else { return }
        UserDefaults.standard.set(data, forKey: Self.settingsKey)
    }

    func telegramURL() -> URL? {
        let secret = "dd" + configuration.secret
        var components = URLComponents()
        components.scheme = "tg"
        components.host = "proxy"
        components.queryItems = [
            URLQueryItem(name: "server", value: configuration.bindAddress),
            URLQueryItem(name: "port", value: String(configuration.port)),
            URLQueryItem(name: "secret", value: secret)
        ]
        return components.url
    }

    func regenerateSecret() {
        guard !isRunning else { return }
        configuration.secret = ProxyConfiguration.makeSecret()
    }

    func reloadCloudflareDomains() {
        cloudflareDomains = NativeProxy.cloudflareDomains()
    }

    func refreshCloudflareDomains() async -> Bool {
        let refreshed = await NativeProxy.refreshCloudflareDomains()
        reloadCloudflareDomains()
        return refreshed
    }
}

struct TrafficSample: Identifiable {
    let id = UUID()
    let time: Date
    let up: Double
    let down: Double
}

struct ProxyStats {
    var uploaded: String
    var downloaded: String
    var active: String
    var webSockets: String
    var total: String
    var tcpFallback: String
    var cloudflare: String
    var errors: String
    var pool: String
    var uploadedBytes: Double
    var downloadedBytes: Double

    static let empty = ProxyStats(raw: "")

    init(rawValue: String) { self = ProxyStats(raw: rawValue) }

    private init(raw: String) {
        uploaded = Self.value(after: "up=", in: raw, fallback: "0 B")
        downloaded = Self.value(after: "down=", in: raw, fallback: "0 B")
        active = Self.value(after: "active=", in: raw, fallback: "0")
        webSockets = Self.value(after: "ws=", in: raw, fallback: "0")
        total = Self.value(after: "total=", in: raw, fallback: "0")
        tcpFallback = Self.value(after: "tcp_fb=", in: raw, fallback: "0")
        cloudflare = Self.value(after: "cf=", in: raw, fallback: "0")
        errors = Self.value(after: "err=", in: raw, fallback: "0")
        pool = Self.value(after: "pool=", in: raw, fallback: "0/0")
        uploadedBytes = Self.bytes(from: Self.value(after: "up=", in: raw, fallback: "0B"))
        downloadedBytes = Self.bytes(from: Self.value(after: "down=", in: raw, fallback: "0B"))
    }

    private static func bytes(from s: String) -> Double {
        let multipliers: [(String, Double)] = [
            ("TB", 1099511627776), ("GB", 1073741824),
            ("MB", 1048576), ("KB", 1024), ("B", 1)
        ]
        for (suffix, mult) in multipliers where s.hasSuffix(suffix) {
            return (Double(s.dropLast(suffix.count)) ?? 0) * mult
        }
        return Double(s) ?? 0
    }

    private static func value(after key: String, in source: String, fallback: String) -> String {
        guard let range = source.range(of: key) else { return fallback }
        let suffix = source[range.upperBound...]
        return suffix.prefix { !$0.isWhitespace }.description
    }
}
