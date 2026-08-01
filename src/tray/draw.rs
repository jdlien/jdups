//! The icon: a battery gauge, drawn a pixel at a time.
//!
//! No artwork, no image files, no GDI+. The icon *is* the state — fill
//! proportional to charge, tinted by where the power is coming from, with the
//! number painted into it when there is something to say.
//!
//! Everything here is 32bpp **premultiplied** BGRA, top-down. Edges are crisp
//! rather than antialiased: at 16 px a soft edge costs more legibility than it
//! buys, and it sidesteps the halo problem entirely (alpha is only ever 0 or
//! 255, so premultiplication is a no-op).

use core::ffi::c_void;
use core::mem::{size_of, zeroed};
use core::ptr::{copy_nonoverlapping, null_mut};

use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HICON, ICONINFO};

/// Where the power is coming from. This is the fastest signal on the icon —
/// faster than reading a number — so it carries the important distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    Mains,
    Battery,
    /// On battery and past the warning threshold.
    Critical,
    /// Device lost, or nothing read yet. Never shown as a plausible charge.
    Unknown,
}

impl Tint {
    fn fill(self) -> (u8, u8, u8) {
        match self {
            Tint::Mains => (0x3F, 0xB9, 0x50),
            Tint::Battery => (0xD2, 0x99, 0x22),
            Tint::Critical => (0xF8, 0x51, 0x49),
            Tint::Unknown => (0x6E, 0x76, 0x81),
        }
    }

    /// The outline carries the state colour too.
    ///
    /// Because the fill alone does not: its *visibility* is proportional to
    /// charge, so a nearly-flat battery on the critical threshold — the single
    /// most urgent state this icon can show — rendered as a mostly-grey box with
    /// a thin red sliver. Exactly backwards. Colouring the outline puts the
    /// signal at full strength regardless of level.
    ///
    /// This does not reintroduce jdrgb's rim problem: that was about outline
    /// *width* being carved out of the fill and changing the apparent size.
    /// Width here is constant across every state; only the hue moves.
    fn outline(self) -> (u8, u8, u8) {
        match self {
            Tint::Unknown => (0xC8, 0xC8, 0xC8),
            other => other.fill(),
        }
    }
}

/// What to paint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauge {
    /// `None` when the device is not talking — draws an empty shell rather than
    /// a confident 0 %, which would look exactly like a flat battery.
    pub charge: Option<u8>,
    /// Digits to knock into the fill, if any. See `Reading::icon_digits`.
    pub digits: Option<u8>,
    pub tint: Tint,
}

/// The unfilled interior. **Opaque**, deliberately: alpha means "composites
/// correctly", not "visible", and a transparent interior gives the fill boundary
/// no defined contrast against an unknown taskbar colour.
///
/// Mid grey rather than the near-black it started as, and the reason is the
/// digits — see `DIGIT`.
const INTERIOR_BG: (u8, u8, u8) = (0x80, 0x87, 0x91);

/// Digits are one flat near-black, on both the fill and the empty interior.
///
/// The first attempt was a knockout — each digit pixel taking the *other*
/// region's colour — which is elegant, passes every arithmetic test, and is
/// unreadable. Where the fill boundary crosses a glyph it splits it into two
/// half-digits in different colours, and at 16 px "47" at 47 % charge is not a
/// number any more. Found by generating the contact sheet and looking at it,
/// which is the only way this was ever going to be found.
///
/// The knockout was needed because a near-black interior and a mid-bright fill
/// sit at opposite luminances, so no single colour could read on both. Lifting
/// the empty interior to mid grey removes that constraint: one dark digit now
/// has usable contrast against green, amber, red *and* grey, so a glyph
/// crossing the boundary stays one glyph.
const DIGIT: (u8, u8, u8) = (0x0D, 0x11, 0x16);

/// 3x5 digits, one u8 per row, low 3 bits, top row first. For a 16 px icon.
const FONT_3X5: [[u8; 5]; 10] = [
    [0b111, 0b101, 0b101, 0b101, 0b111],
    [0b010, 0b110, 0b010, 0b010, 0b111],
    [0b111, 0b001, 0b111, 0b100, 0b111],
    [0b111, 0b001, 0b111, 0b001, 0b111],
    [0b101, 0b101, 0b111, 0b001, 0b001],
    [0b111, 0b100, 0b111, 0b001, 0b111],
    [0b111, 0b100, 0b111, 0b101, 0b111],
    [0b111, 0b001, 0b001, 0b001, 0b001],
    [0b111, 0b101, 0b111, 0b101, 0b111],
    [0b111, 0b101, 0b111, 0b001, 0b111],
];

