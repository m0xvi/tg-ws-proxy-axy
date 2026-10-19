import Network
import NetworkExtension

final class PacketTunnelProvider: NEPacketTunnelProvider {

    private var pathMonitor: NWPathMonitor?
    private let monitorQueue = DispatchQueue(label: "TgWsProxy.TunnelPathMonitor")
    private var lastInterfaceSignature: String?

    override func startTunnel(
        options: [String: NSObject]?,
        completionHandler: @escaping (Error?) -> Void
    ) {
        let configuration = loadConfiguration()
        let code = NativeProxy.start(configuration: configuration)
        guard code == 0 else {
            completionHandler(
                NSError(
                    domain: "TgWsProxy",
                    code: Int(code),
                    userInfo: [NSLocalizedDescriptionKey: "StartProxy failed (\(code))"]
                )
            )
            return
        }

        let settings = NEPacketTunnelNetworkSettings(tunnelRemoteAddress: "127.0.0.1")
        let ipv4 = NEIPv4Settings(addresses: ["10.7.0.2"], subnetMasks: ["255.255.255.255"])
        ipv4.includedRoutes = []
        ipv4.excludedRoutes = [NEIPv4Route.default()]
        settings.ipv4Settings = ipv4
        settings.mtu = 1500

        setTunnelNetworkSettings(settings) { [weak self] error in
            if error != nil {
                NativeProxy.stop()
            } else {
                self?.startNetworkMonitor()
            }
            completionHandler(error)
        }
    }

    override func stopTunnel(
        with reason: NEProviderStopReason,
        completionHandler: @escaping () -> Void
    ) {
        stopNetworkMonitor()
        NativeProxy.stop()
        completionHandler()
    }

    private func startNetworkMonitor() {
        stopNetworkMonitor()
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            guard let self, path.status == .satisfied else { return }
            let sig = path.availableInterfaces
                .filter { $0.type != .other }
                .map { "\($0.type):\($0.name)" }
                .joined(separator: ",")
            if let prev = self.lastInterfaceSignature, !prev.isEmpty, prev != sig {
                NativeProxy.notifyNetworkChanged()
            }
            self.lastInterfaceSignature = sig
        }
        pathMonitor = monitor
        monitor.start(queue: monitorQueue)
    }

    private func stopNetworkMonitor() {
        pathMonitor?.cancel()
        pathMonitor = nil
        lastInterfaceSignature = nil
    }

    override func handleAppMessage(
        _ messageData: Data,
        completionHandler: ((Data?) -> Void)?
    ) {
        guard let command = String(data: messageData, encoding: .utf8) else {
            completionHandler?(nil)
            return
        }
        let response: String
        switch command {
        case "stats":  response = NativeProxy.stats()
        case "secret": response = NativeProxy.secretWithPrefix() ?? ""
        default:       response = ""
        }
        completionHandler?(response.data(using: .utf8))
    }

    private func loadConfiguration() -> ProxyConfiguration {
        guard
            let proto = protocolConfiguration as? NETunnelProviderProtocol,
            let raw = proto.providerConfiguration?["configuration"] as? Data,
            let decoded = try? JSONDecoder().decode(ProxyConfiguration.self, from: raw)
        else {
            return ProxyConfiguration()
        }
        return decoded
    }
}
