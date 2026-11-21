import AVFoundation
import Darwin
import SwiftUI
import UIKit

/// Which of the two screens is shown. The raw values are what an installed app
/// already has in its defaults, so they stay as they were.
enum AppAppearance: String {
    /// The standard screen: a grouped list, sized for arm's length.
    case console
    /// The booth display: black, oversized, read from across the room.
    case cdj
}

enum ServerStatus {
    case starting
    case running
    case stopped
}

extension AppModel {
    var status: ServerStatus {
        if starting { return .starting }
        if serverDown || health == nil { return .stopped }
        return .running
    }

    /// The address a deck should use, when the server is up and the device is
    /// on a network a deck could share.
    var address: String? {
        guard status == .running else { return nil }
        return StemdServer.shared.shareBase()?.absoluteString
    }

    /// "Fast · F16": the preset and the precision it is running at.
    var modelSummary: String? {
        guard let health, status == .running else { return nil }
        let preset = health.preset.capitalized
        guard let precision = health.modelId.split(separator: "-").last else { return preset }
        return "\(preset) · \(precision.uppercased())"
    }

    /// The job being worked on, or the next one in line.
    var activeJob: StemdClient.Job? {
        jobs.first { $0.state == "running" } ?? jobs.first { $0.state == "queued" }
    }
}

extension StemdClient.Health.Models {
    var fraction: Double? {
        total > 0 ? Double(done) / Double(total) : nil
    }

    var percentText: String {
        fraction.map { "\(Int(($0 * 100).rounded()))%" } ?? ""
    }
}

extension StemdClient.Progress {
    var percentText: String {
        "\(Int((Double(fraction) * 100).rounded()))%"
    }

    var stageText: String {
        switch stage {
        case "queued": return "Waiting"
        case "analysing": return "Analysing"
        case "separating": return "Separating"
        case "reconstructing": return "Reconstructing"
        case "writing": return "Writing stems"
        default: return stage.replacingOccurrences(of: "_", with: " ").capitalized
        }
    }
}

extension StemdClient.Job {
    var isRunning: Bool { state == "running" }
    var isWaiting: Bool { state == "queued" }
    var isDone: Bool { state == "done" }
    var isFailed: Bool { state == "failed" }

    /// Finished one way or another, so removing it interrupts nothing.
    var isSettled: Bool { !isRunning && !isWaiting }

    var displayTitle: String {
        title.flatMap { $0.isEmpty ? nil : $0 } ?? "Untitled track"
    }

    var displayArtist: String? {
        let text = artist?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return text.isEmpty ? nil : text
    }

    /// The track's own length, from the geometry the server reports with a
    /// finished job.
    var lengthText: String? {
        guard let frames, frames > 0, let sampleRate, sampleRate > 0 else { return nil }
        let seconds = Int((Double(frames) / Double(sampleRate)).rounded())
        return String(format: "%d:%02d", seconds / 60, seconds % 60)
    }

    /// How long the separation itself took, not how long ago it finished.
    var tookText: String? {
        guard let seconds else { return nil }
        if seconds < 60 {
            return String(format: "%.1f s", seconds)
        }
        let total = Int(seconds.rounded())
        return String(format: "%d:%02d", total / 60, total % 60)
    }

    var speedText: String? {
        guard let realtime, realtime.isFinite, realtime > 0 else { return nil }
        return String(format: "%.1f×", realtime)
    }

    /// "3.2 s · 12.4×": both, or whichever the server reported.
    var timingText: String? {
        let parts = [tookText, speedText].compactMap { $0 }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    /// When the separation was requested, and when it settled. Nil for cached
    /// entries written before the server recorded times.
    var requestedAt: Date? { date(fromUnix: createdUnix) }
    var finishedAt: Date? { date(fromUnix: finishedUnix) }

    private func date(fromUnix unix: UInt64?) -> Date? {
        guard let unix, unix > 0 else { return nil }
        return Date(timeIntervalSince1970: TimeInterval(unix))
    }

    /// One line under the title for anything that is not simply finished.
    var stateText: String? {
        switch state {
        case "running": return progress?.stageText ?? "Working"
        case "queued": return "Waiting"
        case "failed": return error ?? "Separation failed"
        case "cancelled": return "Stopped"
        default: return nil
        }
    }
}

extension StemdClient.Stem {
    var displayName: String { name.capitalized }

