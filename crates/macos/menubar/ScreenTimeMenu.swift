// Screen time menu-bar item for macOS: time left, "Request more time", "My parent is here",
// and small lower-right warnings that never take focus. Talks JSON lines to screentime-mac
// over its Unix socket (the same protocol as the Linux and Windows clients).
//
// Build: swiftc -O -o ScreenTimeMenu ScreenTimeMenu.swift -framework AppKit

import AppKit

// MARK: - Socket

/// The agent's socket: the system daemon's if installed, otherwise the per-user agent's.
func socketPath() -> String {
    if let p = ProcessInfo.processInfo.environment["SCREENTIME_SOCKET"], !p.isEmpty { return p }
    let system = "/Library/Application Support/screentime/client.sock"
    if FileManager.default.fileExists(atPath: system) { return system }
    return NSHomeDirectory() + "/Library/Application Support/screentime/client.sock"
}

enum SocketError: Error, CustomStringConvertible {
    case failed(String)
    var description: String { if case .failed(let s) = self { return s }; return "" }
}

/// A connection to the agent, reading and writing one JSON object per line.
final class Connection {
    private let fd: Int32
    private var buffer = Data()

    init() throws {
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        if fd < 0 { throw SocketError.failed("socket failed") }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let path = socketPath()
        let maxLen = MemoryLayout.size(ofValue: addr.sun_path)
        guard path.utf8.count < maxLen else { close(fd); throw SocketError.failed("socket path too long") }
        _ = withUnsafeMutablePointer(to: &addr.sun_path) { ptr in
            ptr.withMemoryRebound(to: CChar.self, capacity: maxLen) { strncpy($0, path, maxLen) }
        }
        let ok = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if ok != 0 { close(fd); throw SocketError.failed("Screen time isn't running") }
    }

    deinit { close(fd) }

    func send(_ request: [String: Any]) throws {
        var data = try JSONSerialization.data(withJSONObject: request)
        data.append(0x0a)
        try data.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = write(fd, raw.baseAddress! + off, raw.count - off)
                if n <= 0 { throw SocketError.failed("write failed") }
                off += n
            }
        }
    }

    func readLine() throws -> [String: Any] {
        while true {
            if let nl = buffer.firstIndex(of: 0x0a) {
                let line = buffer[buffer.startIndex..<nl]
                buffer.removeSubrange(buffer.startIndex...nl)
                guard let obj = try JSONSerialization.jsonObject(with: line) as? [String: Any] else {
                    throw SocketError.failed("bad reply")
                }
                return obj
            }
            var chunk = [UInt8](repeating: 0, count: 4096)
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { throw SocketError.failed("Screen time stopped") }
            buffer.append(contentsOf: chunk[0..<n])
        }
    }
}

func ask(_ request: [String: Any]) -> Result<[String: Any], Error> {
    Result {
        let c = try Connection()
        try c.send(request)
        return try c.readLine()
    }
}

let me = NSUserName()

// MARK: - Text

func fmtMinutes(_ m: Int) -> String {
    switch (m / 60, m % 60) {
    case (0, let m): return "\(m) min"
    case (let h, 0): return "\(h) h"
    case (let h, let m): return "\(h) h \(m) min"
    }
}

/// (menu-bar title, status line) for a reply.
func describe(_ reply: [String: Any]) -> (String, String) {
    switch reply["reply"] as? String {
    case "decision":
        let d = reply["decision"] as? [String: Any] ?? [:]
        if d["decision"] as? String == "allowed" {
            if let m = d["remaining_minutes"] as? Int { return (fmtMinutes(m), "\(fmtMinutes(m)) of screen time left") }
            return ("", "No screen time limit right now")
        }
        let reason = d["reason"] as? [String: Any] ?? [:]
        switch reason["kind"] as? String {
        case "locked": return ("Locked", "A parent has locked this computer for now.")
        case "out_of_time": return ("0 min", "Screen time is used up for today.")
        case "downtime":
            var until = ""
            if let s = reason["until"] as? String {
                let f = DateFormatter(); f.dateFormat = "yyyy-MM-dd'T'HH:mm:ss"; f.locale = Locale(identifier: "en_US_POSIX")
                if let d = f.date(from: String(s.prefix(19))) {
                    let o = DateFormatter(); o.timeStyle = .short; until = " until " + o.string(from: d)
                }
            }
            return ("Downtime", "Downtime\(until).")
        default: return ("", "Waiting for screen time settings.")
        }
    case "unmanaged": return ("", "No screen time limits on this account.")
    default: return ("", "")
    }
}

