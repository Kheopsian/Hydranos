//go:build windows

package wintray

import (
	"bytes"
	"image"
	"image/png"
	"math"
	"unsafe"

	"golang.org/x/sys/windows"
)

// alphaFloor is the alpha below which a pixel is treated as absent when
// measuring the logo. The artwork carries an antialiasing fringe that peaks at
// 48/255 -- invisible on screen, but enough to push the bounding box a pixel
// wide on each side and waste that pixel at icon size.
const alphaFloor = 64 << 8 // RGBA() reports 16-bit channels

// iconFromPNG decodes the logo and builds an HICON at the shell's small-icon
// size. Decoding at runtime (rather than shipping a .ico) keeps a single
// source image in the repo and lets the icon follow the user's DPI, which a
// fixed 16x16 resource cannot do. Returns 0 on any failure; the caller falls
// back to the stock icon.
func iconFromPNG(data []byte) windows.Handle {
	if len(data) == 0 {
		return 0
	}
	src, err := png.Decode(bytes.NewReader(data))
	if err != nil {
		return 0
	}

	w := metric(smCXSmallIcon)
	h := metric(smCYSmallIcon)
	if w <= 0 || h <= 0 {
		w, h = 16, 16
	}

	// Top-down 32bpp DIB: negative height, so row 0 is the top row and the
	// pixel order matches the image we sample.
	bi := bitmapInfoHeader{
		Size:     uint32(unsafe.Sizeof(bitmapInfoHeader{})),
		Width:    int32(w),
		Height:   int32(-h),
		Planes:   1,
		BitCount: 32,
	}
	var bits unsafe.Pointer
	hbm, _, _ := procCreateDIBSection.Call(
		0, uintptr(unsafe.Pointer(&bi)), 0,
		uintptr(unsafe.Pointer(&bits)), 0, 0,
	)
	if hbm == 0 || bits == nil {
		return 0
	}

	// Cover rather than contain: scale until the glyph fills the square on
	// both axes and let the wider one spill past the edges. The logo is
	// 214x184 once cropped, so containing it leaves a dead row above and
	// below; covering trims 7% off each side, which costs only 3% of the ink
	// because all that lives out there are the tips of the outer two snouts.
	box := opaqueBounds(src)
	scale := math.Max(float64(w)/float64(box.Dx()), float64(h)/float64(box.Dy()))
	offX := (float64(box.Dx())*scale - float64(w)) / 2
	offY := (float64(box.Dy())*scale - float64(h)) / 2

	px := unsafe.Slice((*byte)(bits), w*h*4)
	for y := 0; y < h; y++ {
		for x := 0; x < w; x++ {
			x0 := float64(box.Min.X) + (float64(x)+offX)/scale
			x1 := float64(box.Min.X) + (float64(x+1)+offX)/scale
			y0 := float64(box.Min.Y) + (float64(y)+offY)/scale
			y1 := float64(box.Min.Y) + (float64(y+1)+offY)/scale
			r, g, bl, a := averageRGBA(src, box, x0, y0, x1, y1)
			// The averages are alpha-premultiplied, which is exactly what a
			// 32bpp DIB icon wants. Order on disk is BGRA.
			i := (y*w + x) * 4
			px[i+0] = bl
			px[i+1] = g
			px[i+2] = r
			px[i+3] = a
		}
	}

	// A 1bpp all-zero mask: every pixel opaque, so the color bitmap's own
	// alpha channel decides transparency.
	hmask, _, _ := procCreateBitmap.Call(uintptr(w), uintptr(h), 1, 1, 0)
	if hmask == 0 {
		procDeleteObject.Call(hbm)
		return 0
	}

	ii := iconInfo{FIcon: 1, HbmMask: windows.Handle(hmask), HbmColor: windows.Handle(hbm)}
	hicon, _, _ := procCreateIconIndirect.Call(uintptr(unsafe.Pointer(&ii)))

	// CreateIconIndirect copies the bitmaps; ours are ours to free.
	procDeleteObject.Call(hmask)
	procDeleteObject.Call(hbm)

	return windows.Handle(hicon)
}

// opaqueBounds returns the tightest rectangle holding the logo itself, ignoring
// both the transparent padding around it and the antialiasing fringe at its
// edge. Sampling the raw canvas instead would carry that padding into the icon,
// where it lands the glyph off-centre and a little small.
func opaqueBounds(src image.Image) image.Rectangle {
	b := src.Bounds()
	min := image.Point{X: b.Max.X, Y: b.Max.Y}
	max := image.Point{X: b.Min.X - 1, Y: b.Min.Y - 1}
	for y := b.Min.Y; y < b.Max.Y; y++ {
		for x := b.Min.X; x < b.Max.X; x++ {
			if _, _, _, a := src.At(x, y).RGBA(); a > alphaFloor {
				if x < min.X {
					min.X = x
				}
				if y < min.Y {
					min.Y = y
				}
				if x > max.X {
					max.X = x
				}
				if y > max.Y {
					max.Y = y
				}
			}
		}
	}
	if max.X < min.X || max.Y < min.Y {
		return b // nothing opaque enough: use the canvas and let it look wrong
	}
	return image.Rect(min.X, min.Y, max.X+1, max.Y+1)
}

// averageRGBA averages every source pixel behind one icon pixel, clipped to the
// crop. Picking a single sample instead is what made the icon read as speckle:
// a 214px illustration reduced to 16px by nearest-neighbour lands on isolated
// pixels, so only 38% of the icon carried ink where the neighbouring tray icons
// carry 69%. Averaging keeps the strokes joined up and lifts that to 68%.
func averageRGBA(src image.Image, clip image.Rectangle, x0, y0, x1, y1 float64) (r, g, b, a byte) {
	ix0, iy0 := int(math.Floor(x0)), int(math.Floor(y0))
	ix1, iy1 := int(math.Floor(x1)), int(math.Floor(y1))
	if ix1 <= ix0 {
		ix1 = ix0 + 1
	}
	if iy1 <= iy0 {
		iy1 = iy0 + 1
	}
	if ix0 < clip.Min.X {
		ix0 = clip.Min.X
	}
	if iy0 < clip.Min.Y {
		iy0 = clip.Min.Y
	}
	if ix1 > clip.Max.X {
		ix1 = clip.Max.X
	}
	if iy1 > clip.Max.Y {
		iy1 = clip.Max.Y
	}
	if ix1 <= ix0 || iy1 <= iy0 {
		return 0, 0, 0, 0
	}
	var sr, sg, sb, sa uint64
	for y := iy0; y < iy1; y++ {
		for x := ix0; x < ix1; x++ {
			pr, pg, pb, pa := src.At(x, y).RGBA()
			sr += uint64(pr)
			sg += uint64(pg)
			sb += uint64(pb)
			sa += uint64(pa)
		}
	}
	n := uint64((ix1 - ix0) * (iy1 - iy0))
	return byte(sr / n >> 8), byte(sg / n >> 8), byte(sb / n >> 8), byte(sa / n >> 8)
}

func metric(index int) int {
	r, _, _ := procGetSystemMetrics.Call(uintptr(index))
	return int(int32(r))
}