    var detailText: String {
        let size = ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
        let kind = contentType.replacingOccurrences(of: "audio/", with: "").uppercased()
        return "\(kind) · \(size)"
    }

    var symbol: String {
        switch name {
        case "vocals": return "music.mic"
        case "harmonics": return "pianokeys"
        case "drums": return "metronome"
        default: return "waveform"
        }
    }

    /// The three colours of the app icon, one per part.
    var color: Color {
        switch name {
        case "vocals": return Color(red: 0.89, green: 0.42, blue: 0.38)
        case "drums": return Color(red: 0.38, green: 0.70, blue: 0.47)
        default: return Color(red: 0.36, green: 0.55, blue: 0.93)
        }
    }
}

// MARK: - Stem preview

/// A job's stems mixed for preview: one player node per stem into a common
/// engine, so the stems share a sample clock, and one EQ per stem to undo the
/// server's per-stem transfer gain. The stems are fetched from the app's own
/// server (loopback, so no local-network permission is involved) and played
/// from temporary files, because the server sends them with a content type
/// rather than a file extension.
@MainActor
final class StemPreview: ObservableObject {
    struct Channel: Identifiable {
        let stem: StemdClient.Stem
        var volume: Double
        var id: String { stem.name }
    }

    let job: StemdClient.Job
    @Published private(set) var channels: [Channel]
    @Published private(set) var isPlaying = false
    @Published private(set) var isLoading = false
    @Published private(set) var ready = false
    @Published private(set) var error: String?
    @Published private(set) var position: TimeInterval = 0
    @Published private(set) var duration: TimeInterval = 0

    /// Built after the audio session is active: an engine made earlier can
    /// latch onto the default session's format and refuse to start.
    private var engine: AVAudioEngine?
    private var nodes: [String: AVAudioPlayerNode] = [:]
    private var equalizers: [String: AVAudioUnitEQ] = [:]
    private var files: [String: AVAudioFile] = [:]
    private var directory: URL?
    private var closed = false
    private var ticks: Task<Void, Never>?
    /// Track position the currently scheduled segments start from.
    private var segmentStart: TimeInterval = 0
    /// Bumped for every schedule, so callbacks from a seek's old segments are
    /// ignored.
    private var generation = 0
    private var pending: Set<String> = []

    /// The stem whose play button opened the sheet starts alone; the others
    /// can be mixed in from the sliders. Passing nil starts every stem at once.
    init(job: StemdClient.Job, solo: String? = nil) {
        self.job = job
        channels = job.stems.map {
            Channel(stem: $0, volume: solo == nil || solo == $0.name ? 1 : 0)
        }
    }

