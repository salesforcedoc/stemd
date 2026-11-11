import Foundation

/// The process-wide server. Swift owns the lifecycle; Rust owns everything from
/// the listener down.
final class StemdServer {
    static let shared = StemdServer()

    private(set) var port: UInt16 = 0
    private(set) var token = ""
    /// Mirrors the Rust side's token switch, for the debug screen.
    private(set) var tokenRequired = true
    private let bonjour = BonjourAdvertiser()

    var running: Bool { port != 0 }

    /// Start the embedded server. Throws with the Rust-side message when it
    /// cannot, which is usually a missing weight file or a port that is taken.
    func start(preset: String = "fast") throws {
        let support = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        )
        let data = support.appendingPathComponent("stemd", isDirectory: true)
        try FileManager.default.createDirectory(at: data, withIntermediateDirectories: true)

        var config: [String: Any] = [
            "data_dir": data.path,
            "preset": preset,
            // Every interface on an ephemeral port: the stems are meant to be
            // fetched from another device. The token is generated in Rust and
            // returned below; nothing is reachable without it.
            "bind": "0.0.0.0:0",
            "max_track_secs": 900,
        ]
#if targetEnvironment(simulator)
        // The simulator shares the host's network, so a fixed port and a known
        // token let curl drive the embedded server during development. The
        // emulator's sidecar cannot send a token either, so the simulator build
        // starts open; a device keeps the token and offers the switch in Debug.
        // MLX cannot use Metal in the simulator, so this build is CPU-only.
        config["bind"] = "0.0.0.0:8420"
        config["token"] = "simulator"
        config["require_token"] = false
#endif
#if os(iOS)
        if ProcessInfo.processInfo.isiOSAppOnMac {
            // "Designed for iPad" runs the iOS app on Apple-silicon macOS.
            // Metal works there, unlike the simulator, so this is the fast
            // development path; fixed port and token for curl-driven tests.
            config["bind"] = "0.0.0.0:8420"
            config["token"] = "designed"
            config["require_token"] = false
        }
#endif
        let json = String(decoding: try JSONSerialization.data(withJSONObject: config), as: UTF8.self)
        guard let reply = json.withCString({ stemd_ios_start($0) }) else {
            throw StemdError.message("stemd did not return a start result")
        }
        defer { stemd_ios_free_string(reply) }

