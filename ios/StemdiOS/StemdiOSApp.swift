import SwiftUI
import UIKit
import UniformTypeIdentifiers

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

    init() {
        keepAwake = UserDefaults.standard.object(forKey: Self.keepAwakeKey) as? Bool ?? true
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

    func separate(_ url: URL) async {
        guard let client else { return }
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        do {
            let hash = StemdClient.sourceHash(of: url)
            _ = try await client.separate(
                file: url,
                title: url.deletingPathExtension().lastPathComponent,
                sourceHash: hash
            )
            await refresh()
        } catch {
            self.error = error.localizedDescription
        }
    }

    func shareURL(for stem: StemdClient.Stem) -> URL? {
        client?.shareURL(for: stem)
    }

    func setTokenRequired(_ required: Bool) {
        StemdServer.shared.setTokenRequired(required)
        tokenRequired = required
    }

    /// The Rust side's request log. Fetched quietly, so reading the log does
    /// not add a row about reading the log.
    func serverRequests() async -> [StemdClient.ServerRequest] {
        guard let client else { return [] }
        return (try? await client.serverRequests()) ?? []
    }
}

struct ContentView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.scenePhase) private var scenePhase
    @State private var importing = false
    @State private var confirmingClear = false
    @State private var copiedAddress = false
    /// Open the debug screen directly when launched with `--debug`, so it can
    /// be reached without tapping through the UI during a test.
    @State private var showDebug = ProcessInfo.processInfo.arguments.contains("--debug")

    var body: some View {
        NavigationStack {
            List {
                consoleSection
                serverSection
                modelSection
                jobsSection
            }
            .listStyle(.insetGrouped)
            .navigationTitle("stemd")
            .toolbar { toolbar }
            .navigationDestination(isPresented: $showDebug) {
                DebugView(log: model.apiLog)
            }
            .fileImporter(
                isPresented: $importing,
                allowedContentTypes: [.audio],
                allowsMultipleSelection: false
            ) { result in
                guard case .success(let urls) = result, let url = urls.first else { return }
                Task { await model.separate(url) }
            }
            .confirmationDialog(
                "Clear the stem cache?",
                isPresented: $confirmingClear,
                titleVisibility: .visible
            ) {
                Button("Delete all stems", role: .destructive) {
                    Task { await model.clearCache() }
                }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Every separated stem and its detail file will be deleted. The model files stay installed.")
            }
            .alert(
                "stemd",
                isPresented: Binding(
                    get: { model.error != nil },
                    set: { if !$0 { model.error = nil } }
                )
            ) {
                Button("OK") { model.error = nil }
            } message: {
                Text(model.error ?? "")
            }
        }
        .tint(Theme.accent)
        .task { await model.start() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active { model.foregrounded() }
        }
    }

    // MARK: - Console

    private var consoleSection: some View {
        Section {
            ConsolePanel(
                status: consoleStatus,
                detail: consoleDetail,
                address: StemdServer.shared.shareBase()?.absoluteString ?? "No network",
                copied: copiedAddress,
                copy: {
                    guard let address = StemdServer.shared.shareBase()?.absoluteString else { return }
                    UIPasteboard.general.string = address
                    UINotificationFeedbackGenerator().notificationOccurred(.success)
                    copiedAddress = true
                    Task {
                        try? await Task.sleep(for: .seconds(1.5))
                        copiedAddress = false
                    }
                }
            )
            .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 8, trailing: 16))
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
        }
    }

    private var consoleStatus: ServerState {
        if model.starting { return .starting }
        if model.serverDown || model.health == nil { return .offline }
        return .online
    }

    private var consoleDetail: String {
        guard let health = model.health, !model.serverDown else {
            return model.starting ? "BOOTING" : "NO SERVER"
        }
        let precision = health.modelId.split(separator: "-").last.map(String.init)?.uppercased() ?? ""
        return "\(precision) · \(health.preset.uppercased())"
    }

    // MARK: - Toolbar

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarTrailing) {
            Button {
                importing = true
            } label: {
                Label("Import a track", systemImage: "plus")
            }
            .disabled(model.health?.models.present != true)
            .accessibilityLabel("Import a track")
        }
        ToolbarItem(placement: .topBarTrailing) {
            Menu {
                Button {
                    showDebug = true
                } label: {
                    Label("Debug log", systemImage: "ladybug")
                }
                Button(role: .destructive) {
                    confirmingClear = true
                } label: {
                    Label("Clear stem cache", systemImage: "trash")
                }
                .disabled(model.jobs.isEmpty)
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .accessibilityLabel("More")
        }
    }

    // MARK: - Server

    private var serverSection: some View {
        Section {
            if model.serverDown || (model.health == nil && !model.starting) {
                VStack(alignment: .leading, spacing: 10) {
                    Label("Server is not running", systemImage: "exclamationmark.triangle.fill")
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(.red)
                    Text("A deck cannot reach it. Open the app again, or restart the server here.")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                    Button {
                        Task { await model.restartServer() }
                    } label: {
                        Label("Restart server", systemImage: "arrow.clockwise")
                    }
                    .buttonStyle(.borderedProminent)
                    .tint(Theme.accent)
                }
                .padding(.vertical, 4)
                .listRowBackground(Color.red.opacity(0.06))
            } else if let health = model.health {
                HStack(spacing: 12) {
                    stat("\(health.jobs)", "Separations")
                    Divider().frame(height: 28)
                    stat(health.preset.capitalized, "Preset")
                    Divider().frame(height: 28)
                    stat(
                        health.tokenRequired ? "Locked" : "Open",
                        "Access",
                        color: health.tokenRequired ? .orange : .green
                    )
                }
                .padding(.vertical, 2)

                LabeledContent {
                    Toggle(
                        "Keep screen awake",
                        isOn: Binding(
                            get: { model.keepAwake },
                            set: { model.setKeepAwake($0) }
                        )
                    )
                    .labelsHidden()
                } label: {
                    Label("Keep screen awake", systemImage: "sun.max")
                }

                if health.tokenRequired {
                    VStack(alignment: .leading, spacing: 8) {
                        Label("Decks cannot connect", systemImage: "lock.fill")
                            .font(.subheadline.weight(.semibold))
                            .foregroundStyle(.orange)
                        Text("The token is required, and the CDJ sidecar has no way to send one.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                        Button {
                            model.setTokenRequired(false)
                        } label: {
                            Label("Allow decks", systemImage: "lock.open.fill")
                        }
                        .buttonStyle(.bordered)
                        .tint(.orange)
                    }
                    .padding(.vertical, 4)
                    .listRowBackground(Color.orange.opacity(0.07))
                }
            } else {
                HStack(spacing: 10) {
                    ProgressView()
                    Text("Starting the server")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
            }
        } header: {
            SectionLabel("Server")
        } footer: {
            Text("The server runs only while this app is in front. Keep the screen awake, or use Guided Access.")
        }
    }

    private func stat(_ value: String, _ label: String, color: Color = .primary) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(value)
                .font(.system(.title3, design: .monospaced).weight(.semibold))
                .foregroundStyle(color)
                .lineLimit(1)
                .minimumScaleFactor(0.8)
            Text(label.uppercased())
                .font(.system(size: 9, weight: .medium, design: .monospaced))
                .tracking(0.8)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - Model

    private var modelSection: some View {
        Section {
            if let models = model.health?.models, let health = model.health {
                if models.downloading {
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            Label("Downloading", systemImage: "arrow.down.circle")
                                .font(.subheadline)
                            Spacer()
                            Text(downloadPercent(models))
                                .font(.system(size: 12, design: .monospaced))
                                .monospacedDigit()
                                .foregroundStyle(.secondary)
                        }
                        Text(models.file ?? "Preparing…")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        ProgressView(
                            value: models.total > 0
                                ? Double(models.done) / Double(models.total)
                                : nil
                        )
                        .tint(Theme.accent)
                    }
                    .padding(.vertical, 2)
                } else if models.present {
                    HStack(spacing: 10) {
                        Label(health.preset.capitalized, systemImage: "shippingbox")
                        Spacer()
                        Text(health.modelId)
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundStyle(.secondary)
                        Image(systemName: "checkmark.circle.fill")
                            .foregroundStyle(.green)
                            .accessibilityLabel("Model files installed")
                    }
                } else {
                    VStack(alignment: .leading, spacing: 10) {
                        Label("Model files missing", systemImage: "exclamationmark.triangle")
                            .font(.subheadline.weight(.semibold))
                        Text("Download the preset's weights to start separating.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                        Button {
                            Task { await model.downloadModels() }
                        } label: {
                            Label("Download model files", systemImage: "arrow.down.circle.fill")
                        }
                        .buttonStyle(.borderedProminent)
                        .tint(Theme.accent)
                        if let error = models.error {
                            Text(error)
                                .font(.caption)
                                .foregroundStyle(.red)
                        }
                    }
                    .padding(.vertical, 2)
                }
            } else {
                Text("Waiting for the server.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
        } header: {
            SectionLabel("Model")
        }
    }

    private func downloadPercent(_ models: StemdClient.Health.Models) -> String {
        guard models.total > 0 else { return "" }
        let percent = Double(models.done) / Double(models.total) * 100
        return "\(Int(percent.rounded()))%"
    }

    // MARK: - Stems

    private var jobsSection: some View {
        Section {
            if model.jobs.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "waveform")
                        .font(.system(size: 34, weight: .medium))
                        .foregroundStyle(.secondary)
                        .frame(height: 46)
                    Text("No separations")
                        .font(.headline)
                    Text("Import a track and it will be separated on this iPad. The result stays here until you remove it.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                    Button {
                        importing = true
                    } label: {
                        Label("Import a track", systemImage: "plus")
                            .font(.subheadline.weight(.semibold))
                            .foregroundStyle(.white)
                            .padding(.horizontal, 16)
                            .padding(.vertical, 10)
                            .background(Theme.accent, in: Capsule())
                    }
                    .buttonStyle(.plain)
                    .disabled(model.health?.models.present != true)
                    .opacity(model.health?.models.present == true ? 1 : 0.45)
                    .padding(.top, 2)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 18)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
            }
            ForEach(model.jobs) { job in
                JobRow(job: job)
            }
        } header: {
            SectionLabel("Stems", count: model.jobs.count)
        }
    }
}

