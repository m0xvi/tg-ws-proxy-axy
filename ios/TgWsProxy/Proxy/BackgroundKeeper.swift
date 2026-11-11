import CoreLocation
import Foundation

final class BackgroundKeeper: NSObject, ObservableObject {
    static let shared = BackgroundKeeper()

    private let manager = CLLocationManager()
    private var keepAliveTimer: Timer?
    private(set) var isActive = false

    @Published private(set) var authorizationStatus: CLAuthorizationStatus = .notDetermined

    var isAuthorizedForBackground: Bool {
        authorizationStatus == .authorizedAlways
    }

    private override init() {
        super.init()
        manager.delegate = self
        manager.desiredAccuracy = kCLLocationAccuracyBest
        manager.distanceFilter = kCLDistanceFilterNone
        manager.pausesLocationUpdatesAutomatically = false
        manager.activityType = .other
        authorizationStatus = manager.authorizationStatus
    }

    func activate() {
        guard !isActive else {
            refresh()
            return
        }
        isActive = true
        requestAuthorization()
        applyBackgroundMode()
        manager.startUpdatingLocation()
        startKeepAliveTimer()
    }

    func deactivate() {
        guard isActive else { return }
        isActive = false
        keepAliveTimer?.invalidate()
        keepAliveTimer = nil
        manager.stopUpdatingLocation()
        manager.allowsBackgroundLocationUpdates = false
    }

    /// Принудительное обновление фонового режима: iOS может приостановить
    /// location updates даже при `pausesLocationUpdatesAutomatically = false`.
    /// Дёргаем из watchdog и по таймеру, чтобы не «отпустить» процесс.
    func refresh() {
        guard isActive else { return }
        applyBackgroundMode()
        if isAuthorizedForBackground {
            manager.startUpdatingLocation()
        }
    }

    private func requestAuthorization() {
        switch authorizationStatus {
        case .notDetermined:
            manager.requestWhenInUseAuthorization()
        case .authorizedWhenInUse:
            manager.requestAlwaysAuthorization()
        default:
            break
        }
    }

    private func applyBackgroundMode() {
        manager.allowsBackgroundLocationUpdates = isAuthorizedForBackground
    }

    private func startKeepAliveTimer() {
        keepAliveTimer?.invalidate()
        let timer = Timer(timeInterval: 30, repeats: true) { [weak self] _ in
            self?.refresh()
        }
        RunLoop.main.add(timer, forMode: .common)
        keepAliveTimer = timer
    }
}

extension BackgroundKeeper: CLLocationManagerDelegate {
    func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
        authorizationStatus = manager.authorizationStatus
        guard isActive else { return }
        applyBackgroundMode()
        requestAuthorization()
        refresh()
        NotificationCenter.default.post(name: .tgwsForceHealthCheck, object: nil)
    }

    func locationManagerDidPauseLocationUpdates(_ manager: CLLocationManager) {
        fputs("BackgroundKeeper: location updates paused, restarting\n", stderr)
        refresh()
    }

    func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {
        if (error as? CLError)?.code == .locationUnknown { return }
        fputs("BackgroundKeeper: \(error.localizedDescription)\n", stderr)
        NotificationCenter.default.post(name: .tgwsForceHealthCheck, object: nil)
    }
}
