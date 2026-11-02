#if TGWS_TUNNEL_AVAILABLE
import Foundation

final class TunnelEngine: ProxyEngine {
    let mode: ProxyMode = .tunnel
    let runsInBackground = true

    private let controller = TunnelController.shared
    private let fallback = LoopbackEngine()
    private var usingFallback = false

    func start(_ configuration: ProxyConfiguration) async -> Int32 {
        do {
            try await controller.start(configuration)
            usingFallback = false
            return 0
        } catch {
            usingFallback = true
            return await fallback.start(configuration)
        }
    }

    func stop() async {
        await controller.stop()
        if usingFallback {
            await fallback.stop()
        }
        usingFallback = false
    }

    func stats() async -> ProxyStats {
        if usingFallback {
            return await fallback.stats()
        }
        if let raw = await controller.sendMessage("stats", timeout: 2), !raw.isEmpty {
            return ProxyStats(rawValue: raw)
        }
        // Пустой ответ — ещё не повод переключаться в локальный режим:
        // восстановлением занимается watchdog через health().
        return .empty
    }

    func health(_ configuration: ProxyConfiguration) async -> Bool {
        if usingFallback {
            return await fallback.health(configuration)
        }
        guard let response = await controller.sendMessage("health", timeout: 3) else {
            return false
        }
        return response == "ok"
    }
}
#endif
