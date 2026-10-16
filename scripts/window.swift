// Helper for scripts/screenshot.sh.
//   swift scripts/window.swift find <pid>   prints "<window id> <x> <y> <width> <height>"
//   swift scripts/window.swift park <x> <y> moves the pointer, so no row shows a hover
import CoreGraphics
import Foundation

let args = CommandLine.arguments
switch args.count > 1 ? args[1] : "" {
case "find":
    let pid = Int(args[2])!
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
    for w in list where (w[kCGWindowOwnerPID as String] as? Int) == pid && (w[kCGWindowLayer as String] as? Int) == 0 {
        let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
        print(w[kCGWindowNumber as String]!, Int(b["X"] ?? 0), Int(b["Y"] ?? 0), Int(b["Width"] ?? 0), Int(b["Height"] ?? 0))
        exit(0)
    }
    exit(1)
case "park":
    let point = CGPoint(x: Double(args[2])!, y: Double(args[3])!)
    CGEvent(mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: point, mouseButton: .left)?
        .post(tap: .cghidEventTap)
default:
    FileHandle.standardError.write("usage: window.swift find <pid> | park <x> <y>\n".data(using: .utf8)!)
    exit(2)
}
