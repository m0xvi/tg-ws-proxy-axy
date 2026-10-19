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
        controller.stop()
        if usingFallback {
            await fallback.stop()
        }
    }

    func stats() async -> ProxyStats {
        if usingFallback {
            return await fallback.stats()
        }
        if let raw = await controller.sendMessage("stats"), !raw.isEmpty {
            return ProxyStats(rawValue: raw)
        }
        usingFallback = true
        return await fallback.stats()
    }
}
#endif
