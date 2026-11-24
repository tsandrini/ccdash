//! The tray icon: ccdash's own (`dms-plugin/icon.svg`, the panel's too),
//! drawn at whatever size the taskbar asks for, with a dot in the corner
//! when something wants a look. Windows takes no SVG and the repository
//! has no `.ico`, so the file's rectangles are read and drawn here.

use std::sync::OnceLock;

/// The repository's icon.
const SVG: &str = include_str!("../../../dms-plugin/icon.svg");

/// A rectangle of the icon, in the units of its view box.
#[derive(Debug, PartialEq)]
struct Shape {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    /// The radius of its corners.
    round: f32,
    colour: [u8; 3],
}

/// What is drawn of an SVG: its `<rect>` elements, in the file's order (a
/// later one lies over an earlier one), and the side of its view box.
#[derive(Debug, PartialEq)]
struct Mark {
    side: f32,
    shapes: Vec<Shape>,
}

/// The value of an attribute of an element, as `name="value"` has it.
fn attribute<'a>(element: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let mut from = 0;
    while let Some(found) = element[from..].find(&key) {
        let at = from + found;
        // NOTE: `x="…"` itself, not the end of `rx="…"`
        if element[..at].ends_with(char::is_whitespace) {
            return element[at + key.len()..].split('"').next();
        }
        from = at + key.len();
    }
    None
}

/// Reads the rectangles of an SVG that is made of them. One with no size
/// or colour this understands (`#rrggbb`) is left out.
fn read(svg: &str) -> Mark {
    let number = |element: &str, name: &str| attribute(element, name)?.parse::<f32>().ok();
    let side = svg
        .split_once("<svg")
        .and_then(|(_, element)| attribute(element, "viewBox")?.split_whitespace().nth(2))
        .and_then(|side| side.parse().ok())
        .unwrap_or(64.0);
    let shapes = svg
        .split("<rect")
        .skip(1)
        .filter_map(|rest| {
            let element = rest.split('>').next()?;
            let fill = attribute(element, "fill")?.strip_prefix('#')?;
            let part = |at: usize| u8::from_str_radix(fill.get(at..at + 2)?, 16).ok();
            Some(Shape {
                left: number(element, "x").unwrap_or(0.0),
                top: number(element, "y").unwrap_or(0.0),
                width: number(element, "width")?,
                height: number(element, "height")?,
                round: number(element, "rx").unwrap_or(0.0),
                colour: [part(0)?, part(2)?, part(4)?],
            })
        })
        .collect();
    Mark { side, shapes }
}

fn mark() -> &'static Mark {
    static MARK: OnceLock<Mark> = OnceLock::new();
    MARK.get_or_init(|| read(SVG))
}

/// What the dot in the icon's corner says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    /// Connected and nothing waits: no dot.
    None,
    /// A session waits for the user.
    Waiting,
    /// The hub is out of reach; the collector queues meanwhile.
    Offline,
    /// The collector does not run, or cannot talk to its hub.
    Trouble,
    /// An update is being put in place.
    Busy,
}

impl Badge {
    fn colour(self) -> Option<[u8; 3]> {
        match self {
            Self::None => None,
            Self::Waiting => Some([0xf2, 0xb0, 0x1e]),
            Self::Offline => Some([0x9a, 0x9a, 0x96]),
            Self::Trouble => Some([0xe5, 0x48, 0x4d]),
            Self::Busy => Some([0x39, 0x87, 0xe5]),
        }
    }
}

/// The dot's centre, its radius, and the radius cleared around it, in
/// sixty-fourths of the icon's side.
const DOT: (f32, f32, f32, f32) = (48.0, 48.0, 13.0, 16.5);
/// Samples per pixel and side, for smooth edges.
const SAMPLES: usize = 4;

