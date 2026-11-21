import SwiftUI

private enum Booth {
    static let background = Color.black
    static let accent = Color(red: 0.27, green: 0.70, blue: 1.0)
    static let secondary = Color.white.opacity(0.62)
    static let hairline = Color.white.opacity(0.16)
    static let fill = Color.white.opacity(0.11)
    static let alert = Color(red: 1.0, green: 0.35, blue: 0.32)
}

/// One line in the activity log: a request, a finished separation, or a live job.
private struct BoothLogEvent: Identifiable {
    enum Kind {
        case requested, separating, separated, failed, stopped

        /// Terminal outcomes sort above the request they belong to when the
        /// two share a time, which happens for cached entries without stamps.
        var rank: Int {
            switch self {
            case .separated, .failed, .stopped: return 0
            case .separating: return 1
            case .requested: return 2
            }
        }
    }

    let id: String
    let kind: Kind
    let job: StemdClient.Job
    let time: Date?

    /// 24-hour, like the clock on a deck.
    static let clock: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm"
        formatter.locale = Locale(identifier: "en_US_POSIX")
        return formatter
    }()

    var label: String {
        switch kind {
        case .requested: return "Requested"
        case .separating: return job.isWaiting ? "Up next" : "Separating"
        case .separated: return "Separated"
        case .failed: return "Failed"
        case .stopped: return "Stopped"
        }
    }

    var color: Color {
        switch kind {
        case .requested: return Booth.secondary
        case .separating: return Booth.accent
        case .separated: return .green
        case .failed: return Booth.alert
        case .stopped: return Booth.secondary
        }
    }

    var detail: String? {
        switch kind {
        case .requested, .stopped:
            return nil
        case .separating:
            return job.progress?.stageText ?? (job.isWaiting ? "Waiting for a free worker" : nil)
        case .separated:
            guard let took = job.tookText else { return job.speedText.map { "\($0) realtime" } }
            return job.speedText.map { "Separated in \(took), \($0) realtime" } ?? "Separated in \(took)"
        case .failed:
            return job.error ?? "Separation failed"
        }
    }
}

