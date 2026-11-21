import SwiftUI

/// The settings screen, including the wire log the app keeps.
///
/// Two lists on purpose:
///
/// * **Server** — every request the Rust listener answered, from this app's
///   loopback calls to a CDJ/emulator on the LAN, including ones rejected by
///   the token check. `LAN only` hides the app's own polling so a deck stands
///   out.
/// * **App** — the calls this Swift client made, with latency and bytes.
struct DebugView: View {
    enum Source: String, CaseIterable, Identifiable {
        case server = "Server"
        case app = "App"
        var id: String { rawValue }
    }

    @ObservedObject var log: APILog
    @EnvironmentObject private var model: AppModel
    @State private var source: Source = .server
    @State private var lanOnly = true
    @State private var server: [StemdClient.ServerRequest] = []

    private var shownServer: [StemdClient.ServerRequest] {
        lanOnly ? server.filter { !$0.loopback } : server
    }

    var body: some View {
        List {
            Section("Build") {
                LabeledContent("App", value: BuildInfo.version)
                LabeledContent("Binary", value: BuildInfo.binaryDateText)
                if let health = model.health {
                    LabeledContent("Server", value: health.version)
                }
                Text("The binary date is the app executable on this device; it changes when a new build is installed. Server is the version of the Rust library behind the listener.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            }
            Section {
                Toggle("Save Original PCM", isOn: Binding(
                    get: { model.savePCM },
                    set: { model.setSavePCM($0) }
                ))
                Text("Each new job also writes the mix the model separated to Documents/stemd-pcm as WAV. Compare it with the stems to hear what reconstruction is missing. Off by default because it doubles storage for the track.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
            } header: {
                Text("Diagnostics")
            } footer: {
                Text("Files → On My iPhone → stemd → stemd-pcm")
            }

            Section("Server") {
                LabeledContent("Port", value: "\(StemdServer.shared.port)")
                if let base = StemdServer.shared.shareBase() {
                    Text(base.absoluteString)
                        .font(.footnote.monospaced())
                        .textSelection(.enabled)
                }
                Text("Advertising _stemd._tcp over Bonjour")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                Toggle("Require token", isOn: Binding(
                    get: { model.tokenRequired },
                    set: { model.setTokenRequired($0) }
                ))
                if !model.tokenRequired {
                    Text("Off lets the CDJ/emulator connect. The server is open to every device on this network.")
                        .font(.footnote)
                        .foregroundStyle(.orange)
                }
                if let health = model.health {
                    LabeledContent("Preset", value: health.preset)
                    LabeledContent("Jobs", value: "\(health.jobs)")
                    LabeledContent("Model files", value: health.models.present ? "installed" : "missing")
                } else {
                    Text("No health response yet.")
                        .foregroundStyle(.secondary)
                }
                if let error = model.error {
                    Text(error).font(.footnote).foregroundStyle(.red)
                }
            }

            Section {
                Picker("Source", selection: $source) {
                    ForEach(Source.allCases) { source in
                        Text(source.rawValue).tag(source)
                    }
                }
                .pickerStyle(.segmented)

                switch source {
                case .server:
                    Toggle("LAN only", isOn: $lanOnly)
                    if shownServer.isEmpty {
                        Text("No requests yet. A deck's health probe will appear here.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                    ForEach(shownServer.reversed()) { request in
                        ServerRow(request: request)
                    }
                case .app:
                    Toggle("Pause", isOn: $log.paused)
                    Button("Clear") { log.clear() }
                    if log.calls.isEmpty {
                        Text("No calls recorded.")
                            .font(.footnote)
                            .foregroundStyle(.secondary)
                    }
                    ForEach(log.calls.reversed()) { call in
                        CallRow(call: call)
                    }
                }
            } header: {
                Text(
                    source == .server
                        ? "Server requests (\(shownServer.count))"
                        : "App calls (\(log.calls.count))"
                )
            }
        }
        .navigationTitle("Settings")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            // Poll while the screen is open, quietly: reading the log must not
            // add rows to it.
            while !Task.isCancelled {
                server = await model.serverRequests()
                try? await Task.sleep(for: .seconds(1))
            }
        }
    }
}

private struct ServerRow: View {
    let request: StemdClient.ServerRequest

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Text(request.method)
                .font(.caption2.monospaced().bold())
                .foregroundStyle(.secondary)
                .frame(width: 42, alignment: .leading)
            VStack(alignment: .leading, spacing: 2) {
                Text(request.path)
                    .font(.caption.monospaced())
                    .lineLimit(2)
                    .truncationMode(.middle)
                HStack(spacing: 8) {
                    Text(request.remote)
                        .foregroundStyle(request.loopback ? Color.secondary : Color.blue)
                    Text(request.date.formatted(date: .omitted, time: .standard))
                    Text("\(request.millis) ms")
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            Text("\(request.status)")
                .font(.caption.monospaced().bold())
                .foregroundStyle(color)
        }
        .padding(.vertical, 2)
    }

    private var color: Color {
        if request.status == 401 { return .orange }
        if (200..<300).contains(request.status) { return .green }
        return .red
    }
}

private struct CallRow: View {
    let call: APILog.Call

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Text(call.method)
                .font(.caption2.monospaced().bold())
                .foregroundStyle(.secondary)
                .frame(width: 42, alignment: .leading)
            VStack(alignment: .leading, spacing: 2) {
                Text(call.path)
                    .font(.caption.monospaced())
                    .lineLimit(2)
                    .truncationMode(.middle)
                HStack(spacing: 8) {
                    Text(call.at.formatted(date: .omitted, time: .standard))
                    if let millis = call.millis {
                        Text("\(millis) ms")
                    }
                    if let bytes = call.bytes {
                        Text("\(bytes) B")
                    }
                }
                .font(.caption2)
                .foregroundStyle(.secondary)
                if let error = call.error {
                    Text(error)
                        .font(.caption2)
                        .foregroundStyle(.red)
                        .lineLimit(2)
                }
            }
            Spacer(minLength: 4)
            Text(status)
                .font(.caption.monospaced().bold())
                .foregroundStyle(color)
        }
        .padding(.vertical, 2)
    }

    private var status: String {
        if call.error != nil { return "ERR" }
        if let status = call.status { return "\(status)" }
        return "…"
    }

    private var color: Color {
        if call.error != nil { return .red }
        guard let status = call.status else { return .secondary }
        if (200..<300).contains(status) { return .green }
        if status == 401 { return .orange }
        return .red
    }
}

/// What the installed bundle is, for the Debug screen.
///
/// The executable's date is the closest thing to a build stamp without a
/// generated source file: it is written when the app is linked (and again if
/// the installer rewrites it), so it changes when a new build reaches the
/// device. The server's own version comes from the Rust library and is shown
/// beside it.
enum BuildInfo {
    static var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "unknown"
        let build = info?["CFBundleVersion"] as? String ?? "?"
        return "\(short) (\(build))"
    }

    static var binaryDateText: String {
        guard
            let url = Bundle.main.executableURL,
            let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
            let date = attributes[.modificationDate] as? Date
        else {
            return "unknown"
        }
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy-MM-dd HH:mm:ss"
        return formatter.string(from: date)
    }
}