    func load() async {
        guard !ready, !isLoading else { return }
        guard !channels.isEmpty else {
            error = "This job has no stems to play."
            return
        }
        isLoading = true
        error = nil
        defer { isLoading = false }
        let job = self.job

        do {
            let session = AVAudioSession.sharedInstance()
            try session.setCategory(.playback, mode: .default)
            try session.setActive(true)

            let engine = AVAudioEngine()
            self.engine = engine

            let directory = FileManager.default.temporaryDirectory
                .appendingPathComponent("stemd-preview-\(UUID().uuidString)", isDirectory: true)
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
            self.directory = directory

            let base = StemdServer.shared.loopback()
            for channel in channels {
                guard let url = URL(string: channel.stem.url, relativeTo: base) else {
                    throw PreviewError.message("The \(channel.stem.displayName) stem has no URL.")
                }
                let (download, response) = try await URLSession.shared.download(from: url)
                let status = (response as? HTTPURLResponse)?.statusCode ?? 0
                guard status == 200 else {
                    throw PreviewError.message("The server would not send the \(channel.stem.displayName) stem (HTTP \(status)).")
                }
                let kind = channel.stem.contentType.split(separator: "/").last.map(String.init) ?? "audio"
                let extensionName = kind == "octet-stream" ? "pcm" : kind
                let downloaded = directory.appendingPathComponent("\(channel.stem.name).\(extensionName)")
                try FileManager.default.moveItem(at: download, to: downloaded)

                // A raw PCM stem can be hundreds of megabytes; copying it into
                // a WAV on the main actor freezes the app and everything it is
                // hosting, which reads as the server hanging.
                let readable = try await Task.detached(priority: .userInitiated) {
                    try Self.readableFile(downloaded, stem: channel.stem, job: job, in: directory)
                }.value
                let file: AVAudioFile
                do {
                    file = try AVAudioFile(forReading: readable)
                } catch {
                    throw PreviewError.message("The \(channel.stem.displayName) stem could not be read: \(error.localizedDescription)")
                }
                let node = AVAudioPlayerNode()
                let equalizer = AVAudioUnitEQ(numberOfBands: 1)
                equalizer.bands[0].bypass = true
                // The API says sample * 1/gain; the server scales any stem that
                // peaks past full scale. Skipping this plays those stems quiet
                // and the parts no longer sum to the track.
                let gain = channel.stem.gain
                let factor = gain.isFinite && gain > 0 ? 1 / Double(gain) : 1
                equalizer.globalGain = Float(min(max(20 * log10(factor), -96), 24))

                engine.attach(node)
                engine.attach(equalizer)
                engine.connect(node, to: equalizer, format: file.processingFormat)
                engine.connect(equalizer, to: engine.mainMixerNode, format: file.processingFormat)
                node.volume = Float(channel.volume)

                nodes[channel.id] = node
                equalizers[channel.id] = equalizer
                files[channel.id] = file
            }

            // The sheet can be dismissed while the stems are still coming down.
            guard !closed, !Task.isCancelled else { return }
            duration = files.values
                .map { Double($0.length) / $0.processingFormat.sampleRate }
                .max() ?? 0
            engine.prepare()
            ready = true
            startTicking()
            schedule(from: 0)
            startTogether()
        } catch {
            guard !closed, !Task.isCancelled else { return }
            self.error = error.localizedDescription
            tearDown()
        }
    }

    func togglePlayback() {
        isPlaying ? pause() : play()
    }

    private func play() {
        guard ready, !nodes.isEmpty else { return }
        if position >= duration - 0.05 {
            schedule(from: 0)
        }
        startTogether()
    }

    /// Start every node on the same host time. Starting them in a loop instead
    /// offsets the stems and comb-filters the sum.
    private func startTogether() {
        guard let engine, !nodes.isEmpty else { return }
        if !engine.isRunning {
            do {
                try engine.start()
            } catch {
                // A route can be momentarily unavailable; activating the
                // session again and retrying once is enough in practice.
                try? AVAudioSession.sharedInstance().setActive(true)
                do {
                    try engine.start()
                } catch {
                    self.error = "Audio could not start: \(error.localizedDescription)"
                    return
                }
            }
        }
        error = nil
        let start = AVAudioTime(
            hostTime: mach_absolute_time() + AVAudioTime.hostTime(forSeconds: 0.12)
        )
        for node in nodes.values { node.play(at: start) }
        isPlaying = true
    }

    private func pause() {
        position = currentPosition()
        for node in nodes.values { node.pause() }
        isPlaying = false
    }

    /// Schedule every stem from the same point in the track. Their segments
    /// stay queued until `play(at:)`, so a seek does not start them one at a
    /// time.
    private func schedule(from time: TimeInterval) {
        generation += 1
        let gen = generation
        pending = Set(nodes.keys)
        for (id, node) in nodes {
            node.stop()
            guard let file = files[id] else {
                pending.remove(id)
                continue
            }
            let rate = file.processingFormat.sampleRate
            let first = min(
                max(AVAudioFramePosition((time * rate).rounded()), 0),
                file.length
            )
            let count = AVAudioFrameCount(max(0, file.length - first))
            guard count > 0 else {
                pending.remove(id)
                continue
            }
            node.scheduleSegment(
                file,
                startingFrame: first,
                frameCount: count,
                at: nil,
                completionCallbackType: .dataPlayedBack
            ) { [weak self] _ in
                Task { @MainActor in self?.segmentFinished(id, generation: gen) }
            }
        }
        segmentStart = time
        position = time
    }