// MARK: - Console panel

private enum ServerState {
    case starting
    case online
    case offline
}

private enum Theme {
    static let accent = Color(red: 1.0, green: 0.45, blue: 0.12)
    static let panel = Color(red: 0.07, green: 0.075, blue: 0.09)
}

/// The one piece of the screen that is deliberately not a grouped-list row: a
/// graphite instrument panel with a live waveform, like a piece of gear rather
/// than a web card.
private struct ConsolePanel: View {
    let status: ServerState
    let detail: String
    let address: String
    let copied: Bool
    let copy: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 3) {
                    Text("STATUS")
                        .font(.system(size: 9, weight: .semibold, design: .monospaced))
                        .tracking(1.6)
                        .foregroundStyle(.white.opacity(0.42))
                    HStack(spacing: 7) {
                        Circle()
                            .fill(statusColor)
                            .frame(width: 7, height: 7)
                        Text(statusText)
                            .font(.system(size: 13, weight: .semibold, design: .monospaced))
                            .tracking(0.8)
                            .foregroundStyle(.white.opacity(0.94))
                    }
                }
                Spacer()
                Text(detail)
                    .font(.system(size: 10, weight: .medium, design: .monospaced))
                    .tracking(0.8)
                    .foregroundStyle(.white.opacity(0.5))
            }

            HStack(spacing: 10) {
                Text(address)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(.white.opacity(0.78))
                    .lineLimit(1)
                    .minimumScaleFactor(0.7)
                Spacer(minLength: 4)
                Button(action: copy) {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(copied ? .green : Theme.accent)
                        .frame(width: 28, height: 28)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Copy the LAN address")
            }

            WaveformLine()
                .frame(height: 18)
        }
        .padding(16)
        .background(
            Theme.panel,
            in: RoundedRectangle(cornerRadius: 16, style: .continuous)
        )
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .strokeBorder(Color.white.opacity(0.08))
        )
    }

    private var statusText: String {
        switch status {
        case .starting: return "STARTING"
        case .online: return "ONLINE"
        case .offline: return "OFFLINE"
        }
    }

    private var statusColor: Color {
        switch status {
        case .starting: return .orange
        case .online: return .green
        case .offline: return .red
        }
    }
}