        let payload = String(cString: reply)
        guard let object = try JSONSerialization.jsonObject(with: Data(payload.utf8)) as? [String: Any] else {
            throw StemdError.message("unreadable start result: \(payload)")
        }
        if let error = object["error"] as? String {
            throw StemdError.message(error)
        }
        guard let port = (object["port"] as? NSNumber)?.uint16Value,
              let token = object["token"] as? String
        else {
            throw StemdError.message("start result has no port or token: \(payload)")
        }
        self.port = port
        self.token = token
        self.tokenRequired = (object["token_required"] as? Bool)
            ?? ((object["token_required"] as? NSNumber)?.boolValue ?? true)
        // Bonjour advertising needs no multicast entitlement, just the local
        // network permission and NSBonjourServices. Failure is not fatal: a
        // client can still type the address.
        bonjour.start(port: port)
    }

    func stop() {
        bonjour.stop()
        stemd_ios_stop()
        port = 0
        token = ""
    }

    /// Switch the token requirement at runtime, for clients (the CDJ sidecar)
    /// that have no way to send one.
    func setTokenRequired(_ required: Bool) {
        tokenRequired = required
        stemd_ios_set_token_required(required ? 1 : 0)
    }

    /// Loopback: what the app itself uses, and what never needs the local
    /// network permission.
    func loopback() -> URL {
        URL(string: "http://127.0.0.1:\(port)/v1")!
    }

    /// A base another device on the same network can reach, when there is one.
    func shareBase() -> URL? {
        localIPv4().flatMap { URL(string: "http://\($0):\(port)/v1") }
    }

    /// The address another device on the LAN should use.
    ///
    /// Wi-Fi first (`en0` on iOS), then any other Ethernet-like interface
    /// (`en1+`, where a USB-C adapter lands). Tunnel, cellular and AWDL/LLW
    /// interfaces are skipped: iOS lists them beside Wi-Fi, and taking the
    /// first IPv4 can hand back a VPN's `10.x` address that nothing on the
    /// local network can reach — which is what happened on an iPad whose Wi-Fi
    /// was fine and whose VPN was up.
    private func localIPv4() -> String? {
        var head: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&head) == 0, let first = head else { return nil }
        defer { freeifaddrs(head) }

        var ethernet: String?
        var other: String?
        for pointer in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let interface = pointer.pointee
            guard let address = interface.ifa_addr,
                  address.pointee.sa_family == UInt8(AF_INET)
            else { continue }
            let flags = interface.ifa_flags
            guard flags & UInt32(IFF_UP) != 0,
                  flags & UInt32(IFF_LOOPBACK) == 0
            else { continue }

            let name = String(cString: interface.ifa_name)
            var host = [CChar](repeating: 0, count: Int(NI_MAXHOST))
            let result = getnameinfo(
                address,
                socklen_t(address.pointee.sa_len),
                &host,
                socklen_t(host.count),
                nil,
                0,
                NI_NUMERICHOST
            )
            guard result == 0 else { continue }
            let text = String(cString: host)
            guard !text.hasPrefix("169.254.") else { continue }

            if name == "en0" {
                return text                      // Wi-Fi on iOS
            }
            if name.hasPrefix("en") {
                if ethernet == nil { ethernet = text }
                continue
            }
            // A tunnel, the cellular PDP interface, or AirDrop's radio is not
            // reachable from the deck's LAN.
            if name.hasPrefix("utun") || name.hasPrefix("ipsec") ||
                name.hasPrefix("tun") || name.hasPrefix("tap") ||
                name.hasPrefix("pdp_ip") || name.hasPrefix("awdl") ||
                name.hasPrefix("llw") || name == "ap1" {
                continue
            }
            if other == nil { other = text }
        }
        return ethernet ?? other
    }
}

enum StemdError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        switch self {
        case .message(let text): text
        }
    }
}

// MARK: - HTTP

struct StemdClient {
    let base: URL
    let token: String
    /// Where the debug screen reads its rows. Optional so the client can be
    /// used without one, and so nothing here fails when there is no UI.
    let log: APILog?

    struct Health: Decodable {
        struct Models: Decodable {
            let present: Bool
            let downloading: Bool
            let file: String?
            let done: UInt64
            let total: UInt64
            let error: String?
        }

        let preset: String
        let maxTrackSecs: Double
        let jobs: Int
        let models: Models
        let tokenRequired: Bool

        enum CodingKeys: String, CodingKey {
            case preset, jobs, models
            case maxTrackSecs = "max_track_secs"
            case tokenRequired = "token_required"
        }
    }

    struct ServerRequest: Decodable, Identifiable {
        let seq: UInt64
        let at: Double
        let method: String
        let path: String
        let status: Int
        let millis: UInt64
        let remote: String
        let loopback: Bool

        var id: UInt64 { seq }
        var date: Date { Date(timeIntervalSince1970: at) }
    }

    struct Progress: Decodable {
        let stage: String
        let completed: UInt32
        let total: UInt32
        let fraction: Float
    }

    struct Stem: Decodable, Identifiable {
        let name: String
        let url: String
        let bytes: UInt64
        let gain: Float
        let contentType: String

        var id: String { name }

        enum CodingKeys: String, CodingKey {
            case name, url, bytes, gain
            case contentType = "content_type"
        }
    }

    struct Job: Decodable, Identifiable {
        let id: String
        let created: UInt64
        let title: String?
        let artist: String?
        let album: String?
        let state: String
        let progress: Progress?
        let seconds: Double?
        let realtime: Double?
        let error: String?
        let stems: [Stem]
        /// Present on a finished job; track length is frames / sample_rate.
        let frames: Int?
        let sampleRate: UInt32?