    private func segmentFinished(_ id: String, generation gen: Int) {
        guard !closed, gen == generation else { return }
        pending.remove(id)
        guard pending.isEmpty else { return }
        isPlaying = false
        // Leave the next Play starting at the top.
        schedule(from: 0)
    }

    private func currentPosition() -> TimeInterval {
        for node in nodes.values {
            guard node.isPlaying,
                  let render = node.lastRenderTime,
                  render.isSampleTimeValid,
                  let time = node.playerTime(forNodeTime: render)
            else { continue }
            return segmentStart + Double(time.sampleTime) / time.sampleRate
        }
        return position
    }

    /// Follow the scheduled stems so the scrubber moves while they play.
    private func startTicking() {
        ticks?.cancel()
        ticks = Task { @MainActor [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .milliseconds(200))
                guard let self, !Task.isCancelled else { return }
                guard self.isPlaying else { continue }
                let current = self.currentPosition()
                if abs(current - self.position) > 0.05 {
                    self.position = min(current, self.duration)
                }
            }
        }
    }

    /// A deck can ask the server for headerless PCM (`pcm16`/`pcm32`, served
    /// as `application/octet-stream`). AVFoundation needs a container, so put
    /// the same samples behind a WAV header; nothing is decoded or rescaled.
    nonisolated private static func readableFile(
        _ file: URL,
        stem: StemdClient.Stem,
        job: StemdClient.Job,
        in directory: URL
    ) throws -> URL {
        guard stem.contentType == "application/octet-stream" else { return file }
        guard let format = job.format?.lowercased(),
              let sampleRate = job.sampleRate,
              let channels = job.channels,
              channels > 0 else {
            throw PreviewError.message("The \(stem.displayName) stem is raw PCM without its sample format.")
        }

        let isFloat: Bool
        let bits: Int
        switch format {
        case "pcm16", "s16le", "s16":
            isFloat = false
            bits = 16
        case "pcm32", "f32le", "f32":
            isFloat = true
            bits = 32
        default:
            throw PreviewError.message("The \(stem.displayName) stem uses an unknown PCM format: \(format).")
        }

        let attributes = try FileManager.default.attributesOfItem(atPath: file.path)
        guard let size = (attributes[.size] as? NSNumber)?.uint64Value,
              size > 0,
              size <= UInt64(UInt32.max) - 36 else {
            throw PreviewError.message("The \(stem.displayName) stem has an unsupported size.")
        }
        let blockAlign = channels * bits / 8
        var header = Data()
        header.append(contentsOf: Array("RIFF".utf8))
        append(UInt32(36 + size), to: &header)
        header.append(contentsOf: Array("WAVEfmt ".utf8))
        append(UInt32(16), to: &header)
        append(UInt16(isFloat ? 3 : 1), to: &header)
        append(UInt16(channels), to: &header)
        append(sampleRate, to: &header)
        append(UInt32(sampleRate) * UInt32(blockAlign), to: &header)
        append(UInt16(blockAlign), to: &header)
        append(UInt16(bits), to: &header)
        header.append(contentsOf: Array("data".utf8))
        append(UInt32(size), to: &header)

        let wrapped = directory.appendingPathComponent("\(stem.name)-\(format).wav")
        guard FileManager.default.createFile(atPath: wrapped.path, contents: nil) else {
            throw PreviewError.message("Could not write the \(stem.displayName) preview file.")
        }
        let output = try FileHandle(forWritingTo: wrapped)
        defer { try? output.close() }
        try output.write(contentsOf: header)
        let input = try FileHandle(forReadingFrom: file)
        defer { try? input.close() }
        while let chunk = try input.read(upToCount: 1 << 20), !chunk.isEmpty {
            try output.write(contentsOf: chunk)
        }
        try? FileManager.default.removeItem(at: file)
        return wrapped
    }