fn in_round_rect(x: f32, y: f32, (left, top, width, height): (f32, f32, f32, f32), r: f32) -> bool {
    if x < left || y < top || x >= left + width || y >= top + height {
        return false;
    }
    // NOTE: the nearest point of the rectangle shrunk by the radius: inside it, or within `r`
    let cx = x.clamp(left + r, left + width - r);
    let cy = y.clamp(top + r, top + height - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

/// What is at a point of the icon, in the units of `mark`: a colour, or
/// nothing.
fn at(mark: &Mark, x: f32, y: f32, badge: Badge) -> Option<[u8; 3]> {
    let mut colour = None;
    for shape in &mark.shapes {
        let bounds = (shape.left, shape.top, shape.width, shape.height);
        if in_round_rect(x, y, bounds, shape.round) {
            colour = Some(shape.colour);
        }
    }
    if let Some(dot) = badge.colour() {
        let [cx, cy, r, cleared] = [DOT.0, DOT.1, DOT.2, DOT.3].map(|n| n * mark.side / 64.0);
        let d = (x - cx).powi(2) + (y - cy).powi(2);
        if d <= cleared * cleared {
            colour = None;
        }
        if d <= r * r {
            colour = Some(dot);
        }
    }
    colour
}

/// The icon as `size`×`size` pixels of red, green, blue and (straight)
/// alpha, top row first.
pub fn rgba(size: usize, badge: Badge) -> Vec<u8> {
    let mark = mark();
    let step = mark.side / (size * SAMPLES) as f32;
    let mut out = Vec::with_capacity(size * size * 4);
    for py in 0..size {
        for px in 0..size {
            let (mut sum, mut covered) = ([0u32; 3], 0u32);
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = ((px * SAMPLES + sx) as f32 + 0.5) * step;
                    let y = ((py * SAMPLES + sy) as f32 + 0.5) * step;
                    if let Some(colour) = at(mark, x, y, badge) {
                        covered += 1;
                        for (total, part) in sum.iter_mut().zip(colour) {
                            *total += u32::from(part);
                        }
                    }
                }
            }
            let all = (SAMPLES * SAMPLES) as u32;
            out.extend(sum.map(|total| (total / covered.max(1)) as u8));
            out.push((covered * 255 / all) as u8);
        }
    }
    out
}

