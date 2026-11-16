// Draws the PsConnector app icon: a dark rounded tile, a blue rounded play mark and two signal arcs.
// usage: swift appicon.swift out.png [size]
import AppKit
import CoreGraphics

let args = CommandLine.arguments
let out = args[1]
let S = CGFloat(Double(args.count > 2 ? args[2] : "1024") ?? 1024)
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: Int(S), height: Int(S), bitsPerComponent: 8, bytesPerRow: 0,
                    space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
func c(_ r: Int, _ g: Int, _ b: Int, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat(r) / 255, green: CGFloat(g) / 255, blue: CGFloat(b) / 255, alpha: a)
}
let u = S / 1024

// tile: macOS icon grid, 824 px body centred in 1024 with ~185 px corners
let inset = 100 * u
let tile = CGRect(x: inset, y: inset, width: S - 2 * inset, height: S - 2 * inset)
let tilePath = CGPath(roundedRect: tile, cornerWidth: 185 * u, cornerHeight: 185 * u, transform: nil)
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10 * u), blur: 28 * u, color: c(0, 0, 0, 0.35))
ctx.addPath(tilePath)
ctx.setFillColor(c(0x16, 0x18, 0x1D))
ctx.fillPath()
ctx.restoreGState()
ctx.saveGState()
ctx.addPath(tilePath)
ctx.clip()
let grad = CGGradient(colorsSpace: cs, colors: [c(0x23, 0x27, 0x30), c(0x0E, 0x0F, 0x12)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(grad, start: CGPoint(x: S / 2, y: tile.maxY), end: CGPoint(x: S / 2, y: tile.minY), options: [])
ctx.restoreGState()
ctx.addPath(tilePath)
ctx.setStrokeColor(c(255, 255, 255, 0.08))
ctx.setLineWidth(3 * u)
ctx.strokePath()

// play mark: rounded triangle, slightly left of centre to leave room for the arcs
let cx = 452 * u, cy = 512 * u, h = 300 * u
let p1 = CGPoint(x: cx - h * 0.42, y: cy + h / 2)
let p2 = CGPoint(x: cx - h * 0.42, y: cy - h / 2)
let p3 = CGPoint(x: cx + h * 0.48, y: cy)
let tri = CGMutablePath()
tri.move(to: CGPoint(x: (p1.x + p2.x) / 2, y: (p1.y + p2.y) / 2))
tri.addArc(tangent1End: p2, tangent2End: p3, radius: 46 * u)
tri.addArc(tangent1End: p3, tangent2End: p1, radius: 46 * u)
tri.addArc(tangent1End: p1, tangent2End: p2, radius: 46 * u)
tri.closeSubpath()
ctx.saveGState()
ctx.addPath(tri)
ctx.clip()
let blue = CGGradient(colorsSpace: cs, colors: [c(0x5B, 0x92, 0xF5), c(0x2F, 0x6F, 0xE4)] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(blue, start: CGPoint(x: cx, y: cy + h / 2), end: CGPoint(x: cx, y: cy - h / 2), options: [])
ctx.restoreGState()

// signal arcs to the right of the tip
ctx.setLineCap(.round)
for (i, r) in [CGFloat(110), CGFloat(185)].enumerated() {
    ctx.setStrokeColor(c(0xA9, 0xC4, 0xF8, i == 0 ? 0.95 : 0.6))
    ctx.setLineWidth(34 * u)
    ctx.addArc(center: CGPoint(x: p3.x - 40 * u, y: cy), radius: r * u, startAngle: -0.62, endAngle: 0.62, clockwise: false)
    ctx.strokePath()
}

let img = ctx.makeImage()!
let rep = NSBitmapImageRep(cgImage: img)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