        enum CodingKeys: String, CodingKey {
            case id, created, title, artist, album, state, progress, seconds, realtime, error, stems, frames
            case sampleRate = "sample_rate"
        }
    }

    func health() async throws -> Health {
        try await decode(request("health"))
    }

    func jobs() async throws -> [Job] {
        try await decode(request("jobs"))
    }

    func job(_ id: String) async throws -> Job {
        try await decode(request("jobs/\(id)"))
    }

    /// What the Rust side recorded, including requests from other clients (the
    /// emulator's guest) and requests that failed the token check.
    func serverRequests() async throws -> [ServerRequest] {
        struct Envelope: Decodable { let requests: [ServerRequest] }
        let (data, _) = try await send(request("server/requests"), logging: false)
        return try JSONDecoder().decode(Envelope.self, from: data).requests
    }

    /// Ask Rust to fetch the preset's weights. `409` means one is already
    /// running, which is not an error for the caller.
    func downloadModels() async throws {
        var request = request("models/download", method: "POST")
        request.setValue("0", forHTTPHeaderField: "Content-Length")
        _ = try await send(request, accepting: [409])
    }

    /// Ask whether a source file already has a finished separation.
    func lookup(sourceHash: String, format: String = "flac") async throws -> Job? {
        let query = [
            URLQueryItem(name: "source_hash", value: sourceHash),
            URLQueryItem(name: "format", value: format),
        ]
        let (data, response) = try await send(
            request("jobs/lookup", query: query),
            accepting: [404]
        )
        if response.statusCode == 404 { return nil }
        return try JSONDecoder().decode(Job.self, from: data)
    }

    /// Upload a file and return the job. If the source hash is already known,
    /// the lookup happens first and the body is never sent on a hit.
    func separate(file: URL, title: String?, sourceHash: String?) async throws -> Job {
        if let sourceHash,
           let cached = try? await lookup(sourceHash: sourceHash),
           cached.state == "done"
        {
            return cached
        }

        var items = [URLQueryItem(name: "filename", value: file.lastPathComponent)]
        if let title, !title.isEmpty {
            items.append(URLQueryItem(name: "title", value: title))
        }
        if let sourceHash {
            items.append(URLQueryItem(name: "source_hash", value: sourceHash))
        }
        var request = request("jobs", method: "POST", query: items)
        request.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
        let (data, _) = try await send(request, fromFile: file)
        return try JSONDecoder().decode(Job.self, from: data)
    }

    /// FNV-1a over the file size (little-endian), then its first and last
    /// 64 KiB. This is the source identity the server and the deck's sidecar
    /// also compute, so the same file finds the same cache entry from any of
    /// them.
    static func sourceHash(of url: URL) -> String? {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        guard let size = try? handle.seekToEnd() else { return nil }

        // The offset basis the deck's C code has always used (see
        // mods/stem/cache.c). It is not the textbook 64-bit basis, but every
        // client has to agree bit for bit, so this matches the deck rather
        // than correcting it: a "better" constant here would be a hash that
        // never matches the one the sidecar sends.
        var hash: UInt64 = 1_469_598_103_934_665_603
        func mix(_ data: Data) {
            for byte in data {
                hash ^= UInt64(byte)
                hash = hash &* 1_099_511_628_211
            }
        }

        var little = size.littleEndian
        withUnsafeBytes(of: &little) { mix(Data($0)) }

        try? handle.seek(toOffset: 0)
        if let head = try? handle.read(upToCount: 65_536), !head.isEmpty {
            mix(head)
        }
        if size > 65_536 {
            try? handle.seek(toOffset: size - 65_536)
            if let tail = try? handle.read(upToCount: 65_536), !tail.isEmpty {
                mix(tail)
            }
        }
        return String(format: "%016llx", hash)
    }

