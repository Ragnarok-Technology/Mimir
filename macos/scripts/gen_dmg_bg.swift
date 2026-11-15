#!/usr/bin/env swift
import AppKit
import CoreGraphics
import Foundation

let width = 1360
let height = 880
let outPath = CommandLine.arguments.dropFirst().first ?? "assets/brand/dmg-background.png"

let rep = NSBitmapImageRep(
    bitmapDataPlanes: nil,
    pixelsWide: width,
    pixelsHigh: height,
    bitsPerSample: 8,
    samplesPerPixel: 4,
    hasAlpha: true,
    isPlanar: false,
    colorSpaceName: .deviceRGB,
    bytesPerRow: 0,
    bitsPerPixel: 0
)!
rep.size = NSSize(width: 680, height: 440) // 144 DPI (@2x Retina)

NSGraphicsContext.saveGraphicsState()
let nsCtx = NSGraphicsContext(bitmapImageRep: rep)!
NSGraphicsContext.current = nsCtx
let ctx = nsCtx.cgContext

// Flip to top-left origin in 680x440 logical coordinates
ctx.translateBy(x: 0, y: 440)
ctx.scaleBy(x: 1, y: -1)

func color(_ hex: UInt32, _ alpha: CGFloat = 1.0) -> CGColor {
    let r = CGFloat((hex >> 16) & 0xFF) / 255.0
    let g = CGFloat((hex >> 8) & 0xFF) / 255.0
    let b = CGFloat(hex & 0xFF) / 255.0
    return CGColor(srgbRed: r, green: g, blue: b, alpha: alpha)
}

// 1. Base void #06080D
ctx.setFillColor(color(0x06080D))
ctx.fill(CGRect(x: 0, y: 0, width: 680, height: 440))

// 2. Subtle top-center radial refraction glow
let glowColors = [color(0x0EA5E9, 0.16), color(0x2563EB, 0.06), color(0x06080D, 0.0)] as CFArray
if let grad = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: glowColors, locations: [0.0, 0.45, 1.0]) {
    ctx.drawRadialGradient(
        grad,
        startCenter: CGPoint(x: 340, y: 80),
        startRadius: 0,
        endCenter: CGPoint(x: 340, y: 80),
        endRadius: 340,
        options: [.drawsAfterEndLocation]
    )
}

// 3. Subtle dot grid texture
ctx.setFillColor(color(0x182436, 0.45))
for gx in stride(from: 20, to: 670, by: 24) {
    for gy in stride(from: 20, to: 430, by: 24) {
        ctx.fillEllipse(in: CGRect(x: CGFloat(gx) - 0.6, y: CGFloat(gy) - 0.6, width: 1.2, height: 1.2))
    }
}

// 4. Header: small Refracted M mark + M I M I R + subtitle
func drawMimirMark(x: CGFloat, y: CGFloat, size: CGFloat) {
    let s = size / 512.0
    func poly(_ pts: [(CGFloat, CGFloat)], _ hex: UInt32, _ a: CGFloat = 1.0) {
        ctx.beginPath()
        ctx.move(to: CGPoint(x: x + pts[0].0 * s, y: y + pts[0].1 * s))
        for p in pts.dropFirst() {
            ctx.addLine(to: CGPoint(x: x + p.0 * s, y: y + p.1 * s))
        }
        ctx.closePath()
        ctx.setFillColor(color(hex, a))
        ctx.fillPath()
    }
    poly([(92, 92), (156, 92), (256, 192), (356, 92), (420, 92), (420, 104), (256, 268), (92, 104)], 0x67E8F9)
    poly([(92, 125.21), (156, 189.21), (156, 344.23), (92, 394.15)], 0x38BDF8)
    poly([(356, 189.21), (420, 125.21), (420, 394.15), (356, 344.23)], 0x0EA5E9)
    poly([(92, 413.17), (156, 363.25), (156, 420), (92, 420)], 0x2563EB, 0.9)
    poly([(356, 363.25), (420, 413.17), (420, 420), (356, 420)], 0x2563EB, 0.9)
}
drawMimirMark(x: 282, y: 40, size: 22)