// MARK: - Warning panel

/// A small panel in the lower-right corner: never focused, clicks pass through, shown over
/// full-screen apps on every Space.
final class Toast {
    private let panel: NSPanel
    private let label = NSTextField(labelWithString: "")
    private var hideTimer: Timer?
    private let size = NSSize(width: 300, height: 64)

    init() {
        panel = NSPanel(contentRect: NSRect(origin: .zero, size: size),
                        styleMask: [.nonactivatingPanel, .borderless], backing: .buffered, defer: true)
        panel.level = .screenSaver
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]
        panel.ignoresMouseEvents = true
        panel.hidesOnDeactivate = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        let box = NSVisualEffectView(frame: NSRect(origin: .zero, size: size))
        box.material = .hudWindow
        box.state = .active
        box.wantsLayer = true
        box.layer?.cornerRadius = 12
        box.layer?.masksToBounds = true
        let accent = NSView(frame: NSRect(x: 0, y: 0, width: 5, height: size.height))
        accent.wantsLayer = true
        accent.layer?.backgroundColor = NSColor.systemOrange.cgColor
        box.addSubview(accent)
        label.font = .systemFont(ofSize: 15, weight: .semibold)
        label.textColor = .labelColor
        label.frame = NSRect(x: 20, y: 0, width: size.width - 28, height: size.height)
        label.cell = VerticallyCentered(textCell: "")
        label.font = .systemFont(ofSize: 15, weight: .semibold)
        box.addSubview(label)
        panel.contentView = box
    }

    func show(_ text: String, seconds: TimeInterval = 10) {
        label.stringValue = text
        let screen = NSScreen.screens.first(where: { $0.frame.contains(NSEvent.mouseLocation) }) ?? NSScreen.main
        if let area = screen?.visibleFrame {
            panel.setFrameOrigin(NSPoint(x: area.maxX - size.width - 16, y: area.minY + 16))
        }
        panel.orderFrontRegardless()
        hideTimer?.invalidate()
        hideTimer = Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { [weak self] _ in
            self?.panel.orderOut(nil)
        }
    }
}

final class VerticallyCentered: NSTextFieldCell {
    override func drawingRect(forBounds rect: NSRect) -> NSRect {
        var r = super.drawingRect(forBounds: rect)
        let h = cellSize(forBounds: rect).height
        r.origin.y += (r.height - h) / 2
        r.size.height = h
        return r
    }
}

// MARK: - App

final class App: NSObject, NSApplicationDelegate {
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
    private let statusLine = NSMenuItem(title: "Screen time", action: nil, keyEquivalent: "")
    private let toast = Toast()