    /// Delete every separation and its stems. Model files are separate.
    func clearCache() async throws {
        var request = request("jobs", method: "DELETE")
        request.setValue("0", forHTTPHeaderField: "Content-Length")
        _ = try await send(request)
    }

    /// Remove one finished job and its stems from the cache.
    func removeJob(_ id: String) async throws {
        var request = request("jobs/\(id)/cache", method: "DELETE")
        request.setValue("0", forHTTPHeaderField: "Content-Length")
        _ = try await send(request)
    }

    /// The URL to hand another device. The token is already in the path from
    /// the server's own job representation.
    func shareURL(for stem: Stem) -> URL? {
        StemdServer.shared.shareBase().flatMap { URL(string: stem.url, relativeTo: $0) }
    }

    private func request(
        _ path: String,
        method: String = "GET",
        query: [URLQueryItem] = []
    ) -> URLRequest {
        var components = URLComponents(url: base, resolvingAgainstBaseURL: false)!
        // `path` can carry slashes, so add it to the path rather than treating
        // it as one component.
        components.path = base.path + "/" + path
        if !query.isEmpty { components.queryItems = query }
        var request = URLRequest(url: components.url!)
        request.httpMethod = method
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    /// Perform a request, recording it for the debug screen.
    ///
    /// `accepting` names non-2xx statuses the caller treats as success; they are
    /// still recorded with their real status.
    private func send(
        _ request: URLRequest,
        fromFile: URL? = nil,
        accepting extra: Set<Int> = [],
        logging: Bool = true
    ) async throws -> (Data, HTTPURLResponse) {
        let started = logging
            ? await log?.begin(request.httpMethod ?? "GET", request.url)
            : nil
        let data: Data
        let response: URLResponse
        do {
            if let fromFile {
                (data, response) = try await URLSession.shared.upload(for: request, fromFile: fromFile)
            } else {
                (data, response) = try await URLSession.shared.data(for: request)
            }
        } catch {
            if logging {
                await log?.finish(started, status: nil, bytes: nil, error: error.localizedDescription)
            }
            throw error
        }

        let http = response as? HTTPURLResponse
        if logging {
            await log?.finish(started, status: http?.statusCode, bytes: data.count, error: nil)
        }
        guard let http, (200..<300).contains(http.statusCode) || extra.contains(http.statusCode) else {
            throw StemdError.message("the server answered \(http?.statusCode ?? 0)")
        }
        return (data, http)
    }

    private func decode<T: Decodable>(_ request: URLRequest) async throws -> T {
        let (data, _) = try await send(request)
        return try JSONDecoder().decode(T.self, from: data)
    }
}

/// Advertises `_stemd._tcp` through the system's Bonjour responder.
///
/// Bonjour is the mDNS mechanism iOS allows without the restricted multicast
/// entitlement; it needs `NSBonjourServices` and the local network permission.
/// The CDJ sidecar's AUTO discovery browses exactly this service type.
final class BonjourAdvertiser: NSObject, NetServiceDelegate {
    private var service: NetService?

    func start(port: UInt16) {
        stop()
        let service = NetService(
            domain: "local.",
            type: "_stemd._tcp.",
            name: "stemd",
            port: Int32(port)
        )
        let txt: [String: Data] = [
            "v": Data("1".utf8),
            "path": Data("/v1".utf8),
        ]
        service.setTXTRecord(NetService.data(fromTXTRecord: txt))
        service.delegate = self
        service.publish()
        self.service = service
    }

    func stop() {
        service?.stop()
        service = nil
    }

    func netServiceDidPublish(_ sender: NetService) {
        NSLog("stemd: advertised %@ %@:%d", sender.name, sender.type, sender.port)
    }

    func netService(_ sender: NetService, didNotPublish errorDict: [String: NSNumber]) {
        NSLog("stemd: Bonjour advertisement failed: %@", errorDict.description)
    }
}
