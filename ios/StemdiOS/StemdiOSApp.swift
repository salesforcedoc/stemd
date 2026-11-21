import SwiftUI
import UIKit

@main
struct StemdiOSApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
        }
    }
}

/// The UI's view of the embedded server. Everything below it is Rust; this
/// object only starts the server and polls the same HTTP API any other client
/// would use.
@MainActor
final class AppModel: ObservableObject {
    @Published var health: StemdClient.Health?
    @Published var jobs: [StemdClient.Job] = []
    @Published var error: String?
    @Published var starting = true
    /// Mirrors the Rust-side switch so the debug screen can bind to it.
    @Published var tokenRequired = true
    /// Stop the display sleeping while the app is in front. An auto-lock would
    /// suspend the process and take the server with it, which is the one part of
    /// "background" this app can control.
    @Published var keepAwake: Bool
    /// Keep the pre-separation mix of each new job as a WAV reference.
    @Published var savePCM: Bool

    init() {
        keepAwake = UserDefaults.standard.object(forKey: Self.keepAwakeKey) as? Bool ?? true
        savePCM = UserDefaults.standard.bool(forKey: StemdServer.savePCMKey)
    }

    private static let keepAwakeKey = "stemd.keepAwake"

    /// True when the Rust listener is not answering while the app is in front.
    /// The cache list is still valid; this only says the server needs bringing
    /// back.
    @Published var serverDown = false
    private var pollFailures = 0
    private var restarting = false

    /// Shared with the debug screen; every request the app makes is recorded
    /// here, including the one-second polls.
    let apiLog = APILog()

    private var client: StemdClient?
    private var polling: Task<Void, Never>?

    func start() async {
        guard client == nil else { return }
        do {
            try StemdServer.shared.start()
            makeClient()
            startPolling()
            await refresh()
        } catch {
            self.error = error.localizedDescription
            serverDown = true
        }
        starting = false
    }

    func stop() {
        polling?.cancel()
        polling = nil
        StemdServer.shared.stop()
        client = nil
        serverDown = false
    }

    private func makeClient() {
        client = StemdClient(
            base: StemdServer.shared.loopback(),
            token: StemdServer.shared.token,
            log: apiLog
        )
        tokenRequired = StemdServer.shared.tokenRequired
    }

    private func startPolling() {
        polling?.cancel()
        polling = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                await self?.refresh()
            }
        }
    }

    func refresh() async {
        guard let client else { return }
        do {
            async let health = client.health()
            async let jobs = client.jobs()
            self.health = try await health
            self.jobs = try await jobs
            serverDown = false
            pollFailures = 0
        } catch {
            pollFailures += 1
            // A failure in the background is the suspension itself, not a broken
            // server. Only a foreground app can tell the difference, and only it
            // can do anything about it.
            if UIApplication.shared.applicationState == .active, pollFailures >= 3 {
                serverDown = true
                self.health = nil
            }
            // Make noise only when there is nothing on screen to explain it.
            if self.jobs.isEmpty { self.error = error.localizedDescription }
        }
        let working = jobs.contains { $0.state == "queued" || $0.state == "running" }
        updateIdleTimer(working: working || (self.health?.models.downloading ?? false))
    }

    /// The app cannot run in the background; the least it can do is not let the
    /// display lock while it is the foreground app.
    private func updateIdleTimer(working: Bool) {
        UIApplication.shared.isIdleTimerDisabled = keepAwake || working
    }

    func setKeepAwake(_ awake: Bool) {
        keepAwake = awake
        UserDefaults.standard.set(awake, forKey: Self.keepAwakeKey)
        refreshSoon()
    }

    /// Back in the foreground. If the process was only suspended, the listener
    /// survived; prove it with a loopback probe, then re-register Bonjour. If
    /// it is gone, rebind it.
    func foregrounded() {
        Task { await ensureServer() }
    }

    private func ensureServer() async {
        // The first appearance runs `start()`; this hook must not race it.
        guard !starting, !restarting, let client else { return }
        if (try? await client.health()) != nil {
            serverDown = false
            pollFailures = 0
            StemdServer.shared.republish()
            await refresh()
            return
        }
        await restartServer()
    }

    /// Rebind the listener and republish it.
    ///
    /// Called when the app returns to the front and the probe failed, and from
    /// the Server section. The fixed port can stay reserved for a moment after
    /// a stop, so a failed bind is retried once.
    func restartServer() async {
        guard !restarting else { return }
        restarting = true
        defer { restarting = false }
        serverDown = true
        polling?.cancel()
        polling = nil
        StemdServer.shared.stop()
        client = nil
        do {
            try StemdServer.shared.start()
        } catch {
            try? await Task.sleep(for: .milliseconds(800))
            do {
                try StemdServer.shared.start()
            } catch {
                self.error = "The server could not restart: \(error.localizedDescription)"
                return
            }
        }
        makeClient()
        StemdServer.shared.republish()
        serverDown = false
        pollFailures = 0
        startPolling()
        await refresh()
    }

    private func refreshSoon() {
        Task { await refresh() }
    }

    func downloadModels() async {
        guard let client else { return }
        do {
            try await client.downloadModels()
            await refresh()
        } catch {
            self.error = error.localizedDescription
        }
    }

    func clearCache() async {
        guard let client else { return }
        do {
            try await client.clearCache()
            await refresh()
        } catch {
            self.error = error.localizedDescription
        }
    }

    func remove(_ job: StemdClient.Job) async {
        guard let client else { return }
        do {
            try await client.removeJob(job.id)
            await refresh()
        } catch {
            self.error = error.localizedDescription
        }
    }

    func setTokenRequired(_ required: Bool) {
        StemdServer.shared.setTokenRequired(required)
        tokenRequired = required
    }

    func setSavePCM(_ save: Bool) {
        savePCM = save
        UserDefaults.standard.set(save, forKey: StemdServer.savePCMKey)
        StemdServer.shared.setSavePCM(save)
    }

    /// The Rust side's request log. Fetched quietly, so reading the log does
    /// not add a row about reading the log.
    func serverRequests() async -> [StemdClient.ServerRequest] {
        guard let client else { return [] }
        return (try? await client.serverRequests()) ?? []
    }
}

/// Picks the screen. Both share the same `AppModel`; only the presentation
/// differs. The booth display is the default because this app lives next to a
/// deck and is read from where the DJ stands.
struct ContentView: View {
    @AppStorage("stemd.appearance") private var appearance = AppAppearance.cdj

    var body: some View {
        switch appearance {
        case .console: LibraryView()
        case .cdj: BoothView()
        }
    }
}
