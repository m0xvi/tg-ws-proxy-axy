import BackgroundTasks
import Foundation
import Network
import NetworkExtension
import os
import UIKit

extension Notification.Name {
    /// Любое системное событие, после которого надо принудительно проверить прокси:
    /// смена сети, смена статуса VPN, foreground, фоновый BGAppRefreshTask.
    static let tgwsForceHealthCheck = Notification.Name("tgws.forceHealthCheck")
}

enum ProxyHealthSettings {
    static let enabledKey = "app.forceHealthChecks"
    static let intervalKey = "app.healthCheckInterval"
    static let desiredRunningKey = "proxy.shouldBeRunning.v1"

    static var isEnabled: Bool {
        UserDefaults.standard.object(forKey: enabledKey) as? Bool ?? true
    }

    static var checkInterval: TimeInterval {
        let seconds = UserDefaults.standard.object(forKey: intervalKey) as? Int ?? 10
        return TimeInterval(min(max(seconds, 5), 60))
    }

    /// Намерение пользователя. Хранится отдельно от UI-состояния: после того как
    /// iOS убила процесс, приложение при следующем запуске обязано восстановить прокси.
    static var shouldBeRunning: Bool {
        UserDefaults.standard.bool(forKey: desiredRunningKey)
    }

    static func setShouldBeRunning(_ value: Bool) {
        UserDefaults.standard.set(value, forKey: desiredRunningKey)
    }
}

enum BackgroundHealthTasks {
    static let identifier = "com.m0xvi.kiixy.healthcheck"
    static let minimumInterval: TimeInterval = 15 * 60

    /// iOS не гарантирует время запуска, но это лучший легальный способ получить
    /// немного фонового времени, когда приложение уже выгружено.
    static func scheduleNext() {
        let request = BGAppRefreshTaskRequest(identifier: identifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: minimumInterval)
        do {
            try BGTaskScheduler.shared.submit(request)
        } catch {
            fputs("BackgroundHealthTasks: \(error.localizedDescription)\n", stderr)
        }
    }

    static func cancel() {
        BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: identifier)
    }
}

enum PortProbe {
    static func loopbackHost(for bindAddress: String) -> String {
        let trimmed = bindAddress.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty || trimmed == "0.0.0.0" || trimmed == "::" {
            return "127.0.0.1"
        }
        return trimmed
    }

    /// Реальный TCP-коннект к слушателю, а не только чтение статистики Rust.
    static func isListening(host: String, port: Int, timeout: TimeInterval = 2) async -> Bool {
        guard port > 0, port <= Int(UInt16.max),
              let nwPort = NWEndpoint.Port(rawValue: UInt16(port)) else { return false }
        let endpointHost = NWEndpoint.Host(loopbackHost(for: host))

        return await withCheckedContinuation { continuation in
            let connection = NWConnection(host: endpointHost, port: nwPort, using: .tcp)
            let finished = OSAllocatedUnfairLock(initialState: false)

            func finish(_ alive: Bool) {
                let first = finished.withLock { done -> Bool in
                    guard !done else { return false }
                    done = true
                    return true
                }
                guard first else { return }
                connection.cancel()
                continuation.resume(returning: alive)
            }

            connection.stateUpdateHandler = { state in
                switch state {
                case .ready: finish(true)
                case .failed, .cancelled: finish(false)
                default: break
                }
            }
            connection.start(queue: .global())
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { finish(false) }
        }
    }
}

enum ProxyHealthEvents {
    private static let monitor = NetworkEventMonitor()
    private static var vpnObserver: NSObjectProtocol?
    private static var started = false

    static func start() {
        guard !started else { return }
        started = true
        monitor.start()

        // Если iOS сама сбросила VPN-сессию, приложение должно попробовать её поднять.
        vpnObserver = NotificationCenter.default.addObserver(
            forName: .NEVPNStatusDidChange,
            object: nil,
            queue: .main
        ) { _ in
            NotificationCenter.default.post(name: .tgwsForceHealthCheck, object: nil)
        }
    }
}

private final class NetworkEventMonitor {
    private let queue = DispatchQueue(label: "TgWsProxy.HealthNetworkMonitor")
    private var monitor: NWPathMonitor?
    private var lastPost = Date.distantPast
    private var lastSignature: String?

    func start() {
        guard monitor == nil else { return }
        let pathMonitor = NWPathMonitor()
        pathMonitor.pathUpdateHandler = { [weak self] path in
            guard let self, path.status == .satisfied else { return }
            let signature = path.availableInterfaces
                .map { "\($0.type):\($0.name)" }
                .joined(separator: ",")
            let now = Date()
            guard now.timeIntervalSince(self.lastPost) >= 3 else { return }
            self.lastPost = now
            if let previous = self.lastSignature, previous != signature {
                NotificationCenter.default.post(name: .tgwsForceHealthCheck, object: nil)
            }
            self.lastSignature = signature
        }
        monitor = pathMonitor
        pathMonitor.start(queue: queue)
    }
}
