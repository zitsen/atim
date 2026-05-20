use ab_glyph::{FontArc, PxScale};
use image::ImageEncoder;
use image::RgbaImage;
use regex::Regex;

use atim_core::error::{Error, Result};

/// Default font paths (Latin monospace).
const LATIN_FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
    "/usr/share/fonts/Adwaita/AdwaitaMono-Regular.ttf",
];

/// CJK fallback font paths.
const CJK_FONT_PATHS: &[&str] = &[
    "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-SC-Regular.otf",
];

// ── ANSI color mapping ──

const ANSI_COLORS: [(u8, u8, u8); 16] = [
    (0, 0, 0),       // 0 black
    (205, 49, 49),   // 1 red
    (13, 188, 121),  // 2 green
    (229, 229, 16),  // 3 yellow
    (36, 114, 200),  // 4 blue
    (188, 63, 188),  // 5 magenta
    (17, 168, 205),  // 6 cyan
    (229, 229, 229), // 7 white
    (102, 102, 102), // 8 bright black
    (241, 76, 76),   // 9 bright red
    (35, 209, 139),  // 10 bright green
    (245, 245, 67),  // 11 bright yellow
    (59, 142, 234),  // 12 bright blue
    (214, 112, 214), // 13 bright magenta
    (41, 184, 219),  // 14 bright cyan
    (255, 255, 255), // 15 bright white
];

const DEFAULT_FG: (u8, u8, u8) = (212, 212, 212);
const DEFAULT_BG: (u8, u8, u8) = (30, 30, 30);

fn ansi_256_to_rgb(idx: u8) -> (u8, u8, u8) {
    if idx < 16 {
        ANSI_COLORS[idx as usize]
    } else if idx < 232 {
        let i = idx - 16;
        let r = (i / 36) % 6;
        let g = (i / 6) % 6;
        let b = i % 6;
        let expand = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        (expand(r), expand(g), expand(b))
    } else {
        let v = 8 + (idx - 232) * 10;
        (v, v, v)
    }
}

// ── ANSI SGR parser ──

#[derive(Clone, Copy, Debug, PartialEq)]
struct Style {
    fg: Option<(u8, u8, u8)>,
    bg: Option<(u8, u8, u8)>,
}

impl Style {
    fn fg_rgb(&self) -> (u8, u8, u8) {
        self.fg.unwrap_or(DEFAULT_FG)
    }
    fn bg_rgb(&self) -> Option<(u8, u8, u8)> {
        self.bg
    }
}

struct Segment {
    text: String,
    style: Style,
    has_cjk: bool,
}

/// Parse a line with ANSI SGR codes into styled segments.
fn parse_ansi_line(line: &str, re: &Regex) -> Vec<Segment> {
    let mut segments: Vec<Segment> = Vec::new();
    let mut style = Style { fg: None, bg: None };
    let mut pos = 0;

    for cap in re.find_iter(line) {
        // Text before this escape
        if cap.start() > pos {
            let text = &line[pos..cap.start()];
            push_segment(&mut segments, text, style);
        }

        // Parse the SGR parameters
        let params = cap.as_str();
        let codes: Vec<u8> = params[2..params.len() - 1]
            .split(';')
            .filter_map(|s| s.parse().ok())
            .collect();

        style = apply_sgr(style, &codes);
        pos = cap.end();
    }

    // Remaining text after last escape
    if pos < line.len() {
        push_segment(&mut segments, &line[pos..], style);
    }

    if segments.is_empty() {
        segments.push(Segment {
            text: String::new(),
            style,
            has_cjk: false,
        });
    }
    segments
}

fn push_segment(segments: &mut Vec<Segment>, text: &str, style: Style) {
    if text.is_empty() {
        return;
    }
    // Merge with previous segment if same style
    if let Some(last) = segments.last_mut()
        && last.style == style {
            let has_cjk = text.chars().any(is_cjk);
            last.has_cjk = last.has_cjk || has_cjk;
            last.text.push_str(text);
            return;
        }
    segments.push(Segment {
        text: text.to_string(),
        style,
        has_cjk: text.chars().any(is_cjk),
    });
}

