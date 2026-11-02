import BackgroundTasks
import SwiftUI

@main
struct TgWsProxyApp: App {
    @StateObject private var proxy = ProxyViewModel()
    @StateObject private var settings = AppSettings()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .id(settings.restartToken)
                .environmentObject(proxy)
                .environmentObject(settings)
                .environment(\.locale, settings.language.locale)
                .environment(\.liquidGlassEnabled, settings.liquidGlass)
                .tint(settings.accent.color)
                .preferredColorScheme(settings.theme.colorScheme)
                .fullScreenCover(isPresented: .constant(!settings.onboardingDone)) {
                    OnboardingView()
                        .environmentObject(settings)
                        .environment(\.liquidGlassEnabled, settings.liquidGlass)
                }
        }
        // Легальный фоновый «пинок» от iOS: даже если приложение выгружено,
        // система иногда даёт ~30 секунд на принудительную проверку.
        .backgroundTask(.appRefresh(BackgroundHealthTasks.identifier)) { [proxy] in
            await proxy.forceHealthCheck(reason: "bg-refresh")
            BackgroundHealthTasks.scheduleNext()
        }
    }
}
