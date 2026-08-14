import CoreGraphics
import Foundation

guard CommandLine.arguments.count == 2,
      let requestedPID = Int32(CommandLine.arguments[1]) else {
    FileHandle.standardError.write(Data("usage: window-probe <pid>\n".utf8))
    exit(2)
}

let options: CGWindowListOption = [.optionAll, .excludeDesktopElements]
guard let windows = CGWindowListCopyWindowInfo(options, kCGNullWindowID)
        as? [[String: Any]] else {
    exit(1)
}

for window in windows {
    guard let ownerPID = (window[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value,
          ownerPID == requestedPID,
          let layer = (window[kCGWindowLayer as String] as? NSNumber)?.intValue,
          layer == 0,
          let alpha = (window[kCGWindowAlpha as String] as? NSNumber)?.doubleValue,
          alpha > 0,
          let bounds = window[kCGWindowBounds as String] as? [String: Any],
          let width = (bounds["Width"] as? NSNumber)?.doubleValue,
          let height = (bounds["Height"] as? NSNumber)?.doubleValue,
          width >= 100,
          height >= 100 else {
        continue
    }

    let title = window[kCGWindowName as String] as? String ?? ""
    print(title)
    exit(0)
}

exit(1)