fn apply_sgr(mut style: Style, codes: &[u8]) -> Style {
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => {
                style = Style { fg: None, bg: None };
            }
            30..=37 => {
                style.fg = Some(ANSI_COLORS[(codes[i] - 30) as usize]);
            }
            38 => {
                if i + 1 < codes.len() && codes[i + 1] == 5 && i + 2 < codes.len() {
                    style.fg = Some(ansi_256_to_rgb(codes[i + 2]));
                    i += 2;
                } else if i + 1 < codes.len() && codes[i + 1] == 2 && i + 4 < codes.len() {
                    style.fg = Some((codes[i + 2], codes[i + 3], codes[i + 4]));
                    i += 4;
                }
            }
            39 => {
                style.fg = None;
            }
            40..=47 => {
                style.bg = Some(ANSI_COLORS[(codes[i] - 40) as usize]);
            }
            48 => {
                if i + 1 < codes.len() && codes[i + 1] == 5 && i + 2 < codes.len() {
                    style.bg = Some(ansi_256_to_rgb(codes[i + 2]));
                    i += 2;
                } else if i + 1 < codes.len() && codes[i + 1] == 2 && i + 4 < codes.len() {
                    style.bg = Some((codes[i + 2], codes[i + 3], codes[i + 4]));
                    i += 4;
                }
            }
            49 => {
                style.bg = None;
            }
            90..=97 => {
                style.fg = Some(ANSI_COLORS[(codes[i] - 90 + 8) as usize]);
            }
            100..=107 => {
                style.bg = Some(ANSI_COLORS[(codes[i] - 100 + 8) as usize]);
            }
            _ => {}
        }
        i += 1;
    }
    style
}

// ── Font / CJK helpers ──

/// Attempt to load a font from a list of candidate paths.
fn load_font(candidates: &[&str]) -> Result<FontArc> {
    for path in candidates {
        if let Ok(data) = std::fs::read(path)
            && let Ok(font) = FontArc::try_from_vec(data) {
                return Ok(font);
            }
    }
    Err(Error::Font("no suitable font found".into()))
}

/// Attempt to load an optional font (returns None if not found).
fn load_font_opt(candidates: &[&str]) -> Option<FontArc> {
    candidates
        .iter()
        .find_map(|p| std::fs::read(p).ok())
        .and_then(|d| FontArc::try_from_vec(d).ok())
}

/// Check if a character likely needs a CJK font.
fn is_cjk(ch: char) -> bool {
    match ch as u32 {
        0x2E80..=0x2EFF
        | 0x2F00..=0x2FDF
        | 0x3000..=0x303F
        | 0x3040..=0x309F
        | 0x30A0..=0x30FF
        | 0x3100..=0x312F
        | 0x3130..=0x318F
        | 0x3190..=0x319F
        | 0x31A0..=0x31EF
        | 0x3200..=0x32FF
        | 0x3300..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFFEF
        | 0x20000..=0x2A6DF
        | 0x2A700..=0x2B73F
        | 0x2B740..=0x2B81F
        | 0x2B820..=0x2CEAF
        | 0x2CEB0..=0x2EBEF
        | 0x30000..=0x3134F
        | 0x31350..=0x323AF
        | 0xAC00..=0xD7AF => true,
        _ => false,
    }
}

// ── Main renderer ──

