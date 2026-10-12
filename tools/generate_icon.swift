// Run from the repository root: swift tools/generate_icon.swift
// Reuses the actual patron head from design/cafe.js; no generated artwork dependency.
import AppKit

let source = try String(contentsOfFile: "design/cafe.js", encoding: .utf8)
let head = source.components(separatedBy: "open: S('head.open', [")[1].components(separatedBy: "]),")[0]
let rows = head.split(separator: "\n").compactMap { line -> [Character]? in
    let parts = line.split(separator: "'")
    return parts.count >= 2 ? Array(parts[1]) : nil
}
precondition(rows.count == 13 && rows.allSatisfy { $0.count == 14 })
let palette: [Character: UInt32] = ["K": 0x191c3b, "H": 0x858bef, "h": 0x5354a5,
    "J": 0xc4c5ff, "S": 0xffd6a0, "s": 0xda9a72, "R": 0xee8791]
func color(_ value: UInt32) -> CGColor {
    CGColor(red: CGFloat((value >> 16) & 255) / 255, green: CGFloat((value >> 8) & 255) / 255,
            blue: CGFloat(value & 255) / 255, alpha: 1)
}
let output = "assets/TokenCafe.iconset"
try FileManager.default.createDirectory(atPath: output, withIntermediateDirectories: true)
func render(_ size: Int, _ path: String) throws {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    let ctx = NSGraphicsContext(bitmapImageRep: rep)!.cgContext
    ctx.scaleBy(x: CGFloat(size) / 1024, y: CGFloat(size) / 1024)
    ctx.translateBy(x: 0, y: 1024)
    ctx.scaleBy(x: 1, y: -1)
    let tile = CGPath(roundedRect: CGRect(x: 64, y: 64, width: 896, height: 896),
                      cornerWidth: 202, cornerHeight: 202, transform: nil)
    ctx.saveGState()
    ctx.addPath(tile); ctx.clip()
    let gradient = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [color(0x45467e), color(0x181c3c)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(gradient, start: CGPoint(x: 512, y: 64), end: CGPoint(x: 512, y: 960), options: [])
    ctx.restoreGState()
    ctx.addPath(tile); ctx.setStrokeColor(color(0x666aab)); ctx.setLineWidth(8); ctx.strokePath()
    // Crisp square pixels at every supplied resolution, with a grounded silhouette.
    ctx.setShouldAntialias(false)
    for (row, line) in rows.enumerated() {
        for (column, pixel) in line.enumerated() where pixel != "." {
            ctx.setFillColor(color(0x11152e))
            ctx.fill(CGRect(x: 176 + column * 48, y: 216 + row * 48 + 24, width: 48, height: 48))
        }
    }
    for (row, line) in rows.enumerated() {
        for (column, pixel) in line.enumerated() {
            guard let value = palette[pixel] else { continue }
            ctx.setFillColor(color(value))
            ctx.fill(CGRect(x: 176 + column * 48, y: 216 + row * 48, width: 48, height: 48))
        }
    }
    try rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: path))
}
for base in [16, 32, 128, 256, 512] {
    try render(base, "\(output)/icon_\(base)x\(base).png")
    try render(base * 2, "\(output)/icon_\(base)x\(base)@2x.png")
}
try render(1024, "assets/app-icon.png")