private struct WaveformLine: View {
    var body: some View {
        Canvas { context, size in
            let mid = size.height / 2
            var path = Path()
            let steps = 96
            for i in 0...steps {
                let t = CGFloat(i) / CGFloat(steps)
                let envelope = sin(.pi * t)
                let x = size.width * t
                let y = mid + sin(t * 23) * envelope * size.height * 0.42
                if i == 0 {
                    path.move(to: CGPoint(x: x, y: y))
                } else {
                    path.addLine(to: CGPoint(x: x, y: y))
                }
            }
            context.stroke(path, with: .color(Theme.accent.opacity(0.55)), lineWidth: 1.3)

            var axis = Path()
            axis.move(to: CGPoint(x: 0, y: mid))
            axis.addLine(to: CGPoint(x: size.width, y: mid))
            context.stroke(axis, with: .color(.white.opacity(0.06)), lineWidth: 1)
        }
    }
}

private struct SectionLabel: View {
    let title: String
    var count: Int? = nil

    init(_ title: String, count: Int? = nil) {
        self.title = title
        self.count = count
    }

    var body: some View {
        HStack(spacing: 8) {
            Text(title.uppercased())
                .font(.system(size: 10, weight: .semibold, design: .monospaced))
                .tracking(1.2)
            Spacer()
            if let count {
                Text("\(count)")
                    .font(.system(size: 10, design: .monospaced))
                    .monospacedDigit()
            }
        }
        .foregroundStyle(.secondary)
    }
}

