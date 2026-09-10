import Foundation
import Network

/// Finds the paired desktop's embedded relay on the local network via
/// Bonjour (`_pendant._tcp`, matched on the `id` TXT record) and resolves
/// it to a `ws://host:port/ws` URL. Only works where multicast reaches
/// (same Wi-Fi); over VPN/overlay networks like Tailscale the stored
/// addresses from the pairing QR do the job instead.
@MainActor
final class RelayDiscovery {
    static let serviceType = "_pendant._tcp"

    let relayId: String
    /// Direct URL of the matched desktop; nil while not found.
    private(set) var url: String? {
        didSet { if url != oldValue { onChange?() } }
    }
    var onChange: (() -> Void)?

    private var browser: NWBrowser?
    private var resolver: NWConnection?

    init(relayId: String) {
        self.relayId = relayId
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
            NSLog("pendant: relay browser failed: \(error); restarting")
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
        url = nil
    }

    private func handle(_ results: Set<NWBrowser.Result>) {
        let match = results.first { result in
            guard case .bonjour(let txt) = result.metadata else { return false }
            return txt.dictionary["id"] == relayId
        }
        guard let match else {
            resolver?.cancel()
            resolver = nil
            url = nil
            return
        }
        resolve(match.endpoint, path: txtPath(match))
    }

    private func txtPath(_ result: NWBrowser.Result) -> String {
        if case .bonjour(let txt) = result.metadata, let path = txt.dictionary["path"] {
            return path
        }
        return "/ws"
    }

    /// Bonjour hands back a service endpoint, not an address; a throwaway
    /// TCP connection resolves it (IPv4 forced so the URL stays simple).
    private func resolve(_ endpoint: NWEndpoint, path: String) {
        resolver?.cancel()
        let params = NWParameters.tcp
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
                    self?.url = "ws://\(hostText):\(port)\(path)"
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