    nonisolated private static func append<T: FixedWidthInteger>(_ value: T, to data: inout Data) {
        withUnsafeBytes(of: value.littleEndian) { data.append(contentsOf: $0) }
    }

    /// Move every stem to the same point in the track.
    func seek(to time: TimeInterval) {
        let clamped = min(max(time, 0), duration)
        let wasPlaying = isPlaying
        isPlaying = false
        schedule(from: clamped)
        if wasPlaying { startTogether() }
    }

    func volume(for id: String) -> Binding<Double> {
        Binding(
            get: { self.channels.first { $0.id == id }?.volume ?? 0 },
            set: { self.setVolume($0, for: id) }
        )
    }

    func setVolume(_ volume: Double, for id: String) {
        guard let index = channels.firstIndex(where: { $0.id == id }) else { return }
        channels[index].volume = volume
        nodes[id]?.volume = Float(volume)
    }

    func tearDown() {
        closed = true
        ticks?.cancel()
        ticks = nil
        generation += 1
        pending.removeAll()
        for node in nodes.values { node.stop() }
        engine?.stop()
        engine = nil
        nodes.removeAll()
        equalizers.removeAll()
        files.removeAll()
        ready = false
        isPlaying = false
        position = 0
        duration = 0
        if let directory {
            try? FileManager.default.removeItem(at: directory)
        }
        directory = nil
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }
}

private enum PreviewError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        switch self {
        case .message(let text): return text
        }
    }
}

/// The player's contents — transport, scrubber and one compact volume dial
/// per stem — with no chrome of its own. Screens that already have a bottom
/// bar embed this inside it; screens that do not use `StemPlayerBar`.
struct StemPlayerControls: View {
    @ObservedObject var preview: StemPreview
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 12) {
                Button {
                    preview.togglePlayback()
                } label: {
                    Image(systemName: preview.isPlaying ? "pause.fill" : "play.fill")
                        .font(.system(size: 18, weight: .bold))
                        .foregroundStyle(.black)
                        .frame(width: 42, height: 42)
                        .background(Color.white, in: Circle())
                }
                .buttonStyle(.plain)
                .disabled(!preview.ready || preview.isLoading)
                .accessibilityLabel(preview.isPlaying ? "Pause" : "Play")

                VStack(alignment: .leading, spacing: 1) {
                    Text(preview.job.displayTitle)
                        .font(.system(size: 15, weight: .bold))
                        .foregroundStyle(.white)
                        .lineLimit(1)
                    if let artist = preview.job.displayArtist {
                        Text(artist)
                            .font(.system(size: 12))
                            .foregroundStyle(.white.opacity(0.55))
                            .lineLimit(1)
                    }
                }
                Spacer(minLength: 8)
                Button(action: close) {
                    Image(systemName: "xmark")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(.white.opacity(0.7))
                        .frame(width: 30, height: 30)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Close preview")
            }

            HStack(spacing: 8) {
                Text(clock(preview.position))
                    .frame(width: 34, alignment: .leading)
                Slider(
                    value: Binding(
                        get: { preview.position },
                        set: { preview.seek(to: $0) }
                    ),
                    in: 0...max(preview.duration, 0.01)
                )
                .tint(.white)
                .disabled(!preview.ready)
                .accessibilityLabel("Playback position")
                Text(clock(preview.duration))
                    .frame(width: 38, alignment: .trailing)
            }
            .font(.system(size: 11, weight: .medium).monospacedDigit())
            .foregroundStyle(.white.opacity(0.6))

            HStack(spacing: 16) {
                ForEach(preview.channels) { channel in
                    StemDial(
                        icon: channel.stem.symbol,
                        label: channel.stem.displayName,
                        color: channel.stem.color,
                        volume: preview.volume(for: channel.id)
                    )
                }
                Spacer(minLength: 0)
                if preview.isLoading {
                    ProgressView()
                        .tint(.white)
                        .scaleEffect(0.75)
                }
            }

            if let error = preview.error {
                Text(error)
                    .font(.system(size: 12))
                    .foregroundStyle(.red)
                    .lineLimit(2)
            }
        }
    }

    private func clock(_ seconds: TimeInterval) -> String {
        guard seconds.isFinite, seconds > 0 else { return "0:00" }
        let total = Int(seconds.rounded())
        return String(format: "%d:%02d", total / 60, total % 60)
    }
}