    func applicationDidFinishLaunching(_ note: Notification) {
        if let button = item.button {
            button.image = NSImage(systemSymbolName: "hourglass", accessibilityDescription: "Screen time")
            button.imagePosition = .imageLeading
        }
        let menu = NSMenu()
        statusLine.isEnabled = false
        menu.addItem(statusLine)
        menu.addItem(.separator())
        menu.addItem(withTitle: "Request more time", action: #selector(requestTime), keyEquivalent: "").target = self
        menu.addItem(withTitle: "My parent is here…", action: #selector(parentIsHere), keyEquivalent: "").target = self
        item.menu = menu
        refresh()
        Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { [weak self] _ in self?.refresh() }
        Thread.detachNewThread { [weak self] in self?.followWarnings() }
        // `--preview` shows what a warning looks like.
        if CommandLine.arguments.contains("--preview") {
            toast.show("10 minutes of screen time left")
        }
    }

    private func setStatus(_ title: String, _ line: String) {
        item.button?.title = title.isEmpty ? "" : " " + title
        item.button?.toolTip = line
        statusLine.title = line.isEmpty ? "Screen time" : line
    }

    private func refresh() {
        DispatchQueue.global().async {
            let r = ask(["op": "status", "username": me])
            DispatchQueue.main.async {
                switch r {
                case .success(let reply): let (t, l) = describe(reply); self.setStatus(t, l)
                case .failure(let e): self.setStatus("", "Screen time unavailable (\(e))")
                }
            }
        }
    }

    /// Follow the agent's warnings, reconnecting if it restarts.
    private func followWarnings() {
        while true {
            do {
                let c = try Connection()
                try c.send(["op": "subscribe", "username": me])
                while true {
                    let reply = try c.readLine()
                    if reply["reply"] as? String == "warning", let m = reply["remaining_minutes"] as? Int {
                        DispatchQueue.main.async {
                            self.toast.show(m == 1 ? "1 minute of screen time left" : "\(m) minutes of screen time left")
                            self.refresh()
                        }
                    }
                }
            } catch {}
            Thread.sleep(forTimeInterval: 5)
        }
    }

    private func tell(_ title: String, _ text: String) {
        toast.show("\(title): \(text)", seconds: 6)
    }

    private func report(_ r: Result<[String: Any], Error>) {
        DispatchQueue.main.async {
            switch r {
            case .success(let reply):
                switch reply["reply"] as? String {
                case "request_sent": self.tell("Request sent", "your parent will get a notification.")
                case "request_queued": self.tell("Request saved", "it goes out when the server is reachable.")
                case "granted": self.tell("Time added", "\(reply["minutes"] as? Int ?? 0) more minutes.")
                case "error": self.tell("Couldn't do that", reply["message"] as? String ?? "")
                case "unmanaged": self.tell("Screen time", "no limits on this account.")
                default: break
                }
            case .failure(let e): self.tell("Screen time", "\(e)")
            }
            self.refresh()
        }
    }

    @objc private func requestTime() {
        DispatchQueue.global().async { self.report(ask(["op": "request_time", "username": me])) }
    }

    @objc private func parentIsHere() {
        // Which account approves: the one used last, else the first administrator not limited.
        var admin = "parent"
        if case .success(let reply) = ask(["op": "list_admins"]) {
            let admins = reply["admins"] as? [String] ?? []
            admin = (reply["last"] as? String) ?? admins.first ?? admin
        }
        let alert = NSAlert()
        alert.messageText = "My parent is here"
        alert.informativeText = "A parent adds time with the Screen time password (the one for the web page)."
        let form = NSStackView(frame: NSRect(x: 0, y: 0, width: 260, height: 56))
        form.orientation = .vertical
        form.alignment = .leading
        let minutes = NSPopUpButton(frame: .zero, pullsDown: false)
        let choices = [15, 30, 45, 60, 90, 120]
        minutes.addItems(withTitles: choices.map { "Add \(fmtMinutes($0))" })
        minutes.selectItem(at: 1)
        let password = NSSecureTextField(frame: NSRect(x: 0, y: 0, width: 260, height: 24))
        password.placeholderString = "Screen time password"
        form.addArrangedSubview(minutes)
        form.addArrangedSubview(password)
        alert.accessoryView = form
        alert.addButton(withTitle: "Add time")
        alert.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        alert.window.initialFirstResponder = password
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let request: [String: Any] = ["op": "parent_grant", "username": me,
                                      "minutes": choices[minutes.indexOfSelectedItem],
                                      "admin": admin, "password": password.stringValue]
        DispatchQueue.global().async { self.report(ask(request)) }
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = App()
app.delegate = delegate
app.run()