// MARK: - Job row

private struct JobRow: View {
    @EnvironmentObject private var model: AppModel
    @State private var expanded = false
    let job: StemdClient.Job

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(alignment: .center, spacing: 12) {
                ArtTile(color: tileColor)
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(.body.weight(.semibold))
                        .lineLimit(1)
                    Text(subtitle)
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer(minLength: 8)
                trailing
            }

            if isRunning, let progress = job.progress {
                progressRow(progress)
            } else if isDone {
                metricsRow
            } else if job.state == "failed" {
                Label(job.error ?? "Separation failed", systemImage: "exclamationmark.circle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
            }

            if expanded {
                stemRows
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .padding(.vertical, 3)
        .sensoryFeedback(.selection, trigger: expanded)
    }

    private var tileColor: Color {
        switch job.state {
        case "done": return .green
        case "failed": return .red
        case "running": return Theme.accent
        default: return Color(.systemGray)
        }
    }

    @ViewBuilder
    private var trailing: some View {
        if isDone && !job.stems.isEmpty {
            Button {
                withAnimation(.snappy(duration: 0.22)) {
                    expanded.toggle()
                }
            } label: {
                Image(systemName: "chevron.down")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .rotationEffect(.degrees(expanded ? 180 : 0))
                    .frame(width: 30, height: 30)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel(expanded ? "Hide stems" : "Show stems")
        } else {
            statusLabel
        }
    }

    private var statusLabel: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(statusColor)
                .frame(width: 5, height: 5)
            Text(statusText)
                .font(.system(size: 9, weight: .semibold, design: .monospaced))
                .tracking(0.8)
        }
        .foregroundStyle(statusColor)
    }

    private var statusText: String {
        switch job.state {
        case "done": return "DONE"
        case "failed": return "FAILED"
        case "cancelled": return "STOPPED"
        case "queued": return "QUEUED"
        default: return (job.progress?.stage ?? "working").uppercased()
        }
    }

    private var statusColor: Color {
        switch job.state {
        case "done": return .green
        case "failed": return .red
        case "running": return Theme.accent
        default: return Color(.secondaryLabel)
        }
    }

    private func progressRow(_ progress: StemdClient.Progress) -> some View {
        VStack(spacing: 5) {
            HStack {
                Spacer()
                Text("\(Int((Double(progress.fraction) * 100).rounded()))%")
                    .font(.system(size: 10, design: .monospaced))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .contentTransition(.numericText())
            }
            GeometryReader { proxy in
                ZStack(alignment: .leading) {
                    Capsule()
                        .fill(Color.primary.opacity(0.08))
                    Capsule()
                        .fill(Theme.accent)
                        .frame(width: proxy.size.width * CGFloat(progress.fraction))
                }
            }
            .frame(height: 4)
        }
    }