// Flip back briefly for CoreText / NSAttributedString text drawing
func drawText(_ str: String, x: CGFloat, y: CGFloat, font: NSFont, hex: UInt32, alpha: CGFloat = 1.0, centered: Bool = true) {
    ctx.saveGState()
    ctx.translateBy(x: 0, y: 440)
    ctx.scaleBy(x: 1, y: -1)
    let flippedY = 440 - y
    let attrs: [NSAttributedString.Key: Any] = [
        .font: font,
        .foregroundColor: NSColor(cgColor: color(hex, alpha))!,
    ]
    let attrStr = NSAttributedString(string: str, attributes: attrs)
    let sz = attrStr.size()
    let drawX = centered ? x - sz.width / 2.0 : x
    attrStr.draw(at: NSPoint(x: drawX, y: flippedY - sz.height / 2.0))
    ctx.restoreGState()
}

drawText("M I M I R", x: 356, y: 51, font: NSFont.systemFont(ofSize: 15, weight: .bold), hex: 0xE2E8F0)
drawText("100% LOCAL  ·  WHISTLE AI  ·  ZERO TELEMETRY", x: 340, y: 78, font: NSFont.monospacedSystemFont(ofSize: 9.5, weight: .medium), hex: 0x64748B)

// 5. Two glass pedestals at (190, 222) and (490, 222)
func drawPedestal(cx: CGFloat, cy: CGFloat, w: CGFloat, h: CGFloat, highlight: UInt32) {
    let rect = CGRect(x: cx - w / 2, y: cy - h / 2, width: w, height: h)
    let path = CGPath(roundedRect: rect, cornerWidth: 18, cornerHeight: 18, transform: nil)
    ctx.addPath(path)
    ctx.setFillColor(color(0x0B101A, 0.92))
    ctx.fillPath()
    ctx.addPath(path)
    ctx.setStrokeColor(color(highlight, 0.35))
    ctx.setLineWidth(1.2)
    ctx.strokePath()
}
drawPedestal(cx: 190, cy: 222, w: 158, h: 158, highlight: 0x38BDF8)
drawPedestal(cx: 490, cy: 222, w: 158, h: 158, highlight: 0x334155)

// 6. Center directional prism arrow (x: 304..376, y: 222)
ctx.setStrokeColor(color(0x38BDF8, 0.85))
ctx.setLineWidth(2.2)
ctx.setLineCap(.round)
ctx.setLineJoin(.round)
ctx.beginPath()
ctx.move(to: CGPoint(x: 304, y: 222))
ctx.addLine(to: CGPoint(x: 374, y: 222))
ctx.strokePath()

ctx.beginPath()
ctx.move(to: CGPoint(x: 360, y: 209))
ctx.addLine(to: CGPoint(x: 375, y: 222))
ctx.addLine(to: CGPoint(x: 360, y: 235))
ctx.strokePath()

drawText("DRAG TO INSTALL", x: 340, y: 254, font: NSFont.monospacedSystemFont(ofSize: 9.5, weight: .semibold), hex: 0x67E8F9, alpha: 0.85)

// 7. Footer hint
drawText("Open Mimir after copying to Applications  ·  Unlicense", x: 340, y: 404, font: NSFont.systemFont(ofSize: 10.5, weight: .regular), hex: 0x64748B)

NSGraphicsContext.restoreGraphicsState()

guard let png = rep.representation(using: .png, properties: [:]) else {
    fputs("Failed to encode PNG\n", stderr)
    exit(1)
}
try png.write(to: URL(fileURLWithPath: outPath))
print("✓ Wrote \(outPath) (\(width)x\(height) @2x)")
