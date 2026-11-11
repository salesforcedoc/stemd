import Foundation

/// A ring buffer of the HTTP calls the app itself makes.
///
/// The UI polls health and jobs once a second, so this is also the quickest way
/// to see that the app is talking to the embedded server at all. Requests are
/// recorded when they start and completed when they return: a row with no
/// status yet is one still in flight.
@MainActor
final class APILog: ObservableObject {
    struct Call: Identifiable {
        let id: UUID
        let at: Date
        let method: String
        /// Path and query, with the token masked.
        let path: String
        var status: Int?
        var millis: Int?
        var bytes: Int?
        var error: String?
    }

    @Published private(set) var calls: [Call] = []
    /// Paused by default: the polling rows are useful when diagnosing, but they
    /// should not fill the list the moment the screen opens.
    @Published var paused = true

    /// Enough to see a few minutes of polling without the list growing without
    /// bound. The UI's own two requests per second fill it in about three
    /// minutes.
    private let limit = 400

    func begin(_ method: String, _ url: URL?) -> (UUID, Date) {
        let id = UUID()
        let at = Date()
        guard !paused else { return (id, at) }
        calls.append(
            Call(
                id: id,
                at: at,
                method: method,
                path: Self.display(url),
                status: nil,
                millis: nil,
                bytes: nil,
                error: nil
            )
        )
        if calls.count > limit {
            calls.removeFirst(calls.count - limit)
        }
        return (id, at)
    }

    func finish(
        _ started: (UUID, Date)?,
        status: Int?,
        bytes: Int?,
        error: String?
    ) {
        guard let started, !paused,
              let index = calls.firstIndex(where: { $0.id == started.0 })
        else { return }
        calls[index].status = status
        calls[index].millis = Int(Date().timeIntervalSince(started.1) * 1000)
        calls[index].bytes = bytes
        calls[index].error = error
    }

    func clear() {
        calls.removeAll()
    }

    private static func display(_ url: URL?) -> String {
        guard let url else { return "?" }
        var path = url.path
        if let query = url.query, !query.isEmpty {
            let masked = query
                .split(separator: "&")
                .map { pair -> String in
                    pair.hasPrefix("token=") ? "token=…" : String(pair)
                }
                .joined(separator: "&")
            path += "?" + masked
        }
        return path
    }
}