/// The player as a standalone full-width bottom bar, for screens that do not
/// already have one.
struct StemPlayerBar: View {
    @ObservedObject var preview: StemPreview
    let close: () -> Void

    var body: some View {
        StemPlayerControls(preview: preview, close: close)
            .padding(.horizontal, 16)
            .padding(.top, 10)
            .padding(.bottom, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Color(white: 0.08).ignoresSafeArea(edges: .bottom))
            .overlay(alignment: .top) {
                Rectangle()
                    .fill(Color.white.opacity(0.12))
                    .frame(height: 1)
            }
    }
}

/// One stem's volume: a small knob. Drag up and down on it; the coloured arc
/// shows where the level sits.
private struct StemDial: View {
    let icon: String
    let label: String
    let color: Color
    @Binding var volume: Double
    @State private var dragStart: Double?

    var body: some View {
        VStack(spacing: 3) {
            ZStack {
                Circle()
                    .stroke(Color.white.opacity(0.14), lineWidth: 3)
                Circle()
                    .trim(from: 0, to: max(volume, 0.001))
                    .stroke(color, style: StrokeStyle(lineWidth: 3, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                Image(systemName: icon)
                    .font(.system(size: 12, weight: .medium))
                    .foregroundStyle(.white.opacity(volume > 0.01 ? 0.95 : 0.4))
            }
            .frame(width: 38, height: 38)
            .contentShape(Circle())
            .gesture(
                DragGesture(minimumDistance: 0)
                    .onChanged { value in
                        if dragStart == nil { dragStart = volume }
                        let delta = -value.translation.height / 120
                        volume = min(max((dragStart ?? volume) + delta, 0), 1)
                    }
                    .onEnded { _ in dragStart = nil }
            )
            .accessibilityLabel(label)
            .accessibilityValue(percentText)
            .accessibilityAdjustableAction { direction in
                switch direction {
                case .increment: volume = min(volume + 0.1, 1)
                case .decrement: volume = max(volume - 0.1, 0)
                @unknown default: break
                }
            }
            Text(label)
                .font(.system(size: 9, weight: .medium))
                .foregroundStyle(.white.opacity(0.55))
                .lineLimit(1)
                .minimumScaleFactor(0.7)
                .frame(width: 48)
        }
    }

    private var percentText: String {
        let percent = Int((volume * 100).rounded())
        return "\(percent) percent"
    }
}

/// Copies the server's address and tells its label so for a moment.
struct CopyAddressButton<Label: View>: View {
    @State private var copied = false
    @ViewBuilder let label: (_ copied: Bool) -> Label

    var body: some View {
        Button {
            guard let address = StemdServer.shared.shareBase()?.absoluteString else { return }
            UIPasteboard.general.string = address
            copied = true
            Task {
                try? await Task.sleep(for: .seconds(1.5))
                copied = false
            }
        } label: {
            label(copied)
        }
        .sensoryFeedback(.success, trigger: copied) { _, now in now }
        .accessibilityLabel("Copy the server address")
    }
}

/// What both screens do the same way: start the server, bring it back when
/// the app returns to the front, and present the clear confirmation and
/// errors.
struct HomeActions: ViewModifier {
    @EnvironmentObject private var model: AppModel
    @Environment(\.scenePhase) private var scenePhase
    @Binding var confirmingClear: Bool

    func body(content: Content) -> some View {
        content
            .confirmationDialog(
                "Clear the stem cache?",
                isPresented: $confirmingClear,
                titleVisibility: .visible
            ) {
                Button("Delete All Stems", role: .destructive) {
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
            .task { await model.start() }
            .onChange(of: scenePhase) { _, phase in
                if phase == .active { model.foregrounded() }
            }
    }
}