/// Render ANSI-colored terminal text to a PNG image (ccbot-style).
///
/// Splits the text by line, parses ANSI SGR codes per line,
/// and draws each styled segment using monospace font metrics.
pub fn render_ansi_to_png(contents: &str) -> Result<Vec<u8>> {
    let font = load_font(LATIN_FONT_PATHS).map_err(|e| {
        tracing::warn!("Failed to load Latin font: {e}");
        e
    })?;
    let cjk_font = load_font_opt(CJK_FONT_PATHS);
    if cjk_font.is_none() {
        tracing::warn!("No CJK font found — CJK chars use Latin fallback");
    }

    let font_size = 16.0f32;
    let scale = PxScale {
        x: font_size,
        y: font_size,
    };
    let char_w = imageproc::drawing::text_size(scale, &font, "W").0 as u32;
    let line_h = (font_size * 1.35).ceil() as u32;
    let padding = 16u32;

    // Parse lines
    let ansi_re = Regex::new(r"\x1b\[([0-9;]*)m").unwrap();
    let mut lines: Vec<Vec<Segment>> = contents
        .lines()
        .map(|l| parse_ansi_line(l, &ansi_re))
        .collect();

    // Trim trailing whitespace from each line to avoid wasted space
    for segs in &mut lines {
        loop {
            let is_empty = segs
                .last()
                .map(|s| s.text.trim_end().is_empty())
                .unwrap_or(false);
            if is_empty {
                segs.pop();
            } else {
                break;
            }
        }
        // Trim the last non-empty segment
        if let Some(last) = segs.last_mut() {
            let trimmed = last.text.trim_end().to_string();
            last.text = trimmed;
        }
    }

    // Calculate max visible line width (approximate CJK as 2 Latin chars)
    let max_chars = lines
        .iter()
        .map(|segs| {
            segs.iter()
                .map(|s| {
                    let cjk = s.text.chars().filter(|c| is_cjk(*c)).count();
                    s.text.chars().count() + cjk // add extra width for CJK
                })
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    if max_chars == 0 || lines.is_empty() {
        let img = RgbaImage::from_pixel(100, 100, image::Rgba([30, 30, 30, 255]));
        let mut png = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut png);
        encoder
            .write_image(img.as_raw(), 100, 100, image::ExtendedColorType::Rgba8)
            .map_err(|e| Error::PngEncoding(e.to_string()))?;
        return Ok(png);
    }

    let mut img_w = max_chars as u32 * char_w + padding * 2;
    let mut img_h = lines.len() as u32 * line_h + padding * 2;
    if img_w < 100 {
        img_w = 100;
    }
    if img_h < 100 {
        img_h = 100;
    }

    let mut img = RgbaImage::from_pixel(
        img_w,
        img_h,
        image::Rgba([DEFAULT_BG.0, DEFAULT_BG.1, DEFAULT_BG.2, 255]),
    );

    let mut y = padding as i32;
    for line_segs in &lines {
        let mut x = padding as i32;
        for seg in line_segs {
            let seg_chars = seg.text.chars().count();
            // CJK chars are ~2x width in terminal; approximate by treating as 2 Latin chars
            let cjk_count = if seg.has_cjk {
                seg.text.chars().filter(|c| is_cjk(*c)).count()
            } else {
                0
            };
            let latin_count = seg_chars - cjk_count;
            let seg_w = (latin_count as u32 + cjk_count as u32 * 2) * char_w;

            // Draw background
            if let Some(bg) = seg.style.bg_rgb() {
                for dy in y..y + line_h as i32 {
                    for dx in x..x + seg_w as i32 {
                        if dx >= 0 && dy >= 0 && dx < img.width() as i32 && dy < img.height() as i32
                        {
                            img.put_pixel(
                                dx as u32,
                                dy as u32,
                                image::Rgba([bg.0, bg.1, bg.2, 255]),
                            );
                        }
                    }
                }
            }

            // Draw text
            let fg = seg.style.fg_rgb();
            let rgba = image::Rgba([fg.0, fg.1, fg.2, 255]);
            let f = cjk_font.as_ref().unwrap_or(&font);
            imageproc::drawing::draw_text_mut(&mut img, rgba, x, y, scale, f, &seg.text);

            x += seg_w as i32;
        }
        y += line_h as i32;
    }

    // Only resize if Telegram constraints require it (w+h <= 10000, ratio <= 20).
    // Otherwise keep native resolution for maximum sharpness.
    let telegram_ok =
        img_w + img_h <= 10000 && img_w.max(img_h) as f64 / img_w.min(img_h).max(1) as f64 <= 20.0;
    let (resized_w, resized_h) = if telegram_ok {
        (img_w, img_h)
    } else {
        fit_to_bounds(img_w, img_h, 5000, 5000)
    };
    let resized = if resized_w != img_w || resized_h != img_h {
        image::imageops::resize(
            &img,
            resized_w,
            resized_h,
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        img
    };

    let mut png = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut png);
    encoder
        .write_image(
            resized.as_raw(),
            resized_w,
            resized_h,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| Error::PngEncoding(e.to_string()))?;

    Ok(png)
}

/// Calculate dimensions that satisfy Telegram photo constraints:
/// width + height <= 10000, aspect ratio (max/min) <= 20
fn fit_to_bounds(w: u32, h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    let ratio_ok = |nw: u32, nh: u32| nw.max(nh) as f64 / nw.min(nh).max(1) as f64 <= 20.0;
    let mut scale = (max_w as f64 / w as f64)
        .min(max_h as f64 / h as f64)
        .min(1.0);
    while scale > 0.01 {
        let nw = (w as f64 * scale).round() as u32;
        let nh = (h as f64 * scale).round() as u32;
        if nw + nh <= 10000 && ratio_ok(nw, nh) {
            return (nw, nh);
        }
        scale -= 0.01;
    }
    (1, 1)
}

/// Fill a rectangular region of an image with a color.
#[allow(dead_code)]
pub fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: image::Rgba<u8>) {
    for dy in y..y + h {
        for dx in x..x + w {
            if dx < img.width() && dy < img.height() {
                img.put_pixel(dx, dy, color);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sgr_reset() {
        let style = apply_sgr(
            Style {
                fg: Some((1, 2, 3)),
                bg: Some((4, 5, 6)),
            },
            &[0],
        );
        assert_eq!(style.fg, None);
        assert_eq!(style.bg, None);
    }

    #[test]
    fn test_sgr_fg_bg() {
        let style = apply_sgr(Style { fg: None, bg: None }, &[31, 41]);
        assert_eq!(style.fg, Some(ANSI_COLORS[1]));
        assert_eq!(style.bg, Some(ANSI_COLORS[1]));
    }

    #[test]
    fn test_parse_ansi_line_simple() {
        let re = Regex::new(r"\x1b\[([0-9;]*)m").unwrap();
        let segs = parse_ansi_line("\x1b[31mred\x1b[0mnormal", &re);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "red");
        assert_eq!(segs[0].style.fg, Some(ANSI_COLORS[1]));
        assert_eq!(segs[1].text, "normal");
        assert_eq!(segs[1].style.fg, None);
    }

    #[test]
    fn test_parse_no_ansi() {
        let re = Regex::new(r"\x1b\[([0-9;]*)m").unwrap();
        let segs = parse_ansi_line("hello world", &re);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].text, "hello world");
    }

    #[test]
    fn test_render_simple_text() {
        let result = render_ansi_to_png("Hello, World!\nThis is a test.\n");
        assert!(result.is_ok(), "PNG rendering failed: {:?}", result.err());
        let png = result.unwrap();
        assert!(!png.is_empty(), "PNG should not be empty");
        assert_eq!(
            &png[..8],
            &[137, 80, 78, 71, 13, 10, 26, 10],
            "Not a valid PNG"
        );
    }

    #[test]
    fn test_render_with_colors() {
        let result = render_ansi_to_png("\x1b[31mRed text\x1b[0m\n");
        assert!(result.is_ok());
        let png = result.unwrap();
        assert!(!png.is_empty());
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    #[test]
    fn test_render_empty_input() {
        let result = render_ansi_to_png("");
        assert!(result.is_ok());
        let png = result.unwrap();
        assert!(!png.is_empty());
    }

    #[test]
    fn test_render_cjk_text() {
        let result = render_ansi_to_png("Hello 世界\n");
        assert!(result.is_ok());
        let png = result.unwrap();
        assert_eq!(&png[..8], &[137, 80, 78, 71, 13, 10, 26, 10]);
    }

    #[test]
    fn test_is_cjk() {
        assert!(is_cjk('中'));
        assert!(is_cjk('文'));
        assert!(is_cjk('あ'));
        assert!(is_cjk('ア'));
        assert!(is_cjk('한'));
        assert!(!is_cjk('a'));
        assert!(!is_cjk('1'));
        assert!(!is_cjk(' '));
        assert!(!is_cjk('{'));
    }
}
