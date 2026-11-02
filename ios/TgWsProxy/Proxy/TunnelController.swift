#if TGWS_TUNNEL_AVAILABLE
import Foundation
import NetworkExtension
import os

enum TunnelControllerError: Error {
    case unavailable
    case connectionFailed
    case timedOut
}

final class TunnelController {
    static let shared = TunnelController()

    private let lock = NSLock()
    private var manager: NETunnelProviderManager?

    private var providerBundleID: String {
        (Bundle.main.bundleIdentifier ?? "com.m0xvi.kiixy") + ".tunnel"
    }

    private func storedManager() -> NETunnelProviderManager? {
        lock.lock()
        defer { lock.unlock() }
        return manager
    }

    private func store(_ value: NETunnelProviderManager?) {
        lock.lock()
        defer { lock.unlock() }
        manager = value
    }

    /// После перезапуска приложения `manager` ещё nil, но VPN-конфигурация
    /// уже сохранена системой. Всегда подгружаем её из preferences.
    private func activeSession() async -> NETunnelProviderSession? {
        if let session = storedManager()?.connection as? NETunnelProviderSession {
            return session
        }
        guard let managers = try? await NETunnelProviderManager.loadAllFromPreferences(),
              let mgr = managers.first, mgr.isEnabled else { return nil }
        store(mgr)
        return mgr.connection as? NETunnelProviderSession
    }

    @discardableResult
    func prepare() async throws -> NETunnelProviderManager {
        let managers = try await NETunnelProviderManager.loadAllFromPreferences()
        let mgr = managers.first ?? NETunnelProviderManager()
        store(mgr)
        return mgr
    }

    func start(_ configuration: ProxyConfiguration) async throws {
        let mgr = try await prepare()
        let proto = NETunnelProviderProtocol()
        proto.providerBundleIdentifier = providerBundleID
        proto.serverAddress = "127.0.0.1"
        proto.providerConfiguration = [
            "configuration": (try? JSONEncoder().encode(configuration)) ?? Data()
        ]
        mgr.protocolConfiguration = proto
        mgr.localizedDescription = "TG WS Proxy"
        mgr.isEnabled = true
        try await mgr.saveToPreferences()
        try await mgr.loadFromPreferences()
        guard let session = mgr.connection as? NETunnelProviderSession else {
            throw TunnelControllerError.unavailable
        }

        // Preferences уже обновлены. Если VPN ещё подключён, не дёргаем
        // startTunnel() повторно — это частая причина падения в loopback-fallback.
        if session.status == .connected {
            return
        }
        // recover() только что вызвал stopTunnel(): даём сессии завершить
        // переход, иначе startTunnel() получит .disconnecting и упадёт.
        for _ in 0..<20 {
            guard session.status == .disconnecting else { break }
            try? await Task.sleep(for: .milliseconds(100))
        }

        try session.startTunnel()

        for _ in 0..<40 {
            switch session.status {
            case .connected:
                return
            case .invalid, .disconnected:
                if session.status == .disconnected {
                    try? await Task.sleep(for: .milliseconds(200))
                    continue
                }
                throw TunnelControllerError.connectionFailed
            case .disconnecting:
                throw TunnelControllerError.connectionFailed
            case .connecting, .reasserting:
                try await Task.sleep(for: .milliseconds(200))
            @unknown default:
                throw TunnelControllerError.connectionFailed
            }
        }

        session.stopTunnel()
        throw TunnelControllerError.timedOut
    }

    func stop() async {
        guard let session = await activeSession() else { return }
        session.stopTunnel()
    }

    /// Сообщение в процесс расширения с жёстким таймаутом: если провайдер убит
    /// или завис, watchdog не должен ждать ответа вечно.
    func sendMessage(_ command: String, timeout: TimeInterval = 3) async -> String? {
        guard let session = await activeSession() else { return nil }
        guard let data = command.data(using: .utf8) else { return nil }

        return await withCheckedContinuation { continuation in
            let finished = OSAllocatedUnfairLock(initialState: false)
            func finish(_ value: String?) {
                let first = finished.withLock { done -> Bool in
                    guard !done else { return false }
                    done = true
                    return true
                }
                guard first else { return }
                continuation.resume(returning: value)
            }

            do {
                try session.sendProviderMessage(data) { response in
                    finish(response.flatMap { String(data: $0, encoding: .utf8) })
                }
            } catch {
                finish(nil)
            }
            DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { finish(nil) }
        }
    }

    /// Свежий статус VPN из системных настроек, а не из кеша UI:
    /// пользователь мог выключить VPN в «Настройках» в обход приложения.
    func isConnected() async -> Bool {
        do {
            let managers = try await NETunnelProviderManager.loadAllFromPreferences()
            guard let mgr = managers.first, mgr.isEnabled else { return false }
            store(mgr)
            return (mgr.connection as? NETunnelProviderSession)?.status == .connected
        } catch {
            return false
        }
    }
}
#endif
