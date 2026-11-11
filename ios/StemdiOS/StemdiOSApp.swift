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

    /// Shared with the debug screen; every request the app makes is recorded
    /// here, including the one-second polls.
    let apiLog = APILog()

    private var client: StemdClient?
    private var polling: Task<Void, Never>?

    func start() async {
        guard client == nil else { return }
        do {
            try StemdServer.shared.start()
            client = StemdClient(
                base: StemdServer.shared.loopback(),
                token: StemdServer.shared.token,
                log: apiLog
            )
            tokenRequired = StemdServer.shared.tokenRequired
            await refresh()
            polling = Task { [weak self] in
                while !Task.isCancelled {
                    try? await Task.sleep(for: .seconds(1))
                    await self?.refresh()
                }
            }
        } catch {
            self.error = error.localizedDescription
        }
        starting = false
    }

    func stop() {
        polling?.cancel()
        StemdServer.shared.stop()
        client = nil
    }

    func refresh() async {
        guard let client else { return }
        do {
            async let health = client.health()
            async let jobs = client.jobs()
            self.health = try await health
            self.jobs = try await jobs
        } catch {
            // A poll failure is usually the app going to the background. Only
            // make noise when there is nothing on screen to explain it.
            if self.jobs.isEmpty { self.error = error.localizedDescription }
        }
        let working = jobs.contains { $0.state == "queued" || $0.state == "running" }
        UIApplication.shared.isIdleTimerDisabled =
            working || (self.health?.models.downloading ?? false)
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
    @State private var importing = false
    @State private var confirmingClear = false
    /// Open the debug screen directly when launched with `--debug`, so it can
    /// be reached without tapping through the UI during a test.
    @State private var showDebug = ProcessInfo.processInfo.arguments.contains("--debug")

    var body: some View {
        NavigationStack {
            List {
                serverSection
                modelsSection
                jobsSection
            }
            .navigationTitle("stemd")
            .toolbar {
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button {
                        showDebug = true
                    } label: {
                        Image(systemName: "ladybug")
                    }
                    Button {
                        importing = true
                    } label: {
                        Label("Import a track", systemImage: "plus")
                    }
                    .disabled(model.health?.models.present != true)
                    Button(role: .destructive) {
                        confirmingClear = true
                    } label: {
                        Image(systemName: "trash")
                    }
                    .disabled(model.jobs.isEmpty)
                    .accessibilityLabel("Clear the stem cache")
                }
            }
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
        .task { await model.start() }
    }

    private var serverSection: some View {
        Section("Server") {
            if model.starting {
                ProgressView("Starting")
            } else if let health = model.health {
                LabeledContent("Model", value: health.preset.capitalized)
                LabeledContent("Jobs", value: "\(health.jobs)")
                if let base = StemdServer.shared.shareBase() {
                    // The token is in every stem URL; this is the address to
                    // type on the other device, and the app shows it rather
                    // than pretending discovery exists.
                    Text(base.absoluteString)
                        .font(.footnote)
                        .textSelection(.enabled)
                }
            } else {
                Text("The server is not running.")
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var modelsSection: some View {
        Section("Model files") {
            if let models = model.health?.models {
                if models.downloading {
                    ProgressView(
                        value: models.total > 0 ? Double(models.done) / Double(models.total) : nil
                    ) {
                        Text(models.file ?? "Downloading")
                    }
                } else if models.present {
                    Label("Installed", systemImage: "checkmark.circle")
                } else {
                    Button("Download model files") {
                        Task { await model.downloadModels() }
                    }
                    if let error = models.error {
                        Text(error).font(.footnote).foregroundStyle(.red)
                    }
                }
            } else {
                Text("Waiting for the server.").foregroundStyle(.secondary)
            }
        }
    }

    private var jobsSection: some View {
        Section("Stem cache") {
            if model.jobs.isEmpty {
                Text("Drop a track with the + button.")
                    .foregroundStyle(.secondary)
            }
            ForEach(model.jobs) { job in
                JobRow(job: job)
            }
        }
    }
}

private struct JobRow: View {
    @EnvironmentObject private var model: AppModel
    @State private var expanded = false
    let job: StemdClient.Job

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .top, spacing: 8) {
                VStack(alignment: .leading, spacing: 4) {
                    Text(displayTitle).font(.headline)
                    if let progress = job.progress {
                        ProgressView(value: Double(progress.fraction))
                        Text(progress.stage.capitalized)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else if job.state == "done" {
                        Text("\(durationText) · separated in \(separationText)")
                            .font(.caption)
                        if let realtime = job.realtime {
                            Text("\(job.stems.count) stems · \(String(format: "%.1fx", realtime)) realtime")
                                .font(.caption2)
                                .foregroundStyle(.secondary)
                        }
                    } else if job.state == "failed" {
                        Text(job.error ?? "Failed").font(.caption).foregroundStyle(.red)
                    }
                }
                Spacer(minLength: 4)
                if !job.stems.isEmpty {
                    Button {
                        withAnimation(.easeInOut(duration: 0.15)) {
                            expanded.toggle()
                        }
                    } label: {
                        Image(systemName: expanded ? "chevron.up" : "chevron.down")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                            .frame(width: 32, height: 32)
                            .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityLabel(expanded ? "Hide stems" : "Show stems")
                }
            }
            if expanded {
                ForEach(job.stems) { stem in
                    if let url = model.shareURL(for: stem) {
                        ShareLink(item: url) {
                            Label("\(stem.name) · \(stem.contentType)", systemImage: "square.and.arrow.up")
                        }
                        .font(.caption)
                    }
                }
                if job.state != "running" && job.state != "queued" {
                    Button(role: .destructive) {
                        Task { await model.remove(job) }
                    } label: {
                        Label("Remove from cache", systemImage: "trash")
                    }
                    .font(.caption)
                }
            }
        }
        .padding(.vertical, 2)
    }

    /// `Artist - Title`, or the title alone when the source had no artist.
    private var displayTitle: String {
        let title = job.title.flatMap { $0.isEmpty ? nil : $0 } ?? "Untitled track"
        let artist = job.artist?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return artist.isEmpty ? title : "\(artist) - \(title)"
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
}
