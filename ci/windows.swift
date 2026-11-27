// Prints the windows on screen, front first: layer, owner pid, owner name, size and title (titles need a permission CI lacks).
import CoreGraphics

let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
for w in list {
    let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
    let size = "\(b["Width"] ?? "?")x\(b["Height"] ?? "?")"
    print(w[kCGWindowLayer as String] ?? "?", w[kCGWindowOwnerPID as String] ?? "?", w[kCGWindowOwnerName as String] ?? "?", size, w[kCGWindowName as String] ?? "")
}