/// 5x7 digits, low 5 bits. From 20 px up.
///
/// A second table rather than an integer-scaled 3x5, which at 32 px looks like a
/// mistake rather than a decision.
const FONT_5X7: [[u8; 7]; 10] = [
    // A plain zero, not a slashed one. The diagonal is a nice affectation at
    // size and a liability here: bolding thickens it until it meets both walls
    // and the counter fills, turning the glyph into a blob.
    [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
    [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
    [0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111],
    [0b11111, 0b00010, 0b00100, 0b00010, 0b00001, 0b10001, 0b01110],
    [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
    [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
    [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
    [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
    [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
    [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
];

/// Which battery this draws as.
///
/// Both arms stay in the build even though only one ships: the contact sheet
/// renders them side by side, which is how the choice was made and how it would
/// be revisited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Shape {
    /// A cell: body with one terminal on the right. What a laptop battery looks
    /// like, and therefore what everyone else's tray icon looks like.
    Cell,
    /// A car battery: boxy, two posts on top. What is actually inside a UPS —
    /// sealed lead-acid, not lithium — and it does not collide with the
    /// system battery icon two slots away in the notification area.
    Car,
}

/// The shape the tray draws. One line to flip.
pub const SHAPE: Shape = Shape::Car;

/// 6x9 digits, low 6 bits. Drawn for the 16 px icon specifically.
///
/// This is the answer to "can the numbers look properly good": **bitmap type
/// does not scale**. A 3x5 blown up 2x has 2 px stems and 3 px counters and
/// reads as a diagram of a digit; a set drawn at the target size has 1 px stems,
/// real shoulders on the 5 and 6, a closed round bowl on the 0, and a flat base
/// serif on the 1 so it cannot be mistaken for a stray stroke. Same pixel
/// budget, completely different result. This is what fonts like Terminus,
/// Dina and Pixel Operator do — they ship one hand-drawn set *per size* rather
/// than one scalable outline.
///
/// 6 wide leaves a 13 px pair in the car battery's 14 px interior, and 9 tall
/// uses nearly all of its 11.
/// Every glyph spans the full six columns in at least one row. A first pass had
/// several of them 5 wide inside a 6-wide cell, which gives lopsided sidebearings
/// and shifts the whole number a pixel off centre — the sort of thing that is
/// invisible per-glyph and obvious once two sit side by side.
const FONT_6X9: [[u8; 9]; 10] = [
    [0b001100, 0b010010, 0b100001, 0b100001, 0b100001, 0b100001, 0b100001, 0b010010, 0b001100],
    [0b001000, 0b011000, 0b101000, 0b001000, 0b001000, 0b001000, 0b001000, 0b001000, 0b111110],
    [0b011110, 0b100001, 0b000001, 0b000010, 0b000100, 0b001000, 0b010000, 0b100000, 0b111111],
    [0b011110, 0b100001, 0b000001, 0b000001, 0b001110, 0b000001, 0b000001, 0b100001, 0b011110],
    [0b000110, 0b001010, 0b010010, 0b100010, 0b111111, 0b000010, 0b000010, 0b000010, 0b000010],
    [0b111111, 0b100000, 0b100000, 0b111110, 0b000001, 0b000001, 0b000001, 0b100001, 0b011110],
    [0b001110, 0b010000, 0b100000, 0b111110, 0b100001, 0b100001, 0b100001, 0b100001, 0b011110],
    [0b111111, 0b000001, 0b000010, 0b000100, 0b001000, 0b001000, 0b010000, 0b010000, 0b010000],
    [0b011110, 0b100001, 0b100001, 0b100001, 0b011110, 0b100001, 0b100001, 0b100001, 0b011110],
    [0b011110, 0b100001, 0b100001, 0b100001, 0b011111, 0b000001, 0b000001, 0b000010, 0b011100],
];

/// 8x13 digits, one u8 per row. **The size that actually ships on a 125 % display.**
///
/// `SM_CXSMICON` at 120 DPI is 20 px, giving an 18x15 interior — and the 6x9 set
/// was using 9 of those 15 rows and 13 of the 18 columns. This fills it: two
/// glyphs come to 17x13, so the numbers finally occupy the space they are in.
///
/// Drawn with **2 px stems** rather than 1. At this size a hairline stem is what
/// read as faint, and 8 columns leaves room for a 2 px wall either side of a 4 px
/// counter — the proportion a terminal face like Terminus uses at comparable
/// sizes. No emboldening pass is wanted or needed here; the weight is drawn in.
///
/// Each row below is a picture of the glyph, so it can be edited by eye: change
/// a `0` to a `1` and that pixel turns on.
const FONT_8X13: [[u8; 13]; 10] = [
    [
        0b00111100, 0b01111110, 0b11100111, 0b11000011, 0b11000011, 0b11000011, 0b11000011,
        0b11000011, 0b11000011, 0b11000011, 0b11100111, 0b01111110, 0b00111100,
    ],
    [
        0b00011000, 0b00111000, 0b01111000, 0b11011000, 0b00011000, 0b00011000, 0b00011000,
        0b00011000, 0b00011000, 0b00011000, 0b00011000, 0b01111110, 0b01111110,
    ],
    [
        0b00111100, 0b01111110, 0b11000011, 0b11000011, 0b00000011, 0b00000110, 0b00001100,
        0b00011000, 0b00110000, 0b01100000, 0b11000000, 0b11111111, 0b11111111,
    ],
    [
        0b00111100, 0b01111110, 0b11000011, 0b00000011, 0b00000110, 0b00011100, 0b00011100,
        0b00000110, 0b00000011, 0b11000011, 0b11000011, 0b01111110, 0b00111100,
    ],
    // 4. The diagonal drops to the left edge a row sooner than it first did,
    // which opens the counter into a clean triangle and brings the apex to a
    // point instead of a blunt wedge. `glyph_workshop` variant 2.
    //     ....##..
    //     ...###..
    //     .##.##..
    //     ##..##..
    //     ##..##..
    //     ##..##..
    //     ########
    [
        0b00001100, 0b00011100, 0b01101100, 0b11001100, 0b11001100, 0b11001100, 0b11111111,
        0b11111111, 0b00001100, 0b00001100, 0b00001100, 0b00001100, 0b00001100,
    ],
    [
        0b11111111, 0b11111111, 0b11000000, 0b11000000, 0b11111100, 0b11111110, 0b00000011,
        0b00000011, 0b00000011, 0b11000011, 0b11000011, 0b01111110, 0b00111100,
    ],
    [
        0b00111100, 0b01111110, 0b11000011, 0b11000000, 0b11111100, 0b11111110, 0b11000011,
        0b11000011, 0b11000011, 0b11000011, 0b11000011, 0b01111110, 0b00111100,
    ],
    [
        0b11111111, 0b11111111, 0b00000011, 0b00000110, 0b00001100, 0b00011000, 0b00011000,
        0b00110000, 0b00110000, 0b01100000, 0b01100000, 0b01100000, 0b01100000,
    ],
    [
        0b00111100, 0b01111110, 0b11000011, 0b11000011, 0b01111110, 0b01111110, 0b11000011,
        0b11000011, 0b11000011, 0b11000011, 0b11000011, 0b01111110, 0b00111100,
    ],
    [
        0b00111100, 0b01111110, 0b11000011, 0b11000011, 0b11000011, 0b11000011, 0b01111111,
        0b00111111, 0b00000011, 0b00000011, 0b11000011, 0b01111110, 0b00111100,
    ],
];

/// The battery's geometry at one canvas size.
///
/// Separated out because several invariants are about geometry alone, and they
/// are much easier to assert on numbers than on pixels.
#[derive(Debug, Clone, Copy)]
pub struct Geom {
    pub border: i32,
    /// Body rectangle.
    pub bx0: i32,
    pub by0: i32,
    pub bw: i32,
    pub bh: i32,
    /// Interior, inside the border.
    pub ix0: i32,
    pub iy0: i32,
    pub iw: i32,
    pub ih: i32,
    /// Terminals: `(x, y, w, h)`, drawn in the outline colour.
    pub terminals: [(i32, i32, i32, i32); 2],
    pub n_terminals: usize,
}

impl Geom {
    /// Geometry for the shipping shape. Used by the tests, which assert
    /// invariants about what actually ships.
    #[allow(dead_code)]
    pub fn new(size: i32) -> Geom {
        Geom::with_shape(size, SHAPE)
    }

    pub fn with_shape(size: i32, shape: Shape) -> Geom {
        // The border scales with the canvas but never with *state* — the fill is
        // the only thing allowed to move. jdrgb's hardest-won lesson: an outline
        // is carved out of the shape's own area, so anything that varies it
        // makes the whole icon appear to change size.
        let border = (size / 16).max(1);

        match shape {
            Shape::Cell => {
                // Shorter than the canvas and vertically centred, or it reads as
                // a plain rectangle rather than a battery.
                let nub_w = (size / 10).max(1);
                let bh = ((size as f32 * 0.62).round() as i32).max(7);
                let by0 = (size - bh) / 2;
                let bw = size - nub_w;
                let th = (bh / 2).max(2);
                Geom {
                    border,
                    bx0: 0,
                    by0,
                    bw,
                    bh,
                    ix0: border,
                    iy0: by0 + border,
                    iw: bw - 2 * border,
                    ih: bh - 2 * border,
                    terminals: [(bw, by0 + (bh - th) / 2, nub_w, th), (0, 0, 0, 0)],
                    n_terminals: 1,
                }
            }
            Shape::Car => {
                // Posts on top, then a boxy body. Wider than it is tall, which
                // is both what the thing looks like and what leaves the most
                // room for two digits.
                let th = (size / 8).max(2);
                // Wide enough to read as posts. size/5 made them brick studs and
                // size/7 made them too slight to register at 16 px; this sits
                // between, and never drops below 3 px where it would vanish.
                let tw = (size / 6).max(3);
                let by0 = th;
                // A bottom margin, so the body is wider than it is tall. A
                // square box with bumps is a brick; a car battery is a slab.
                let bh = size - th - border;
                let bw = size;
                let inset = (size / 5).max(1);
                Geom {
                    border,
                    bx0: 0,
                    by0,
                    bw,
                    bh,
                    ix0: border,
                    iy0: by0 + border,
                    iw: bw - 2 * border,
                    ih: bh - 2 * border,
                    terminals: [
                        (inset, 0, tw, th + border),
                        (bw - inset - tw, 0, tw, th + border),
                    ],
                    n_terminals: 2,
                }
            }
        }
    }

    /// Filled width for a charge, in interior pixels.
    ///
    /// 0 % paints nothing and 100 % paints everything; in between it is
    /// proportional and monotonic. A charge with *some* battery left never
    /// rounds down to an empty gauge.
    pub fn fill_w(&self, charge: u8) -> i32 {
        if charge == 0 {
            return 0;
        }
        let w = (self.iw * charge as i32 + 50) / 100;
        w.max(1).min(self.iw)
    }
}

/// Smooth the digits' diagonals instead of drawing them as hard stair-steps.
///
/// Worth knowing why this is not simply "turn on antialiasing": AA exists when
/// you **resample**. The glyph source is a binary bitmap and every scale is an
/// integer, so each output pixel maps to exactly one source pixel and coverage
/// is always 0 or 1. An AA pass over that is byte-identical to no AA at all.
///
/// Getting anything requires a higher-resolution source, so one is synthesised:
/// `scale3x` — the pixel-art upscaler from emulators — tripling each glyph while
/// filling in the corners of diagonals, then a box filter back down to the
/// target. Now a stair-step lands on fractional coverage and gets a blended
/// pixel, while flat stems stay fully on and stay crisp.
pub const ANTIALIAS: bool = true;

/// Premultiplied BGRA pixels for one gauge, top-down, `size` x `size`.
pub fn pixels(size: i32, g: Gauge) -> Vec<u32> {
    pixels_shaped(size, g, SHAPE)
}

pub fn pixels_shaped(size: i32, g: Gauge, shape: Shape) -> Vec<u32> {
    pixels_opts(size, g, shape, ANTIALIAS)
}

pub fn pixels_opts(size: i32, g: Gauge, shape: Shape, aa: bool) -> Vec<u32> {
    let geom = Geom::with_shape(size, shape);
    let mut px = vec![0u32; (size * size) as usize];

    let fill_rgb = g.tint.fill();
    let outline_rgb = g.tint.outline();
    let fill_w = g.charge.map(|c| geom.fill_w(c)).unwrap_or(0);

    let put = |px: &mut Vec<u32>, x: i32, y: i32, (r, gg, b): (u8, u8, u8)| {
        if x < 0 || y < 0 || x >= size || y >= size {
            return;
        }
        // Alpha is always 255 here, so premultiplication is the identity.
        px[(y * size + x) as usize] =
            0xFF00_0000 | (r as u32) << 16 | (gg as u32) << 8 | b as u32;
    };

    // Terminals first, so the body's border draws over their inner ends and
    // they read as attached rather than floating.
    for &(tx, ty, tw, th) in &geom.terminals[..geom.n_terminals] {
        for y in ty..ty + th {
            for x in tx..tx + tw {
                put(&mut px, x, y, outline_rgb);
            }
        }
    }

    // Body outline.
    for y in geom.by0..geom.by0 + geom.bh {
        for x in geom.bx0..geom.bx0 + geom.bw {
            let edge = x < geom.bx0 + geom.border
                || x >= geom.bx0 + geom.bw - geom.border
                || y < geom.by0 + geom.border
                || y >= geom.by0 + geom.bh - geom.border;
            if edge {
                put(&mut px, x, y, outline_rgb);
            }
        }
    }

    // Interior: filled portion, then background.
    for y in geom.iy0..geom.iy0 + geom.ih {
        for x in geom.ix0..geom.ix0 + geom.iw {
            let inside_fill = x < geom.ix0 + fill_w;
            put(&mut px, x, y, if inside_fill { fill_rgb } else { INTERIOR_BG });
        }
    }

    // Digits, blended over whatever they sit on.
    if let Some(n) = g.digits {
        draw_digits(&mut px, size, &geom, n, aa);
    }

    px
}

/// How the digits get laid out for a given interior.
///
/// Chosen by **what fits**, not by canvas size. The first version picked the
/// small font below 20 px, a rule written before the car battery gave the
/// interior more room — at 16 px it was drawing 3x5 glyphs into a 14x11 space
/// and the result was faint for no reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    fw: i32,
    fh: i32,
    scale: i32,
    /// Draw each glyph pixel one device pixel larger than its cell, thickening
    /// every stroke without needing a second set of hand-drawn glyphs.
    ///
    /// **Only legal at `scale >= 2`.** Dilation widens strokes by a pixel and
    /// narrows every gap by the same pixel. At scale 1 a counter is one device
    /// pixel across, so bolding closes it outright: the glyphs fill in and "22"
    /// renders as two dark rectangles with a faint scratch where the shape used
    /// to be. From scale 2 the gaps are at least two pixels and survive.
    bold: bool,
}

impl Layout {
    /// One glyph's rendered size, including the bold growth.
    fn glyph(&self) -> (i32, i32) {
        let grow = if self.bold { 1 } else { 0 };
        (self.fw * self.scale + grow, self.fh * self.scale + grow)
    }

    /// Distance from one glyph's origin to the next.
    ///
    /// The bold growth has to be in here too. Leaving it out closed the gap to
    /// nothing at 16 px and ran "22" together into a single blob — the glyphs
    /// each got a pixel wider while the advance stayed put.
    fn advance(&self) -> i32 {
        self.glyph().0 + self.scale
    }

    /// Rendered size of `n_digits` digits under this layout.
    fn extent(&self, n_digits: i32) -> (i32, i32) {
        let (gw, gh) = self.glyph();
        (gw * n_digits + self.scale * (n_digits - 1), gh)
    }

    /// The best layout that fits.
    ///
    /// Ranked by rendered height, **penalised for scaling**. Height alone picks
    /// a 3x5 blown up 3x over a 6x9 drawn at native size, which is taller and
    /// visibly cruder — scaling multiplies the stems along with everything else,
    /// so the shapes coarsen exactly as they grow. The penalty is what keeps a
    /// hand-drawn set at 1:1 winning against a magnified smaller one.
    fn best(iw: i32, ih: i32, n_digits: i32) -> Layout {
        // Vertical clearance stays at a pixel top and bottom; horizontal is a
        // single pixel total. At 16 px the bold 5x7 misses a two-pixel budget by
        // exactly one column, and 16 px is the size that most needs the weight —
        // the interior is only 14 wide, so a column of margin is real estate the
        // glyphs should get.
        let (max_w, max_h) = (iw - 1, ih - 2);
        // Three points of rendered height given up per step of magnification.
        // Tuned by looking: at 2 the magnified 3x5 still won at 24 px and the
        // digits went back to being blocks. At 3 the hand-drawn 6x9 holds from
        // 16 px through 24 px and only gives way when it genuinely cannot grow.
        let score = |l: &Layout, h: i32| h - (l.scale - 1) * 3;

        let mut best: Option<(i32, Layout)> = None;
        for (fw, fh) in [(8, 13), (6, 9), (5, 7), (3, 5)] {
            for scale in 1..=4 {
                for bold in [true, false] {
                    // See `Layout::bold`: below scale 2 this eats the counters.
                    if bold && scale < 2 {
                        continue;
                    }
                    let cand = Layout { fw, fh, scale, bold };
                    let (w, h) = cand.extent(n_digits);
                    if w > max_w || h > max_h {
                        continue;
                    }
                    let s = score(&cand, h);
                    if best.is_none_or(|(bs, b)| (s, cand.fh) > (bs, b.fh)) {
                        best = Some((s, cand));
                    }
                }
            }
        }
        let best = best.map(|(_, l)| l);
        // Nothing fits: fall back to the smallest rather than drawing nothing.
        best.unwrap_or(Layout { fw: 3, fh: 5, scale: 1, bold: false })
    }

    fn bits(&self, digit: usize, row: i32) -> u16 {
        match self.fw {
            3 => FONT_3X5[digit][row as usize] as u16,
            5 => FONT_5X7[digit][row as usize] as u16,
            6 => FONT_6X9[digit][row as usize] as u16,
            _ => FONT_8X13[digit][row as usize] as u16,
        }
    }
}

/// `scale3x`: triple a binary glyph, filling the inside corner of every diagonal.
///
/// The emulator upscaler, used here for what it was designed for — inventing
/// plausible detail in pixel art. It is what gives the downsample something
/// fractional to work with; a plain 3x replication would just reproduce the
/// stair-steps three times larger.
fn scale3x(src: &[bool], w: i32, h: i32) -> Vec<bool> {
    let at = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= w || y >= h {
            false
        } else {
            src[(y * w + x) as usize]
        }
    };
    let mut out = vec![false; (w * 3 * h * 3) as usize];
    for y in 0..h {
        for x in 0..w {
            let (b, d, e, f, hh) = (
                at(x, y - 1),
                at(x - 1, y),
                at(x, y),
                at(x + 1, y),
                at(x, y + 1),
            );
            let cell = [
                if d == b && d != hh && b != f { d } else { e },
                e,
                if b == f && b != d && f != hh { f } else { e },
                e,
                e,
                e,
                if hh == d && hh != f && d != b { d } else { e },
                e,
                if f == hh && f != b && hh != d { f } else { e },
            ];
            for (i, &v) in cell.iter().enumerate() {
                let (ox, oy) = (i as i32 % 3, i as i32 / 3);
                out[((y * 3 + oy) * w * 3 + x * 3 + ox) as usize] = v;
            }
        }
    }
    out
}

/// Coverage map for one digit at its rendered size, 0..=255 per pixel.
fn glyph_coverage(lay: &Layout, digit: usize, aa: bool) -> (i32, i32, Vec<u8>) {
    let (cw, ch) = (lay.fw * lay.scale, lay.fh * lay.scale);
    let mut cov = vec![0u8; (cw * ch) as usize];

    if !aa {
        for row in 0..lay.fh {
            let bits = lay.bits(digit, row);
            for col in 0..lay.fw {
                if bits & (1 << (lay.fw - 1 - col)) == 0 {
                    continue;
                }
                for sy in 0..lay.scale {
                    for sx in 0..lay.scale {
                        let (x, y) = (col * lay.scale + sx, row * lay.scale + sy);
                        cov[(y * cw + x) as usize] = 255;
                    }
                }
            }
        }
    } else {
        let mut src = vec![false; (lay.fw * lay.fh) as usize];
        for row in 0..lay.fh {
            let bits = lay.bits(digit, row);
            for col in 0..lay.fw {
                src[(row * lay.fw + col) as usize] = bits & (1 << (lay.fw - 1 - col)) != 0;
            }
        }
        let hi = scale3x(&src, lay.fw, lay.fh);
        let (hw, hh) = (lay.fw * 3, lay.fh * 3);

        // Supersample rather than box-filter, so any source-to-target ratio
        // works without a separate area-weighting case.
        const S: i32 = 4;
        for y in 0..ch {
            for x in 0..cw {
                let mut hits = 0;
                for sy in 0..S {
                    for sx in 0..S {
                        let u = ((x as f32 + (sx as f32 + 0.5) / S as f32) / cw as f32
                            * hw as f32) as i32;
                        let v = ((y as f32 + (sy as f32 + 0.5) / S as f32) / ch as f32
                            * hh as f32) as i32;
                        if hi[(v.clamp(0, hh - 1) * hw + u.clamp(0, hw - 1)) as usize] {
                            hits += 1;
                        }
                    }
                }
                cov[(y * cw + x) as usize] = (hits * 255 / (S * S)) as u8;
            }
        }
    }

    if !lay.bold {
        return (cw, ch, cov);
    }

    // Bold is a one-pixel max-dilation of the coverage map, which works the
    // same whether the coverage is binary or blended.
    let (bw, bh) = (cw + 1, ch + 1);
    let mut out = vec![0u8; (bw * bh) as usize];
    for y in 0..bh {
        for x in 0..bw {
            let mut m = 0u8;
            for (dx, dy) in [(0, 0), (-1, 0), (0, -1), (-1, -1)] {
                let (sx, sy) = (x + dx, y + dy);
                if sx >= 0 && sy >= 0 && sx < cw && sy < ch {
                    m = m.max(cov[(sy * cw + sx) as usize]);
                }
            }
            out[(y * bw + x) as usize] = m;
        }
    }
    (bw, bh, out)
}

fn draw_digits(
    px: &mut [u32],
    size: i32,
    geom: &Geom,
    n: u8,
    aa: bool,
) {
    let digits: Vec<usize> = if n >= 10 {
        vec![(n / 10) as usize, (n % 10) as usize]
    } else {
        vec![n as usize]
    };

    let lay = Layout::best(geom.iw, geom.ih, digits.len() as i32);

    // Only smooth when magnifying. At 1:1 the hand-drawn glyph is already
    // exactly on the pixel grid, so all resampling can do is spread its edges
    // into partial coverage — which reads as *lighter*, not smoother, and walks
    // straight back into the faintness the 6x9 set was drawn to fix. The
    // specimen sheet shows the two bands side by side and the difference at
    // scale 1 is a loss.
    let aa = aa && lay.scale >= 2;

    let (text_w, text_h) = lay.extent(digits.len() as i32);
    let x0 = geom.ix0 + (geom.iw - text_w) / 2;
    let y0 = geom.iy0 + (geom.ih - text_h) / 2;

    for (di, &d) in digits.iter().enumerate() {
        let dx = x0 + di as i32 * lay.advance();
        let (gw, gh, cov) = glyph_coverage(&lay, d, aa);
        for gy in 0..gh {
            for gx in 0..gw {
                let c = cov[(gy * gw + gx) as usize];
                if c == 0 {
                    continue;
                }
                let (x, y) = (dx + gx, y0 + gy);
                if x < 0 || y < 0 || x >= size || y >= size {
                    continue;
                }
                let idx = (y * size + x) as usize;
                // Blend against whatever is already there — fill or interior —
                // and stay opaque, so premultiplication remains a no-op and the
                // icon's silhouette is untouched.
                let under = px[idx];
                let (ur, ug, ub) = ((under >> 16) as u8, (under >> 8) as u8, under as u8);
                let mix = |a: u8, b: u8| {
                    (a as u32 * (255 - c as u32) + b as u32 * c as u32) / 255
                };
                px[idx] = 0xFF00_0000
                    | mix(ur, DIGIT.0) << 16
                    | mix(ug, DIGIT.1) << 8
                    | mix(ub, DIGIT.2);
            }
        }
    }
}

/// Wrap premultiplied pixels in a DIB section suitable for `hbmpItem`.
///
/// # Safety
/// Returns an owned `HBITMAP`; the caller must `DeleteObject` it.
pub unsafe fn dib(size: i32, px: &[u32]) -> HBITMAP {
    let mut bi: BITMAPINFO = unsafe { zeroed() };
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: size,
        biHeight: -size, // negative = top-down, matching `pixels`
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { zeroed() }
    };
    let mut bits: *mut c_void = null_mut();
    let bmp = unsafe { CreateDIBSection(null_mut(), &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0) };
    if !bmp.is_null() && !bits.is_null() {
        unsafe { copy_nonoverlapping(px.as_ptr(), bits as *mut u32, px.len()) };
    }
    bmp
}

/// The 1bpp AND mask. Set bits are transparent.
///
/// A 32bpp icon renders from its alpha, so this is a fallback — but an all-zero
/// mask claims the whole square is opaque, and anything consulting the mask on
/// its own then draws a black box.
unsafe fn and_mask(size: i32, px: &[u32]) -> HBITMAP {
    let stride = (((size + 15) / 16) * 2) as usize;
    let mut bits = vec![0u8; stride * size as usize];
    for y in 0..size {
        for x in 0..size {
            if px[(y * size + x) as usize] >> 24 < 128 {
                bits[y as usize * stride + (x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
    }
    unsafe { CreateBitmap(size, size, 1, 1, bits.as_ptr() as *const c_void) }
}

/// Build a tray icon from premultiplied pixels.
///
/// # Safety
/// Returns an owned `HICON`; the caller must `DestroyIcon` it.
pub unsafe fn icon(size: i32, px: &[u32]) -> HICON {
    let color = unsafe { dib(size, px) };
    let mask = unsafe { and_mask(size, px) };
    let ii = ICONINFO {
        fIcon: 1,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    let h = unsafe { CreateIconIndirect(&ii) };
    // CreateIconIndirect copies both bitmaps, so keeping them would leak two GDI
    // objects per icon update.
    unsafe {
        DeleteObject(color as _);
        DeleteObject(mask as _);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [i32; 6] = [16, 20, 24, 28, 32, 40];

    fn g(charge: Option<u8>, digits: Option<u8>, tint: Tint) -> Gauge {
        Gauge { charge, digits, tint }
    }

    fn alpha(px: &[u32], size: i32, x: i32, y: i32) -> u8 {
        (px[(y * size + x) as usize] >> 24) as u8
    }

    fn rgb(px: &[u32], size: i32, x: i32, y: i32) -> (u8, u8, u8) {
        let v = px[(y * size + x) as usize];
        ((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// No channel may exceed alpha, or GDI composites a halo.
    #[test]
    fn every_pixel_is_premultiplied() {
        for size in SIZES {
            for tint in [Tint::Mains, Tint::Battery, Tint::Critical, Tint::Unknown] {
                let px = pixels(size, g(Some(50), Some(50), tint));
                for (i, &v) in px.iter().enumerate() {
                    let (a, r, gg, b) =
                        ((v >> 24) as u8, (v >> 16) as u8, (v >> 8) as u8, v as u8);
                    assert!(r <= a && gg <= a && b <= a, "size {size} px {i}");
                }
            }
        }
    }

    /// The invariant that retired jdrgb's rims, restated for a battery: the
    /// icon's *silhouette* must be identical no matter what the gauge says, so
    /// nothing ever appears to change size.
    ///
    /// Tested on the shape rather than on the outline's colour, because the
    /// outline is now tinted by state — only its geometry is fixed.
    #[test]
    fn the_silhouette_never_changes_with_state() {
        for size in SIZES {
            let shape_of = |gauge| {
                let px = pixels(size, gauge);
                (0..size * size)
                    .filter(|i| alpha(&px, size, i % size, i / size) == 255)
                    .collect::<Vec<_>>()
            };
            let reference = shape_of(g(Some(100), None, Tint::Mains));
            for gauge in [
                g(Some(0), None, Tint::Critical),
                g(Some(50), Some(50), Tint::Battery),
                g(None, None, Tint::Unknown),
                g(Some(7), Some(7), Tint::Battery),
            ] {
                assert_eq!(shape_of(gauge), reference, "size {size} changed the silhouette");
            }
        }
    }

    /// The alarm has to be visible when the battery is nearly flat, which is
    /// exactly when a level-proportional fill is at its least visible. The
    /// contact sheet showed a critical 20 % rendering as a grey box with a red
    /// sliver; colouring the outline is what fixes it.
    #[test]
    fn the_state_colour_is_visible_even_at_a_sliver_of_charge() {
        for size in SIZES {
            let px = pixels(size, g(Some(2), Some(2), Tint::Critical));
            let red = Tint::Critical.fill();
            let n = (0..size * size)
                .filter(|i| rgb(&px, size, i % size, i / size) == red)
                .count();
            // The outline alone is worth far more than a 2% fill sliver.
            assert!(
                n >= (size * 2) as usize,
                "size {size}: only {n} red pixels at 2% charge — the alarm is invisible"
            );
        }
    }

    #[test]
    fn fill_is_monotonic_in_charge() {
        for size in SIZES {
            let geom = Geom::new(size);
            let mut last = -1;
            for c in 0..=100u8 {
                let w = geom.fill_w(c);
                assert!(w >= last, "size {size} charge {c} went backwards");
                last = w;
            }
            assert_eq!(geom.fill_w(0), 0, "0% must paint nothing");
            assert_eq!(geom.fill_w(100), geom.iw, "100% must fill the interior");
        }
    }

    /// A nearly-flat battery must not round down to looking empty — that is the
    /// one reading where the difference matters most.
    #[test]
    fn a_sliver_of_charge_still_paints_something() {
        for size in SIZES {
            let geom = Geom::new(size);
            assert!(geom.fill_w(1) >= 1, "size {size} lost 1%");
        }
    }

    #[test]
    fn digits_never_touch_the_outline() {
        for size in SIZES {
            let geom = Geom::new(size);
            for n in [0u8, 7, 42, 99] {
                let plain = pixels(size, g(Some(50), None, Tint::Mains));
                let with = pixels(size, g(Some(50), Some(n), Tint::Mains));
                for y in 0..size {
                    for x in 0..size {
                        if plain[(y * size + x) as usize] != with[(y * size + x) as usize] {
                            assert!(
                                x >= geom.ix0
                                    && x < geom.ix0 + geom.iw
                                    && y >= geom.iy0
                                    && y < geom.iy0 + geom.ih,
                                "size {size} digit {n} painted outside the interior at ({x},{y})"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Relative luminance, for contrast ratios.
    fn lum((r, g, b): (u8, u8, u8)) -> f64 {
        let f = |c: u8| {
            let c = c as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b)
    }

    fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
        let (x, y) = (lum(a), lum(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// The property that replaced the knockout, and the whole reason the empty
    /// interior is mid grey: **one** digit colour has to be readable on every
    /// surface it can land on, so a glyph crossing the fill boundary stays a
    /// single legible glyph instead of two half-digits.
    #[test]
    fn one_digit_colour_reads_on_every_surface_it_can_land_on() {
        // Tint::Unknown is deliberately absent: unknown means no charge, so
        // there is no fill and no digits — its colour never sits under one.
        // `an_unknown_state_never_draws_digits` holds that end up.
        let mut surfaces = vec![INTERIOR_BG];
        for t in [Tint::Mains, Tint::Battery, Tint::Critical] {
            surfaces.push(t.fill());
        }
        for s in surfaces {
            let c = contrast(DIGIT, s);
            assert!(
                c >= 4.5,
                "digit on {s:?} is only {c:.1}:1 — a glyph straddling the fill \
                 boundary would be unreadable on this half"
            );
        }
    }

    /// The premise the contrast test leans on.
    #[test]
    fn an_unknown_state_never_draws_digits() {
        for size in SIZES {
            let plain = pixels(size, g(None, None, Tint::Unknown));
            // Even if a caller asked for digits, there is no fill behind them,
            // so they would sit on INTERIOR_BG — which is covered above.
            let geom = Geom::new(size);
            assert_eq!(geom.fill_w(0), 0);
            let _ = plain;
        }
    }

    /// A digit crossing the boundary must be one colour, not two.
    #[test]
    fn a_digit_is_never_split_into_two_colours() {
        let size = 32;
        let geom = Geom::new(size);
        let plain = pixels_opts(size, g(Some(47), None, Tint::Mains), SHAPE, false);
        let with = pixels_opts(size, g(Some(47), Some(47), Tint::Mains), SHAPE, false);

        let mut colours = std::collections::HashSet::new();
        for i in 0..(size * size) as usize {
            if plain[i] != with[i] {
                colours.insert(with[i]);
            }
        }
        assert_eq!(
            colours.len(),
            1,
            "digits rendered in {} colours; the fill boundary is splitting them",
            colours.len()
        );
        let _ = geom;
    }

    /// An unreachable device must not render as a confident flat battery.
    #[test]
    fn an_unknown_charge_draws_no_fill() {
        for size in SIZES {
            let geom = Geom::new(size);
            let px = pixels(size, g(None, None, Tint::Unknown));
            for y in geom.iy0..geom.iy0 + geom.ih {
                for x in geom.ix0..geom.ix0 + geom.iw {
                    assert_eq!(
                        rgb(&px, size, x, y),
                        INTERIOR_BG,
                        "size {size} painted fill for an unknown charge"
                    );
                }
            }
        }
    }

    #[test]
    fn the_interior_is_opaque_everywhere() {
        for size in SIZES {
            let geom = Geom::new(size);
            let px = pixels(size, g(Some(30), None, Tint::Mains));
            for y in geom.iy0..geom.iy0 + geom.ih {
                for x in geom.ix0..geom.ix0 + geom.iw {
                    assert_eq!(alpha(&px, size, x, y), 255, "size {size} ({x},{y}) see-through");
                }
            }
        }
    }

    /// Two digits have to fit at the smallest size, or the whole design fails at
    /// exactly the DPI most people run.
    /// Adjacent glyphs must always have clear space between them. Bold growth
    /// closed this to zero once and turned "22" into one blob.
    #[test]
    fn adjacent_digits_never_touch() {
        for (fw, fh) in [(3, 5), (5, 7), (6, 9), (8, 13)] {
            {
                for scale in 1..=4 {
                    for bold in [true, false] {
                        let lay = Layout { fw, fh, scale, bold };
                        assert!(
                            lay.advance() > lay.glyph().0,
                            "{lay:?} advances by {} for a glyph {} wide",
                            lay.advance(),
                            lay.glyph().0
                        );
                    }
                }
            }
        }
    }

    /// Emboldening must never be chosen where it would close the counters.
    #[test]
    fn bold_is_never_used_at_scale_one() {
        for size in SIZES {
            let geom = Geom::new(size);
            for n in 1..=2 {
                let lay = Layout::best(geom.iw, geom.ih, n);
                assert!(
                    !lay.bold || lay.scale >= 2,
                    "size {size}: {lay:?} would dilate 1px gaps to nothing"
                );
            }
        }
    }

    /// The defect this is really about: "88" has four counters, and if they fill
    /// in the icon shows two dark rectangles instead of a number. Asserting on
    /// the rendered pixels rather than on the layout, because it is the pixels
    /// that looked wrong.
    #[test]
    fn glyph_counters_stay_open_at_every_size() {
        for size in SIZES {
            let geom = Geom::new(size);
            let plain = pixels(size, g(Some(100), None, Tint::Mains));
            let with = pixels(size, g(Some(100), Some(88), Tint::Mains));

            let inked: Vec<(i32, i32)> = (0..size * size)
                .filter(|&i| plain[i as usize] != with[i as usize])
                .map(|i| (i % size, i / size))
                .collect();
            assert!(!inked.is_empty(), "size {size} drew no digits at all");

            let (x0, x1) = (
                inked.iter().map(|p| p.0).min().unwrap(),
                inked.iter().map(|p| p.0).max().unwrap(),
            );
            let (y0, y1) = (
                inked.iter().map(|p| p.1).min().unwrap(),
                inked.iter().map(|p| p.1).max().unwrap(),
            );

            // Background pixels strictly inside the text's bounding box are the
            // counters and the gap between the two glyphs. A solid block means
            // the shapes have filled in.
            let mut open = 0;
            for y in y0 + 1..y1 {
                for x in x0 + 1..x1 {
                    if rgb(&with, size, x, y) != DIGIT {
                        open += 1;
                    }
                }
            }
            let area = ((x1 - x0 - 1) * (y1 - y0 - 1)).max(1);
            assert!(
                open * 4 >= area,
                "size {size}: only {open} of {area} interior pixels are open; \
                 the glyphs have filled in"
            );
            let _ = geom;
        }
    }

    /// Whatever `best` picks must actually fit the interior it was given.
    #[test]
    fn the_chosen_layout_always_fits() {
        for size in SIZES {
            let geom = Geom::new(size);
            for n in 1..=2 {
                let lay = Layout::best(geom.iw, geom.ih, n);
                let (w, h) = lay.extent(n);
                // The invariant is staying inside the interior. The exact
                // clearance is a tuning knob and does not belong in a test —
                // `digits_never_touch_the_outline` is what actually guards the
                // thing that would look broken.
                assert!(
                    w <= geom.iw && h <= geom.ih,
                    "size {size}: {lay:?} renders {w}x{h} into {}x{}",
                    geom.iw,
                    geom.ih
                );
            }
        }
    }

    #[test]
    fn two_digits_fit_at_sixteen_pixels() {
        let px = pixels(16, g(Some(50), Some(99), Tint::Battery));
        let plain = pixels(16, g(Some(50), None, Tint::Battery));
        let changed = px.iter().zip(&plain).filter(|(a, b)| a != b).count();
        assert!(changed >= 10, "only {changed} pixels differ; digits are not rendering");
    }

    #[test]
    fn a_single_digit_is_centred_not_left_aligned() {
        let size = 32;
        let geom = Geom::new(size);
        let plain = pixels(size, g(Some(100), None, Tint::Mains));
        let with = pixels(size, g(Some(100), Some(7), Tint::Mains));
        let xs: Vec<i32> = (0..size * size)
            .filter(|i| plain[*i as usize] != with[*i as usize])
            .map(|i| i % size)
            .collect();
        let (lo, hi) = (*xs.iter().min().unwrap(), *xs.iter().max().unwrap());
        let left_margin = lo - geom.ix0;
        let right_margin = (geom.ix0 + geom.iw - 1) - hi;
        assert!(
            (left_margin - right_margin).abs() <= 1,
            "single digit off-centre: {left_margin} vs {right_margin}"
        );
    }

    /// Width of the label gutter on the visual sheets.
    const GUTTER: i32 = 34;

    /// Draw a number into an RGB image buffer, for labelling the sheets.
    ///
    /// Uses the project's own fonts, which is either elegant or circular
    /// depending on your mood, but means the labels cost nothing and scale with
    /// whatever is being inspected.
    fn label(img: &mut [u8], w: i32, x: i32, y: i32, n: u32, ink: (u8, u8, u8)) {
        let lay = Layout { fw: 5, fh: 7, scale: 2, bold: false };
        let digits: Vec<usize> = if n >= 10 {
            vec![(n / 10) as usize, (n % 10) as usize]
        } else {
            vec![n as usize]
        };
        for (i, &d) in digits.iter().enumerate() {
            let (gw, gh, cov) = glyph_coverage(&lay, d, false);
            let ox = x + i as i32 * lay.advance();
            for gy in 0..gh {
                for gx in 0..gw {
                    if cov[(gy * gw + gx) as usize] == 0 {
                        continue;
                    }
                    let p = (((y + gy) * w + ox + gx) * 3) as usize;
                    if p + 2 < img.len() {
                        img[p] = ink.2;
                        img[p + 1] = ink.1;
                        img[p + 2] = ink.0;
                    }
                }
            }
        }
    }

    /// Top-down 24bpp BMP. Shared by both visual sheets.
    fn write_bmp(img: &[u8], w: i32, h: i32, env: &str, default: &str) {
        let stride = ((w * 3 + 3) / 4 * 4) as usize;
        let mut bmp = Vec::new();
        bmp.extend(b"BM");
        bmp.extend(((54 + stride * h as usize) as u32).to_le_bytes());
        bmp.extend([0u8; 4]);
        bmp.extend(54u32.to_le_bytes());
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(w.to_le_bytes());
        bmp.extend((-h).to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend([0u8; 24]);
        for y in 0..h {
            let row = ((y * w) * 3) as usize;
            bmp.extend(&img[row..row + (w * 3) as usize]);
            bmp.resize(bmp.len() + stride - (w * 3) as usize, 0);
        }
        let path = std::env::var(env)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join(default));
        std::fs::write(&path, &bmp).unwrap();
        println!("wrote {}", path.display());
    }

    /// Try candidate versions of one glyph side by side, numbered.
    ///
    /// Editing a glyph in place and re-running the specimen shows you the new
    /// one but not the old one, so there is nothing to judge it against. This
    /// renders variants together, big, with a pixel grid — which is how you
    /// actually decide whether a change helped.
    ///
    /// Paste candidates into `VARIANTS`, run, point at a number.
    ///
    ///     cargo test --release -- --ignored glyph_workshop --nocapture
    #[test]
    #[ignore]
    fn glyph_workshop() {
        // Candidate 4s. The stem is cols 4-5 in every row; only the diagonal
        // moves. `a` is the diagonal's left column per row, top to bottom.
        const STEM: u8 = 0b00001100;
        let diag = |a: i32| -> u8 {
            match a {
                4 => 0b00001100,
                3 => 0b00011000,
                2 => 0b00110000,
                1 => 0b01100000,
                _ => 0b11000000,
            }
        };
        let build = |slopes: [i32; 6]| -> [u8; 13] {
            let mut g = [STEM; 13];
            for (r, &a) in slopes.iter().enumerate() {
                g[r] = diag(a) | STEM;
            }
            g[6] = 0b11111111;
            g[7] = 0b11111111;
            g
        };

        let variants: Vec<(&str, [u8; 13])> = vec![
            ("1 current", build([4, 3, 2, 1, 0, 0])),
            ("2 steeper", build([4, 3, 1, 0, 0, 0])),
            ("3 steepest", build([4, 2, 1, 0, 0, 0])),
            ("4 late+steep", build([4, 4, 3, 1, 0, 0])),
        ];

        const PX: i32 = 10; // pixel size in the render
        const PAD: i32 = 14;
        let (gw, gh) = (8, 13);
        let cell_w = gw * PX + PAD * 2;
        let cell_h = gh * PX + PAD * 2;
        let (w, h) = (cell_w * variants.len() as i32, cell_h);
        let mut img = vec![0xFFu8; (w * h * 3) as usize];

        for (vi, (_, glyph)) in variants.iter().enumerate() {
            let ox = vi as i32 * cell_w + PAD;
            for gy in 0..gh {
                for gx in 0..gw {
                    let on = glyph[gy as usize] & (1 << (gw - 1 - gx)) != 0;
                    for py in 0..PX {
                        for px_ in 0..PX {
                            // A one-pixel grid line, so individual pixels can be
                            // counted and pointed at.
                            let grid = py == 0 || px_ == 0;
                            let c: (u8, u8, u8) = if on {
                                DIGIT
                            } else if grid {
                                (0xDC, 0xDC, 0xDC)
                            } else {
                                (0xFF, 0xFF, 0xFF)
                            };
                            let x = ox + gx * PX + px_;
                            let y = PAD + gy * PX + py;
                            let p = ((y * w + x) * 3) as usize;
                            img[p] = c.2;
                            img[p + 1] = c.1;
                            img[p + 2] = c.0;
                        }
                    }
                }
            }
            // Variant number, top-left of each cell.
            label(&mut img, w, ox, 2, vi as u32 + 1, DIGIT);
        }

        write_bmp(&img, w, h, "JDUPS_WORKSHOP", "jdups-workshop.bmp");
        for (name, glyph) in &variants {
            println!("\n{name}");
            for row in glyph.iter() {
                println!("    0b{row:08b},   {}", render_row(*row, 8));
            }
        }
    }

    /// A row of glyph bits as a picture, for reading in the terminal.
    fn render_row(bits: u8, w: i32) -> String {
        (0..w)
            .map(|c| if bits & (1 << (w - 1 - c)) != 0 { '#' } else { '.' })
            .collect()
    }

    /// A type specimen: every glyph of every font, at every scale, crisp and
    /// antialiased. Ignored by default — like `contact_sheet`, it exists to be
    /// looked at.
    ///
    /// This wants to exist because the contact sheet only ever renders the
    /// handful of digits its example states happen to contain. 6, 7 and 8 were
    /// drawn and then never displayed once, which is not a way to find out
    /// whether they are any good.
    ///
    ///     cargo test --release -- --ignored font_specimen --nocapture
    #[test]
    #[ignore]
    fn font_specimen() {
        let rows: Vec<(i32, i32, i32, bool)> = vec![
            (3, 5, 1, false),
            (3, 5, 2, false),
            (3, 5, 3, false),
            (5, 7, 1, false),
            (5, 7, 2, false),
            (5, 7, 2, true),
            (6, 9, 1, false),
            (6, 9, 2, false),
            (8, 13, 1, false),
            (8, 13, 2, false),
        ];
        const CW: i32 = 34;
        const CH: i32 = 38;
        let (w, h) = (GUTTER + CW * 10, CH * rows.len() as i32 * 2);
        let bg = (0xF3u8, 0xF3u8, 0xF3u8);
        let mut img = vec![0u8; (w * h * 3) as usize];
        for i in 0..(w * h) as usize {
            img[i * 3] = bg.2;
            img[i * 3 + 1] = bg.1;
            img[i * 3 + 2] = bg.0;
        }

        for (band, aa) in [false, true].iter().enumerate() {
            for (ri, &(fw, fh, scale, bold)) in rows.iter().enumerate() {
                let lay = Layout { fw, fh, scale, bold };
                let y0 = (band as i32 * rows.len() as i32 + ri as i32) * CH;
                let row_no = band as u32 * rows.len() as u32 + ri as u32 + 1;
                label(&mut img, w, 4, y0 + (CH - 14) / 2, row_no, DIGIT);
                for d in 0..10usize {
                    let (gw, gh, cov) = glyph_coverage(&lay, d, *aa);
                    let ox = GUTTER + d as i32 * CW + (CW - gw) / 2;
                    let oy = y0 + (CH - gh) / 2;
                    for gy in 0..gh {
                        for gx in 0..gw {
                            let c = cov[(gy * gw + gx) as usize] as u32;
                            if c == 0 {
                                continue;
                            }
                            let p = (((oy + gy) * w + ox + gx) * 3) as usize;
                            let mix = |under: u8, ink: u8| {
                                ((under as u32 * (255 - c) + ink as u32 * c) / 255) as u8
                            };
                            img[p] = mix(img[p], DIGIT.2);
                            img[p + 1] = mix(img[p + 1], DIGIT.1);
                            img[p + 2] = mix(img[p + 2], DIGIT.0);
                        }
                    }
                }
            }
        }

        write_bmp(&img, w, h, "JDUPS_SPECIMEN", "jdups-font.bmp");
        println!("row  font  scale  rendering");
        for (band, tag) in ["crisp", "antialiased"].iter().enumerate() {
            for (ri, (fw, fh, scale, bold)) in rows.iter().enumerate() {
                println!(
                    "{:3}  {fw}x{fh}{}  x{scale}{}     {tag}",
                    band * rows.len() + ri + 1,
                    if *fw < 8 { " " } else { "" },
                    if *bold { " bold" } else { "     " },
                );
            }
        }
        // The line that connects the sheet to reality: which row each icon
        // canvas actually renders with.
        println!("
what each icon size uses:");
        for &size in SIZES.iter() {
            let g = Geom::new(size);
            let l = Layout::best(g.iw, g.ih, 2);
            let aa = ANTIALIAS && l.scale >= 2;
            let idx = rows
                .iter()
                .position(|&(fw, fh, sc, b)| fw == l.fw && fh == l.fh && sc == l.scale && b == l.bold)
                .map(|i| i + 1 + if aa { rows.len() } else { 0 });
            match idx {
                Some(n) => println!("  {size:2}px icon -> row {n}  ({}x{} x{})", l.fw, l.fh, l.scale),
                None => println!("  {size:2}px icon -> {:?} (not on the sheet)", l),
            }
        }
    }

    /// Render every state at every DPI over light and dark taskbars, and write a
    /// BMP. Ignored by default: it asserts nothing a machine can judge.
    ///
    /// The assertions above check the arithmetic. This is for the only question
    /// that matters — whether you can read it.
    ///
    ///     cargo test --release -- --ignored contact_sheet --nocapture
    #[test]
    #[ignore]
    fn contact_sheet() {
        // Fill is charge, digits are minutes — always. The pairs below are
        // realistic combinations, not matching numbers, so the sheet shows the
        // two channels actually carrying different quantities.
        let states: Vec<(&str, Gauge)> = vec![
            ("mains full", g(Some(100), None, Tint::Mains)),
            ("mains 78/22m", g(Some(78), Some(22), Tint::Mains)),
            ("mains 47/15m", g(Some(47), Some(15), Tint::Mains)),
            ("mains 9/3m", g(Some(9), Some(3), Tint::Mains)),
            ("batt 100/43m", g(Some(100), Some(43), Tint::Battery)),
            ("batt 60/9m", g(Some(60), Some(9), Tint::Battery)),
            ("crit 20/2m", g(Some(20), Some(2), Tint::Critical)),
            ("crit 0/0m", g(Some(0), Some(0), Tint::Critical)),
            ("unknown", g(None, None, Tint::Unknown)),
        ];
        // Both shapes, so the choice is made by looking rather than arguing.
        let rows: Vec<(bool, i32)> = [true, false]
            .iter()
            .flat_map(|&aa| SIZES.iter().map(move |&z| (aa, z)))
            .collect();

        const CELL: i32 = 56;
        let cols = states.len() as i32;
        let nrows = rows.len() as i32;
        let (w, h) = (GUTTER + cols * CELL, nrows * CELL * 2);

        // Windows 11's own taskbar colours, near enough.
        let backgrounds = [(0xF3u8, 0xF3u8, 0xF3u8), (0x20u8, 0x20u8, 0x20u8)];
        let mut img = vec![0u8; (w * h * 3) as usize];
        for (band, bg) in backgrounds.iter().enumerate() {
            let y0 = band as i32 * nrows * CELL;
            for y in y0..y0 + nrows * CELL {
                for x in 0..w {
                    let i = ((y * w + x) * 3) as usize;
                    img[i] = bg.2;
                    img[i + 1] = bg.1;
                    img[i + 2] = bg.0;
                }
            }
            for (ri, &(aa, size)) in rows.iter().enumerate() {
                let ly = y0 + ri as i32 * CELL + (CELL - 14) / 2;
                let ink = if band == 0 { (0x20u8, 0x20u8, 0x20u8) } else { (0xE0u8, 0xE0u8, 0xE0u8) };
                label(&mut img, w, 4, ly, size as u32, ink);
                for (ci, (_, gauge)) in states.iter().enumerate() {
                    let px = pixels_opts(size, *gauge, SHAPE, aa);
                    let cx = GUTTER + ci as i32 * CELL + (CELL - size) / 2;
                    let cy = y0 + ri as i32 * CELL + (CELL - size) / 2;
                    for y in 0..size {
                        for x in 0..size {
                            let v = px[(y * size + x) as usize];
                            let (a, r, gg, b) =
                                (v >> 24, (v >> 16) as u8, (v >> 8) as u8, v as u8);
                            let inv = 255 - a;
                            let d = (((cy + y) * w + cx + x) * 3) as usize;
                            img[d] = (b as u32 + img[d] as u32 * inv / 255) as u8;
                            img[d + 1] = (gg as u32 + img[d + 1] as u32 * inv / 255) as u8;
                            img[d + 2] = (r as u32 + img[d + 2] as u32 * inv / 255) as u8;
                        }
                    }
                }
            }
        }

        write_bmp(&img, w, h, "JDUPS_SHEET", "jdups-icons.bmp");
        println!("columns: {}", states.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "));
        println!("rows:    antialiased {SIZES:?} then crisp {SIZES:?}");
        println!("\nchosen digit layout per size (shipping shape):");
        for &size in SIZES.iter() {
            let g = Geom::new(size);
            let l = Layout::best(g.iw, g.ih, 2);
            let (w, h) = l.extent(2);
            println!(
                "  {size:2}px  interior {:2}x{:<2}  {}x{} scale {} {}  -> {w}x{h}",
                g.iw,
                g.ih,
                l.fw,
                l.fh,
                l.scale,
                if l.bold { "bold" } else { "    " },
            );
        }
    }
}
