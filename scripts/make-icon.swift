// Renders the Kipimo icon: a gauge. A graphite squircle (the family material of
// Photon and Tamka), a ring of white measurement dots, and a mint needle resting
// low on the dial, where a healthy Mac sits.
// Usage: swift scripts/make-icon.swift   → assets/AppIcon.icns, assets/logo.png
import AppKit

let mint = NSColor(srgbRed: 0.42, green: 0.92, blue: 0.70, alpha: 1)

func draw(_ px: CGFloat) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(px), pixelsHigh: Int(px), bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                               bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let ctx = NSGraphicsContext.current!.cgContext
    ctx.scaleBy(x: px / 1024, y: px / 1024)
    let srgb = CGColorSpace(name: CGColorSpace.sRGB)!

    // Squircle body on the macOS icon grid (824pt body centred in 1024).
    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    let path = CGPath(roundedRect: body, cornerWidth: 186, cornerHeight: 186, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 28, color: NSColor(white: 0, alpha: 0.35).cgColor)
    ctx.addPath(path); ctx.setFillColor(NSColor(srgbRed: 0.08, green: 0.08, blue: 0.10, alpha: 1).cgColor); ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(path); ctx.clip()
    let bg = CGGradient(colorsSpace: srgb, colors: [
        NSColor(srgbRed: 0.17, green: 0.17, blue: 0.21, alpha: 1).cgColor,
        NSColor(srgbRed: 0.06, green: 0.06, blue: 0.08, alpha: 1).cgColor] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(bg, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])

    // The dial: a 240° arc open at the bottom, marked with dots.
    let c = CGPoint(x: 512, y: 470)
    let r: CGFloat = 280
    let start: CGFloat = 210, sweep: CGFloat = 240
    func point(_ deg: CGFloat, _ radius: CGFloat) -> CGPoint {
        let a = deg * .pi / 180
        return CGPoint(x: c.x + cos(a) * radius, y: c.y + sin(a) * radius)
    }
    let steps = 12
    for i in 0...steps {
        let deg = start - sweep * CGFloat(i) / CGFloat(steps)
        let major = i % 3 == 0
        let d: CGFloat = major ? 40 : 24
        let p = point(deg, r)
        ctx.setFillColor(NSColor(white: 1, alpha: major ? 0.95 : 0.55).cgColor)
        ctx.fillEllipse(in: CGRect(x: p.x - d / 2, y: p.y - d / 2, width: d, height: d))
    }

    // The needle, a third of the way round, with a soft glow.
    let needleDeg = start - sweep * 0.33
    let tip = point(needleDeg, 210)
    let glow = CGGradient(colorsSpace: srgb, colors: [
        mint.withAlphaComponent(0.40).cgColor, mint.withAlphaComponent(0).cgColor] as CFArray, locations: [0, 1])!
    ctx.drawRadialGradient(glow, startCenter: c, startRadius: 0, endCenter: c, endRadius: 300, options: [])
    let a = needleDeg * .pi / 180
    let n = CGPoint(x: -sin(a), y: cos(a))  // normal to the needle
    let base: CGFloat = 30
    ctx.beginPath()
    ctx.move(to: CGPoint(x: c.x + n.x * base, y: c.y + n.y * base))
    ctx.addLine(to: tip)
    ctx.addLine(to: CGPoint(x: c.x - n.x * base, y: c.y - n.y * base))
    ctx.closePath()
    ctx.setFillColor(mint.cgColor)
    ctx.fillPath()
    ctx.setFillColor(mint.cgColor)
    ctx.fillEllipse(in: CGRect(x: c.x - 58, y: c.y - 58, width: 116, height: 116))
    ctx.setFillColor(NSColor(srgbRed: 0.10, green: 0.10, blue: 0.13, alpha: 1).cgColor)
    ctx.fillEllipse(in: CGRect(x: c.x - 24, y: c.y - 24, width: 48, height: 48))

    ctx.restoreGState()
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let set = URL(fileURLWithPath: "assets/AppIcon.iconset")
try? FileManager.default.removeItem(at: set)
try! FileManager.default.createDirectory(at: set, withIntermediateDirectories: true)
for (pt, scale) in [(16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2), (256, 1), (256, 2), (512, 1), (512, 2)] {
    let name = scale == 1 ? "icon_\(pt)x\(pt).png" : "icon_\(pt)x\(pt)@2x.png"
    let data = draw(CGFloat(pt * scale)).representation(using: .png, properties: [:])!
    try! data.write(to: set.appendingPathComponent(name))
}
try! draw(512).representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: "assets/logo.png"))
let p = Process(); p.launchPath = "/usr/bin/iconutil"; p.arguments = ["-c", "icns", set.path, "-o", "assets/AppIcon.icns"]
p.launch(); p.waitUntilExit()
// The iconset is an intermediate build product of this script.
try? FileManager.default.removeItem(at: set)
print("assets/AppIcon.icns, assets/logo.png")