/// The booth display: black, high contrast and oversized, so the three things
/// that matter next to a deck — is the server up, where is it, what is it
/// working on — can be read from across the booth rather than at arm's length.
struct BoothView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.horizontalSizeClass) private var sizeClass
    @AppStorage("stemd.appearance") private var appearance = AppAppearance.cdj
    @State private var confirmingClear = false
    @State private var preview: StemPreview?
    @State private var showDebug = ProcessInfo.processInfo.arguments.contains("--debug")

    var body: some View {
        ZStack {
            Booth.background.ignoresSafeArea()
            if sizeClass == .regular && !listed.isEmpty {
                // An iPad has room to keep the track list beside the status
                // instead of below the fold.
                HStack(alignment: .top, spacing: 0) {
                    ScrollView { overview.padding(32) }
                        .frame(maxWidth: .infinity)
                    Rectangle()
                        .fill(Booth.hairline)
                        .frame(width: 1)
                    ScrollView { tracks.padding(32) }
                        .frame(maxWidth: .infinity)
                }
            } else {
                ScrollView {
                    VStack(alignment: .leading, spacing: 32) {
                        overview
                        tracks
                    }
                    .padding(.horizontal, sizeClass == .regular ? 32 : 20)
                    .padding(.vertical, sizeClass == .regular ? 32 : 12)
                }
            }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) { actionBar }
        .sheet(isPresented: $showDebug) {
            NavigationStack {
                DebugView(log: model.apiLog)
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { showDebug = false }
                        }
                    }
            }
        }
        .modifier(HomeActions(confirmingClear: $confirmingClear))
        .preferredColorScheme(.dark)
        .onDisappear { closePreview() }
    }

    /// Start a fresh mix for this job, replacing any preview already playing.
    /// Tapping the same track again pauses or resumes it.
    private func openPreview(_ job: StemdClient.Job) {
        if let preview, preview.job.id == job.id, preview.ready {
            preview.togglePlayback()
            return
        }
        preview?.tearDown()
        let next = StemPreview(job: job)
        preview = next
        Task { await next.load() }
    }

    private func closePreview() {
        preview?.tearDown()
        preview = nil
    }

    private var overview: some View {
        VStack(alignment: .leading, spacing: 28) {
            server
            notices
            Rectangle()
                .fill(Booth.hairline)
                .frame(height: 1)
            activity
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: - Server

    private var server: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .center, spacing: 12) {
                Circle()
                    .fill(statusColor)
                    .frame(width: 14, height: 14)
                Text(statusText)
                    .font(.system(size: 34, weight: .bold))
                    .foregroundStyle(.white)
                Spacer(minLength: 8)
                if let summary = model.modelSummary {
                    Text(summary)
                        .font(.system(size: 17, weight: .medium))
                        .foregroundStyle(Booth.secondary)
                }
            }

            if let address = model.address {
                CopyAddressButton { copied in
                    HStack(spacing: 12) {
                        Text(address)
                            .font(.system(size: 22, weight: .medium, design: .monospaced))
                            .foregroundStyle(.white.opacity(0.9))
                            .lineLimit(1)
                            .minimumScaleFactor(0.5)
                        Spacer(minLength: 4)
                        Image(systemName: copied ? "checkmark" : "doc.on.doc")
                            .font(.system(size: 19, weight: .semibold))
                            .foregroundStyle(copied ? Color.green : Booth.secondary)
                            .contentTransition(.symbolEffect(.replace))
                    }
                    .frame(minHeight: 44)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            } else if model.status == .running {
                Text("Join the deck's network so it can find this device.")
                    .font(.system(size: 19, weight: .medium))
                    .foregroundStyle(Booth.secondary)
            }

            Toggle(
                isOn: Binding(
                    get: { model.keepAwake },
                    set: { model.setKeepAwake($0) }
                )
            ) {
                Text("Keep Screen Awake")
                    .font(.system(size: 19, weight: .medium))
                    .foregroundStyle(Booth.secondary)
            }
            .tint(Booth.accent)
            .padding(.top, 4)
        }
    }

    private var statusText: String {
        switch model.status {
        case .starting: return "Starting"
        case .stopped: return "Stopped"
        case .running: return model.address == nil ? "No network" : "Serving"
        }
    }

    private var statusColor: Color {
        switch model.status {
        case .starting: return .orange
        case .stopped: return Booth.alert
        case .running: return model.address == nil ? .orange : .green
        }
    }

    // MARK: - Notices

    /// The things that stop a deck getting stems. Shown only while they apply.
    @ViewBuilder
    private var notices: some View {
        if model.status == .running, let health = model.health {
            if health.tokenRequired {
                notice(
                    "Decks can't connect",
                    "A token is required, and the CDJ sidecar has no way to send one.",
                    action: "Allow Decks"
                ) {
                    model.setTokenRequired(false)
                }
            }
            if health.models.downloading {
                VStack(alignment: .leading, spacing: 10) {
                    HStack(alignment: .firstTextBaseline) {
                        Text("Downloading the model")
                            .font(.system(size: 20, weight: .semibold))
                            .foregroundStyle(.white)
                        Spacer()
                        Text(health.models.percentText)
                            .font(.system(size: 20, weight: .semibold))
                            .monospacedDigit()
                            .foregroundStyle(.white)
                    }
                    BoothProgressBar(fraction: health.models.fraction ?? 0)
                        .frame(height: 8)
                }
            } else if !health.models.present {
                notice(
                    "The model isn't installed",
                    health.models.error ?? "Download it once to start separating tracks.",
                    action: "Download"
                ) {
                    Task { await model.downloadModels() }
                }
            }
        }
    }

    private func notice(
        _ title: String,
        _ detail: String,
        action: String,
        perform: @escaping () -> Void
    ) -> some View {
        HStack(alignment: .center, spacing: 16) {
            VStack(alignment: .leading, spacing: 3) {
                Text(title)
                    .font(.system(size: 20, weight: .semibold))
                    .foregroundStyle(.orange)
                Text(detail)
                    .font(.system(size: 16))
                    .foregroundStyle(Booth.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 8)
            Button(action: perform) {
                Text(action)
                    .font(.system(size: 17, weight: .semibold))
                    .foregroundStyle(.white)
                    .padding(.horizontal, 18)
                    .frame(height: 44)
                    .background(Booth.fill, in: Capsule())
            }
            .buttonStyle(.plain)
        }
    }

    // MARK: - Activity

    /// The left column reads as a log: every request and how it ended, newest
    /// first. The live job keeps its progress inline, so the booth still shows
    /// where the current separation is.
    private var activity: some View {
        let events = logEvents
        return VStack(alignment: .leading, spacing: 14) {
            BoothLabel("Activity")
            if events.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Nothing separated yet")
                        .font(.system(size: 28, weight: .bold))
                        .foregroundStyle(.white)
                    Text("Load a track on a deck and it will appear here.")
                        .font(.system(size: 19, weight: .medium))
                        .foregroundStyle(Booth.secondary)
                }
            } else {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(events.enumerated()), id: \.element.id) { index, event in
                        eventRow(event, last: index == events.count - 1)
                    }
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    /// At most a screenful; the track list on the right holds the full history.
    private var logEvents: [BoothLogEvent] {
        var events: [BoothLogEvent] = []
        for job in model.jobs {
            switch job.state {
            case "queued", "running":
                events.append(
                    BoothLogEvent(id: "\(job.id)-live", kind: .separating, job: job, time: job.requestedAt)
                )
            case "done":
                events.append(
                    BoothLogEvent(
                        id: "\(job.id)-separated",
                        kind: .separated,
                        job: job,
                        time: job.finishedAt ?? job.requestedAt
                    )
                )
                events.append(
                    BoothLogEvent(id: "\(job.id)-requested", kind: .requested, job: job, time: job.requestedAt)
                )
            case "failed":
                events.append(
                    BoothLogEvent(
                        id: "\(job.id)-failed",
                        kind: .failed,
                        job: job,
                        time: job.finishedAt ?? job.requestedAt
                    )
                )
                events.append(
                    BoothLogEvent(id: "\(job.id)-requested", kind: .requested, job: job, time: job.requestedAt)
                )
            case "cancelled":
                events.append(
                    BoothLogEvent(
                        id: "\(job.id)-stopped",
                        kind: .stopped,
                        job: job,
                        time: job.finishedAt ?? job.requestedAt
                    )
                )
                events.append(
                    BoothLogEvent(id: "\(job.id)-requested", kind: .requested, job: job, time: job.requestedAt)
                )
            default:
                break
            }
        }
        events.sort { lhs, rhs in
            let left = lhs.time ?? .distantPast
            let right = rhs.time ?? .distantPast
            if left != right { return left > right }
            if lhs.job.created != rhs.job.created { return lhs.job.created > rhs.job.created }
            // With no usable times, put the outcome above the request it belongs to.
            return lhs.kind.rank < rhs.kind.rank
        }
        return Array(events.prefix(12))
    }

    private func eventRow(_ event: BoothLogEvent, last: Bool) -> some View {
        HStack(alignment: .top, spacing: 12) {
            timestamp(event.time)
            VStack(spacing: 0) {
                Circle()
                    .fill(event.color)
                    .frame(width: 9, height: 9)
                    .padding(.top, 6)
                if !last {
                    Rectangle()
                        .fill(Booth.hairline)
                        .frame(width: 1)
                        .frame(maxHeight: .infinity)
                }
            }
            .frame(width: 9)
            VStack(alignment: .leading, spacing: 3) {
                Text(event.label.uppercased())
                    .font(.system(size: 13, weight: .semibold))
                    .tracking(1.1)
                    .foregroundStyle(event.color)
                Text(event.job.displayTitle)
                    .font(.system(size: event.kind == .separating ? 27 : 21, weight: .bold))
                    .foregroundStyle(.white)
                    .lineLimit(2)
                    .minimumScaleFactor(0.8)
                if let artist = event.job.displayArtist {
                    Text(artist)
                        .font(.system(size: 16))
                        .foregroundStyle(Booth.secondary)
                        .lineLimit(1)
                }
                if let detail = event.detail {
                    Text(detail)
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(event.kind == .failed ? Booth.alert : Booth.secondary)
                        .lineLimit(event.kind == .failed ? 3 : 2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if event.kind == .separating, let progress = event.job.progress {
                    HStack(alignment: .center, spacing: 10) {
                        BoothProgressBar(fraction: Double(progress.fraction))
                            .frame(height: 7)
                        Text(progress.percentText)
                            .font(.system(size: 18, weight: .semibold))
                            .monospacedDigit()
                            .foregroundStyle(.white)
                            .contentTransition(.numericText())
                    }
                    .padding(.top, 6)
                    .animation(.default, value: progress.fraction)
                }
            }
            .padding(.bottom, 18)
            Spacer(minLength: 0)
        }
    }

    @ViewBuilder
    private func timestamp(_ date: Date?) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            if let date {
                if !Calendar.current.isDateInToday(date) {
                    Text(date, format: .dateTime.month(.abbreviated).day())
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(Booth.secondary.opacity(0.7))
                }
                Text(BoothLogEvent.clock.string(from: date))
                    .font(.system(size: 17, weight: .medium, design: .monospaced))
                    .foregroundStyle(Booth.secondary)
            } else {
                Text("--:--")
                    .font(.system(size: 17, weight: .medium, design: .monospaced))
                    .foregroundStyle(Booth.secondary.opacity(0.5))
            }
        }
        .frame(width: 56, alignment: .leading)
    }

    // MARK: - Tracks

    /// Everything except the job already shown large above.
    private var listed: [StemdClient.Job] {
        let active = model.activeJob?.id
        return model.jobs.filter { $0.id != active }
    }

    @ViewBuilder
    private var tracks: some View {
        if !listed.isEmpty {
            VStack(alignment: .leading, spacing: 4) {
                BoothLabel("Tracks")
                ForEach(listed) { job in
                    BoothTrackRow(job: job) {
                        openPreview(job)
                    }
                    if job.id != listed.last?.id {
                        Rectangle()
                            .fill(Booth.hairline)
                            .frame(height: 1)
                    }
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    // MARK: - Actions

    private var actionBar: some View {
        HStack(alignment: .center, spacing: 12) {
            if let preview {
                // The player lives in the bottom bar itself, beside the menu.
                StemPlayerControls(preview: preview) { closePreview() }
                    .frame(maxWidth: .infinity, alignment: .leading)
            } else if model.status == .stopped {
                primaryButton("Restart Server", symbol: "arrow.clockwise") {
                    Task { await model.restartServer() }
                }
            } else {
                Spacer(minLength: 0)
            }
            moreMenu
        }
        .padding(.horizontal, 20)
        .padding(.top, 12)
        .padding(.bottom, 8)
        .background(Booth.background)
        .overlay(alignment: .top) {
            Rectangle()
                .fill(Booth.hairline)
                .frame(height: 1)
        }
    }

    private var moreMenu: some View {
        Menu {
            Button {
                appearance = .console
            } label: {
                Label("Standard Display", systemImage: "textformat.size.smaller")
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
            Image(systemName: "ellipsis")
                .font(.system(size: 22, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: 60, height: 60)
                .background(Booth.fill, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        }
        .accessibilityLabel("More")
    }

    private func primaryButton(
        _ title: String,
        symbol: String,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Label(title, systemImage: symbol)
                .font(.system(size: 20, weight: .semibold))
                .foregroundStyle(.black)
                .frame(maxWidth: .infinity)
                .frame(height: 60)
                .background(Booth.accent, in: RoundedRectangle(cornerRadius: 16, style: .continuous))
        }
        .buttonStyle(.plain)
    }
}

private struct BoothLabel: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(text)
            .font(.system(size: 15, weight: .semibold))
            .foregroundStyle(Booth.accent)
    }
}

private struct BoothProgressBar: View {
    let fraction: Double

    var body: some View {
        GeometryReader { proxy in
            ZStack(alignment: .leading) {
                Capsule().fill(Booth.fill)
                Capsule()
                    .fill(Booth.accent)
                    .frame(width: proxy.size.width * min(max(fraction, 0), 1))
                    .animation(.default, value: fraction)
            }
        }
    }
}

private struct BoothTrackRow: View {
    @EnvironmentObject private var model: AppModel
    let job: StemdClient.Job
    let play: () -> Void
    @State private var offset: CGFloat = 0
    @State private var dragBase: CGFloat?
    private let reveal: CGFloat = 84

    private var playable: Bool { job.isDone && !job.stems.isEmpty }

    var body: some View {
        ZStack(alignment: .trailing) {
            if job.isSettled {
                Button(role: .destructive) {
                    remove()
                } label: {
                    VStack(spacing: 4) {
                        Image(systemName: "trash")
                            .font(.system(size: 19, weight: .semibold))
                        Text("Remove")
                            .font(.system(size: 11, weight: .semibold))
                    }
                    .foregroundStyle(.white)
                    .frame(width: reveal)
                    .frame(maxHeight: .infinity)
                    .background(Booth.alert)
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Remove \(job.displayTitle) from cache")
            }

            row
                .padding(.vertical, 14)
                .contentShape(Rectangle())
                .background(Booth.background)
                .offset(x: offset)
                .simultaneousGesture(
                    TapGesture().onEnded {
                        if offset != 0 {
                            withAnimation(.snappy(duration: 0.2)) { offset = 0 }
                        }
                    }
                )
                .simultaneousGesture(swipe)
                .contextMenu {
                    if job.isSettled {
                        Button(role: .destructive) {
                            remove()
                        } label: {
                            Label("Remove from Cache", systemImage: "trash")
                        }
                    }
                }
        }
        .clipped()
    }

    private var row: some View {
        HStack(alignment: .center, spacing: 16) {
            VStack(alignment: .leading, spacing: 3) {
                Text(job.displayTitle)
                    .font(.system(size: 22, weight: .bold))
                    .foregroundStyle(.white)
                    .lineLimit(1)
                if let artist = job.displayArtist {
                    Text(artist)
                        .font(.system(size: 17))
                        .foregroundStyle(Booth.secondary)
                        .lineLimit(1)
                }
                if let state = job.stateText {
                    Text(state)
                        .font(.system(size: 17, weight: .medium))
                        .foregroundStyle(job.isFailed ? Booth.alert : Booth.secondary)
                        .lineLimit(job.isFailed ? 3 : 1)
                        .multilineTextAlignment(.leading)
                }
            }
            Spacer(minLength: 8)
            trailing
            if playable {
                Button(action: play) {
                    Image(systemName: "play.fill")
                        .font(.system(size: 19, weight: .medium))
                        .foregroundStyle(Booth.accent)
                        .frame(width: 44, height: 44)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel("Preview \(job.displayTitle)")
            }
        }
    }

    /// Horizontal only, and simultaneous with the enclosing scroll so a
    /// vertical drag still scrolls the list.
    private var swipe: some Gesture {
        DragGesture(minimumDistance: 12)
            .onChanged { value in
                guard job.isSettled else { return }
                guard abs(value.translation.width) > abs(value.translation.height) else { return }
                if dragBase == nil { dragBase = offset }
                offset = min(0, max((dragBase ?? 0) + value.translation.width, -reveal - 40))
            }
            .onEnded { value in
                dragBase = nil
                guard job.isSettled else {
                    withAnimation(.snappy(duration: 0.2)) { offset = 0 }
                    return
                }
                guard abs(value.translation.width) > abs(value.translation.height) else { return }
                if offset < -reveal - 30 {
                    withAnimation(.snappy(duration: 0.2)) { offset = 0 }
                    remove()
                } else {
                    withAnimation(.snappy(duration: 0.2)) {
                        offset = offset < -reveal / 2 ? -reveal : 0
                    }
                }
            }
    }

    private func remove() {
        Task { await model.remove(job) }
    }

    /// The right-hand readout: the track's own length, then how long the
    /// separation took and how much faster than realtime it ran.
    @ViewBuilder
    private var trailing: some View {
        if job.lengthText != nil || job.timingText != nil {
            VStack(alignment: .trailing, spacing: 2) {
                if let length = job.lengthText {
                    Text(length)
                        .font(.system(size: 21, weight: .semibold))
                        .monospacedDigit()
                        .foregroundStyle(.white)
                        .lineLimit(1)
                }
                if let timing = job.timingText {
                    Text(timing)
                        .font(.system(size: 16, weight: .medium))
                        .monospacedDigit()
                        .foregroundStyle(Booth.secondary)
                        .lineLimit(1)
                }
            }
        }
    }

}
