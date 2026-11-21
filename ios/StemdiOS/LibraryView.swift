import SwiftUI

/// The standard screen. A plain grouped list: whether a deck can reach the
/// server, the tracks that have been separated, and the few settings there
/// are. Nothing on it is decoration.
struct LibraryView: View {
    @EnvironmentObject private var model: AppModel
    @AppStorage("stemd.appearance") private var appearance = AppAppearance.cdj
    @State private var confirmingClear = false
    @State private var expanded: Set<String> = []
    @State private var preview: StemPreview?
    /// Open the debug screen directly when launched with `--debug`, so it can
    /// be reached without tapping through the UI during a test.
    @State private var showDebug = ProcessInfo.processInfo.arguments.contains("--debug")

    var body: some View {
        NavigationStack {
            List {
                serverSection
                tracksSection
                if model.status == .running {
                    settingsSection
                }
            }
            .listStyle(.insetGrouped)
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if let preview {
                    StemPlayerBar(preview: preview) { closePreview() }
                }
            }
            .navigationTitle("stemd")
            .toolbar { toolbar }
            .navigationDestination(isPresented: $showDebug) {
                DebugView(log: model.apiLog)
            }
            .modifier(HomeActions(confirmingClear: $confirmingClear))
        }
        .onDisappear { closePreview() }
    }

    /// Start a fresh mix for this job, replacing any preview already playing.
    private func openPreview(_ job: StemdClient.Job, solo: String?) {
        preview?.tearDown()
        let next = StemPreview(job: job, solo: solo)
        preview = next
        Task { await next.load() }
    }

    private func closePreview() {
        preview?.tearDown()
        preview = nil
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItem(placement: .topBarTrailing) {
            Menu {
                Button {
                    appearance = .cdj
                } label: {
                    Label("Booth Display", systemImage: "textformat.size.larger")
                }
                Button {
                    showDebug = true
                } label: {
                    Label("Settings", systemImage: "gearshape")
                }
                Divider()
                Button(role: .destructive) {
                    confirmingClear = true
                } label: {
                    Label("Clear Stem Cache…", systemImage: "trash")
                }
                .disabled(model.jobs.isEmpty)
            } label: {
                Label("More", systemImage: "ellipsis.circle")
            }
        }
    }

    // MARK: - Server

    private var serverSection: some View {
        Section {
            statusRow

            if model.status == .stopped {
                Button("Restart Server") {
                    Task { await model.restartServer() }
                }
            } else if let health = model.health {
                if health.tokenRequired {
                    notice(
                        "Decks can't connect",
                        "A token is required, and the CDJ sidecar has no way to send one."
                    )
                    Button("Allow Decks to Connect") {
                        model.setTokenRequired(false)
                    }
                }
                modelRows(health.models)
            }
        }
    }

    private var statusRow: some View {
        HStack(spacing: 12) {
            statusIcon
                .font(.title3)
                .frame(width: 30)
            VStack(alignment: .leading, spacing: 2) {
                Text(statusTitle)
                    .font(.headline)
                if let address = model.address {
                    Text(address)
                        .font(.subheadline.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .minimumScaleFactor(0.7)
                } else if let statusDetail {
                    Text(statusDetail)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer(minLength: 8)
            if model.address != nil {
                CopyAddressButton { copied in
                    Text(copied ? "Copied" : "Copy")
                        .contentTransition(.opacity)
                }
                .buttonStyle(.bordered)
                .buttonBorderShape(.capsule)
                .controlSize(.small)
            }
        }
        .padding(.vertical, 4)
        .accessibilityElement(children: .combine)
    }

    @ViewBuilder
    private var statusIcon: some View {
        switch model.status {
        case .starting:
            ProgressView()
        case .stopped:
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.red)
        case .running:
            if model.address == nil {
                Image(systemName: "wifi.slash")
                    .foregroundStyle(.orange)
            } else {
                Image(systemName: "dot.radiowaves.left.and.right")
                    .foregroundStyle(.green)
            }
        }
    }

    private var statusTitle: String {
        switch model.status {
        case .starting: return "Starting the server…"
        case .stopped: return "Server stopped"
        case .running: return model.address == nil ? "No network" : "Serving on this network"
        }
    }

    private var statusDetail: String? {
        switch model.status {
        case .starting: return nil
        case .stopped: return "A deck can't reach it until it restarts."
        case .running: return "Join the deck's network so it can find this device."
        }
    }

    @ViewBuilder
    private func modelRows(_ models: StemdClient.Health.Models) -> some View {
        if models.downloading {
            VStack(alignment: .leading, spacing: 8) {
                HStack(alignment: .firstTextBaseline) {
                    Text("Downloading the model")
                    Spacer()
                    Text(models.percentText)
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                }
                ProgressView(value: models.fraction)
                Text(models.file ?? "Preparing…")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            .padding(.vertical, 4)
        } else if !models.present {
            notice(
                "The model isn't installed",
                models.error ?? "Download it once to start separating tracks.",
                isError: models.error != nil
            )
            Button("Download Model") {
                Task { await model.downloadModels() }
            }
        }
    }

    private func notice(_ title: String, _ detail: String, isError: Bool = false) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
            Text(detail)
                .font(.subheadline)
                .foregroundStyle(isError ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
        }
        .padding(.vertical, 2)
    }

    // MARK: - Tracks

    private var tracksSection: some View {
        Section("Tracks") {
            if model.jobs.isEmpty {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Nothing separated yet")
                    Text("Load a track on a deck and its stems are kept here until you remove them.")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                .padding(.vertical, 2)
            }
            ForEach(model.jobs) { job in
                TrackRow(
                    job: job,
                    expanded: Binding(
                        get: { expanded.contains(job.id) },
                        set: { open in
                            if open { expanded.insert(job.id) } else { expanded.remove(job.id) }
                        }
                    ),
                    play: { stem in openPreview(job, solo: stem.name) }
                )
            }
        }
    }

    // MARK: - Settings

    private var settingsSection: some View {
        Section {
            if let summary = model.modelSummary {
                LabeledContent("Model", value: summary)
            }
            if let health = model.health {
                LabeledContent(
                    "Access",
                    value: health.tokenRequired ? "Token required" : "Anyone on this network"
                )
            }
            Toggle(
                "Keep Screen Awake",
                isOn: Binding(
                    get: { model.keepAwake },
                    set: { model.setKeepAwake($0) }
                )
            )
        } header: {
            Text("Server")
        } footer: {
            Text("The server runs only while this app is in front. Keep the screen awake, or use Guided Access.")
        }
    }
}

// MARK: - Track row

private struct TrackRow: View {
    @EnvironmentObject private var model: AppModel
    let job: StemdClient.Job
    @Binding var expanded: Bool
    let play: (StemdClient.Stem) -> Void

    var body: some View {
        Group {
            if job.isDone && !job.stems.isEmpty {
                DisclosureGroup(isExpanded: $expanded) {
                    ForEach(job.stems) { stem in
                        StemRow(stem: stem) {
                            play(stem)
                        }
                    }
                    if let timing {
                        Text(timing)
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                } label: {
                    summary
                }
            } else {
                summary
            }
        }
        .swipeActions(edge: .trailing) {
            if job.isSettled {
                Button(role: .destructive) {
                    Task { await model.remove(job) }
                } label: {
                    Label("Remove", systemImage: "trash")
                }
            }
        }
        .contextMenu {
            if job.isSettled {
                Button(role: .destructive) {
                    Task { await model.remove(job) }
                } label: {
                    Label("Remove from Cache", systemImage: "trash")
                }
            }
        }
    }

    private var summary: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 12) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(job.displayTitle)
                        .font(.headline.weight(.bold))
                        .lineLimit(1)
                    if let artist = job.displayArtist {
                        Text(artist)
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    }
                    if let state = job.stateText {
                        Text(state)
                            .font(.subheadline)
                            .foregroundStyle(job.isFailed ? AnyShapeStyle(.red) : AnyShapeStyle(.secondary))
                            .lineLimit(job.isFailed ? 3 : 1)
                    }
                }
                Spacer(minLength: 8)
                if let trailing {
                    Text(trailing)
                        .font(.subheadline)
                        .monospacedDigit()
                        .foregroundStyle(.secondary)
                        .contentTransition(.numericText())
                }
            }
            if job.isRunning, let progress = job.progress {
                ProgressView(value: Double(progress.fraction))
                    .animation(.default, value: progress.fraction)
            }
        }
        .padding(.vertical, 2)
    }

    private var trailing: String? {
        if job.isRunning { return job.progress?.percentText }
        return job.lengthText
    }

    private var timing: String? {
        guard let took = job.tookText else { return nil }
        guard let speed = job.speedText else { return "Separated in \(took)" }
        return "Separated in \(took), \(speed) realtime"
    }
}

private struct StemRow: View {
    let stem: StemdClient.Stem
    let play: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Label {
                Text(stem.displayName)
            } icon: {
                Image(systemName: stem.symbol)
                    .foregroundStyle(stem.color)
            }
            Spacer(minLength: 8)
            Text(stem.detailText)
                .font(.subheadline)
                .monospacedDigit()
                .foregroundStyle(.secondary)
            Button(action: play) {
                Image(systemName: "play.fill")
                    .frame(width: 32, height: 32)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.borderless)
            .accessibilityLabel("Preview \(stem.name)")
        }
    }
}