/// The icon as the bits of an icon resource (`CreateIconFromResourceEx`):
/// a bitmap header of twice the height, the pixels as blue, green, red and
/// alpha from the bottom row up, then the one-bit mask, which the alpha
/// makes all zeros.
pub fn resource(size: usize, badge: Badge) -> Vec<u8> {
    let pixels = rgba(size, badge);
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::with_capacity(40 + pixels.len() + mask_row * size);
    let word = |n: usize| (n as u32).to_le_bytes();
    out.extend(word(40));
    out.extend(word(size));
    out.extend(word(size * 2));
    out.extend(1u16.to_le_bytes());
    out.extend(32u16.to_le_bytes());
    out.extend(word(0));
    out.extend(word(pixels.len()));
    out.extend([0u8; 16]);
    for row in pixels.chunks_exact(size * 4).rev() {
        for pixel in row.as_chunks::<4>().0 {
            out.extend([pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    out.resize(out.len() + mask_row * size, 0);
    out
}

/// The icon as an `.ico` file holding it at each of `sizes` (sides of up to
/// 255 pixels): the file's directory, then per size the bits [`resource`]
/// makes, which is how such a file holds a bitmap.
pub fn ico(sizes: &[usize], badge: Badge) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|size| resource(*size, badge)).collect();
    let mut out = Vec::new();
    // NOTE: reserved, the kind (1: icons), how many
    for word in [0, 1, images.len() as u16] {
        out.extend(word.to_le_bytes());
    }
    let mut at = 6 + 16 * images.len();
    for (size, image) in sizes.iter().zip(&images) {
        // NOTE: width, height, no palette, reserved; then planes, bits per pixel, where
        out.extend([*size as u8, *size as u8, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((image.len() as u32).to_le_bytes());
        out.extend((at as u32).to_le_bytes());
        at += image.len();
    }
    out.extend(images.into_iter().flatten());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_read_from_the_repositorys_svg() {
        let mark = mark();
        assert_eq!(mark.side, 64.0);
        let tile = Shape {
            left: 0.0,
            top: 0.0,
            width: 64.0,
            height: 64.0,
            round: 14.0,
            colour: [0x1a, 0x1a, 0x19],
        };
        assert_eq!(mark.shapes.first(), Some(&tile));
        assert_eq!(mark.shapes.len(), 4, "the tile and three bars");
        // NOTE: only rectangles are drawn here. When the icon gets another shape, this says
        // so: draw that shape too, or the icon in the taskbar is no longer the icon
        let elements: Vec<&str> = SVG
            .split('<')
            .skip(1)
            .filter_map(|rest| rest.split([' ', '>', '\n', '\r']).next())
            .filter(|name| !name.starts_with(['/', '?', '!']))
            .collect();
        assert_eq!(
            elements,
            ["svg", "rect", "rect", "rect", "rect"],
            "what dms-plugin/icon.svg is made of"
        );
    }

    #[test]
    fn an_svgs_rectangles_are_read_as_written() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32">
            <rect width="32" height="32" fill="#000000"/>
            <rect
                x="4" y="6.5" rx="2" width="8"
                height="4" fill="#ff8000" />
            <rect x="1" y="1" width="2" height="2" fill="url(#gradient)" />
            <circle cx="5" cy="5" r="2" fill="#ffffff" />
        </svg>"##;
        let read = read(svg);
        assert_eq!(read.side, 32.0);
        assert_eq!(read.shapes.len(), 2, "what it cannot draw is left out");
        let bar = Shape {
            left: 4.0,
            top: 6.5,
            width: 8.0,
            height: 4.0,
            round: 2.0,
            colour: [0xff, 0x80, 0x00],
        };
        assert_eq!(read.shapes[1], bar);
        assert_eq!(
            read.shapes[0].round, 0.0,
            "square corners when it names none"
        );
    }

    #[test]
    fn an_ico_file_is_a_directory_and_the_resources_it_names() {
        let file = ico(&[16, 32], Badge::None);
        assert_eq!(file[..6], [0, 0, 1, 0, 2, 0]);
        let entry = |n: usize| &file[6 + 16 * n..6 + 16 * (n + 1)];
        let number = |bytes: &[u8]| u32::from_le_bytes(bytes.try_into().unwrap()) as usize;
        let mut end = 6 + 16 * 2;
        for (n, size) in [16usize, 32].into_iter().enumerate() {
            let (len, at) = (number(&entry(n)[8..12]), number(&entry(n)[12..16]));
            assert_eq!(entry(n)[..2], [size as u8, size as u8]);
            assert_eq!(entry(n)[4..8], [1, 0, 32, 0], "one plane, 32 bits");
            assert_eq!(at, end, "one image after the other");
            assert_eq!(file[at..at + len], resource(size, Badge::None));
            end = at + len;
        }
        assert_eq!(end, file.len());
    }

    fn pixel(image: &[u8], size: usize, x: usize, y: usize) -> [u8; 4] {
        let at = (y * size + x) * 4;
        image[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn the_tile_has_round_corners_and_the_bars_their_colours() {
        for size in [16, 20, 24, 32] {
            let image = rgba(size, Badge::None);
            assert_eq!(image.len(), size * size * 4);
            assert_eq!(pixel(&image, size, 0, 0)[3], 0, "a corner at {size}");
            assert_eq!(pixel(&image, size, size - 1, size - 1)[3], 0);
            let (x, top, middle, bottom) =
                (size * 20 / 64, size * 20 / 64, size / 2, size * 44 / 64);
            assert_eq!(pixel(&image, size, x, top), [0x39, 0x87, 0xe5, 255]);
            assert_eq!(pixel(&image, size, x, middle), [0xd9, 0x59, 0x26, 255]);
            assert_eq!(pixel(&image, size, x, bottom), [0x19, 0x9e, 0x70, 255]);
        }
    }

    #[test]
    fn a_badge_puts_its_dot_in_the_corner_and_leaves_the_rest() {
        let size = 32;
        let (plain, waiting) = (rgba(size, Badge::None), rgba(size, Badge::Waiting));
        let (x, y) = (size * 47 / 64, size * 47 / 64);
        assert_eq!(pixel(&plain, size, x, y), [0x1a, 0x1a, 0x19, 255]);
        assert_eq!(pixel(&waiting, size, x, y), [0xf2, 0xb0, 0x1e, 255]);
        assert_eq!(pixel(&plain, size, 8, 8), pixel(&waiting, size, 8, 8));
        assert_ne!(rgba(size, Badge::Trouble), rgba(size, Badge::Offline));
    }

    #[test]
    fn the_resource_is_a_header_the_pixels_bottom_up_and_a_mask() {
        let size = 16;
        let bits = resource(size, Badge::None);
        assert_eq!(bits.len(), 40 + size * size * 4 + size * 4);
        assert_eq!(bits[..4], 40u32.to_le_bytes());
        assert_eq!(bits[8..12], 32u32.to_le_bytes(), "twice the height");
        assert_eq!(bits[14..16], 32u16.to_le_bytes(), "bits per pixel");
        // NOTE: the top bar's blue, in the row 5 from the top: 10 from the bottom
        let at = 40 + (10 * size + 5) * 4;
        assert_eq!(bits[at..at + 4], [0xe5, 0x87, 0x39, 255]);
        assert!(bits[40 + size * size * 4..].iter().all(|b| *b == 0));
    }
}
