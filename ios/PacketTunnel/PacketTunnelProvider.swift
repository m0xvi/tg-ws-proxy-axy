import NetworkExtension

final class PacketTunnelProvider: NEPacketTunnelProvider {

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

        setTunnelNetworkSettings(settings) { error in
            if error != nil {
                NativeProxy.stop()
            }
            completionHandler(error)
        }
    }

    override func stopTunnel(
        with reason: NEProviderStopReason,
        completionHandler: @escaping () -> Void
    ) {
        NativeProxy.stop()
        completionHandler()
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
