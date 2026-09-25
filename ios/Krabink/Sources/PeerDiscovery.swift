import Foundation
import Network

/// Finds the paired desktop's node on the local network via Bonjour
/// (`_krabink._udp`, matched on the `id` TXT record) and hands the core
/// every `ip:port` it can dial: the TXT record's `addrs` + `port` (every
/// interface the desktop advertises, overlays like Tailscale included,
/// which matters when a firewall on the desktop admits only the overlay)
/// plus the address Bonjour itself resolves. Only matters when the relay
/// is unreachable: with the relay up, the nodes exchange addresses through
/// it and punch the LAN path themselves. Multicast does not cross
/// VPN/overlay networks; there the QR's addresses and the relay do the job.
@MainActor
final class PeerDiscovery {
    static let serviceType = "_krabink._udp"

    let nodeId: String
    /// `ip:port` Bonjour resolved for the matched desktop; nil while not
    /// found. Shown in Settings.
    private(set) var addr: String? {
        didSet { if addr != oldValue, let addr { hint(addr) } }
    }
    /// Every address handed to `onFound` so far; reset on `stop()`.
    private var hinted: Set<String> = []
    /// Called once per new `ip:port`.
    var onFound: ((String) -> Void)?

    private var browser: NWBrowser?
    private var resolver: NWConnection?

    init(nodeId: String) {
        self.nodeId = nodeId
    }

    func start() {
        guard browser == nil else { return }
        let params = NWParameters()
        params.includePeerToPeer = true
        let browser = NWBrowser(
            for: .bonjourWithTXTRecord(type: Self.serviceType, domain: nil), using: params)
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            Task { @MainActor [weak self] in self?.handle(results) }
        }
        browser.stateUpdateHandler = { [weak self] state in
            guard case .failed(let error) = state else { return }
            NSLog("krabink: peer browser failed: \(error); restarting")
            Task { @MainActor [weak self] in
                self?.stop()
                self?.start()
            }
        }
        browser.start(queue: .global(qos: .utility))
        self.browser = browser
    }

    func stop() {
        browser?.cancel()
        browser = nil
        resolver?.cancel()
        resolver = nil
        addr = nil
        hinted = []
    }

    private func hint(_ addr: String) {
        guard hinted.insert(addr).inserted else { return }
        onFound?(addr)
    }

    /// `ip:port` pairs from the TXT record (`addrs=ip,ip…`, `port=n`).
    static func txtAddrs(_ txt: [String: String]) -> [String] {
        guard let port = txt["port"], UInt16(port) != nil else { return [] }
        return (txt["addrs"] ?? "")
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespaces) }
            .filter { !$0.isEmpty }
            .map { "\($0):\(port)" }
    }

    private func handle(_ results: Set<NWBrowser.Result>) {
        let match = results.first { result in
            guard case .bonjour(let txt) = result.metadata else { return false }
            return txt.dictionary["id"] == nodeId
        }
        guard let match else {
            resolver?.cancel()
            resolver = nil
            addr = nil
            return
        }
        if case .bonjour(let txt) = match.metadata {
            for addr in Self.txtAddrs(txt.dictionary) { hint(addr) }
        }
        resolve(match.endpoint)
    }

    /// Bonjour hands back a service endpoint, not an address. A UDP
    /// connection resolves it without any handshake: it is `.ready` as
    /// soon as the path is known (IPv4 forced so the address stays simple).
    private func resolve(_ endpoint: NWEndpoint) {
        resolver?.cancel()
        let params = NWParameters.udp
        if let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let connection = NWConnection(to: endpoint, using: params)
        connection.stateUpdateHandler = { [weak self, weak connection] state in
            guard let connection else { return }
            switch state {
            case .ready:
                let resolved = connection.currentPath?.remoteEndpoint
                connection.cancel()
                guard case .hostPort(let host, let port)? = resolved else { return }
                let hostText: String
                switch host {
                case .ipv4(let address): hostText = "\(address)"
                case .ipv6(let address): hostText = "[\(address)]"
                case .name(let name, _): hostText = name
                @unknown default: hostText = "\(host)"
                }
                Task { @MainActor [weak self] in
                    self?.addr = "\(hostText):\(port)"
                }
            case .failed, .cancelled:
                connection.cancel()
            default:
                break
            }
        }
        connection.start(queue: .global(qos: .utility))
        resolver = connection
    }
}