    private var metricsRow: some View {
        HStack(spacing: 7) {
            Text(durationText)
            Text("·").foregroundStyle(.tertiary)
            Text(separationText)
            Text("·").foregroundStyle(.tertiary)
            Text(realtimeText)
        }
        .font(.system(size: 11, design: .monospaced))
        .monospacedDigit()
        .foregroundStyle(.secondary)
        .lineLimit(1)
        .minimumScaleFactor(0.8)
        .accessibilityLabel("\(durationText) long, separated in \(separationText), \(realtimeText) realtime")
    }

    private var stemRows: some View {
        VStack(spacing: 0) {
            Divider()
            ForEach(Array(job.stems.enumerated()), id: \.element.id) { index, stem in
                HStack(spacing: 9) {
                    Circle()
                        .fill(stemColor(stem.name))
                        .frame(width: 6, height: 6)
                    Text(stem.name.capitalized)
                        .font(.subheadline)
                    Spacer()
                    Text(stemDetail(stem))
                        .font(.system(size: 10, design: .monospaced))
                        .foregroundStyle(.secondary)
                    if let url = model.shareURL(for: stem) {
                        ShareLink(item: url) {
                            Image(systemName: "square.and.arrow.up")
                                .font(.system(size: 12, weight: .semibold))
                                .foregroundStyle(Theme.accent)
                                .frame(width: 28, height: 28)
                        }
                        .buttonStyle(.plain)
                        .accessibilityLabel("Share \(stem.name)")
                    }
                }
                .padding(.vertical, 6)
                if index < job.stems.count - 1 {
                    Divider()
                }
            }
            Button(role: .destructive) {
                Task { await model.remove(job) }
            } label: {
                Label("Remove from cache", systemImage: "trash")
                    .font(.footnote)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.vertical, 5)
            }
            .buttonStyle(.plain)
            .foregroundStyle(.red)
        }
        .padding(.horizontal, 10)
        .padding(.top, 2)
        .background(
            Color.primary.opacity(0.03),
            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
        )
    }

    private func stemColor(_ name: String) -> Color {
        switch name {
        case "vocals": return .purple
        case "drums": return .orange
        default: return .teal
        }
    }

    private func stemDetail(_ stem: StemdClient.Stem) -> String {
        let bytes = ByteCountFormatter.string(fromByteCount: Int64(stem.bytes), countStyle: .file)
        let kind = stem.contentType.replacingOccurrences(of: "audio/", with: "").uppercased()
        return "\(kind) · \(bytes)"
    }

    private var isRunning: Bool { job.state == "running" }
    private var isDone: Bool { job.state == "done" }

    private var title: String {
        job.title.flatMap { $0.isEmpty ? nil : $0 } ?? "Untitled track"
    }

    private var subtitle: String {
        let artist = job.artist?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        if !artist.isEmpty { return artist }
        return isDone ? "Separated on this iPad" : "Processing on this iPad"
    }

    /// The track's own length, from the geometry the server reports with a
    /// finished job.
    private var durationText: String {
        guard let frames = job.frames, frames > 0,
              let rate = job.sampleRate, rate > 0
        else { return "—" }
        let seconds = Int((Double(frames) / Double(rate)).rounded())
        return String(format: "%d:%02d", seconds / 60, seconds % 60)
    }

    /// How long the separation itself took, not how long ago it finished.
    private var separationText: String {
        guard let seconds = job.seconds else { return "—" }
        if seconds < 60 {
            return String(format: "%.1f s", seconds)
        }
        let total = Int(seconds.rounded())
        return String(format: "%d:%02d", total / 60, total % 60)
    }

    private var realtimeText: String {
        guard let realtime = job.realtime, realtime.isFinite, realtime > 0 else { return "—" }
        return String(format: "%.1f×", realtime)
    }
}

private struct ArtTile: View {
    let color: Color

    var body: some View {
        Image(systemName: "waveform")
            .font(.system(size: 17, weight: .semibold))
            .foregroundStyle(.white.opacity(0.92))
            .frame(width: 44, height: 44)
            .background(
                Theme.panel,
                in: RoundedRectangle(cornerRadius: 11, style: .continuous)
            )
            .overlay(alignment: .bottomTrailing) {
                Circle()
                    .fill(color)
                    .frame(width: 8, height: 8)
                    .overlay(Circle().stroke(Color(.secondarySystemGroupedBackground), lineWidth: 2))
                    .offset(x: 1, y: 1)
            }
    }
}
