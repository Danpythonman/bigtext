use std::io::{self, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue, terminal,
};
use font8x8::UnicodeFonts;

/// Display a message in giant letters that fill the whole terminal.
///
/// The text is scaled so it spans the full terminal height. If it is then
/// wider than the terminal it is either truncated or scrolled horizontally.
/// Press q, Esc or Ctrl-C to quit.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// The message to display (read from stdin if omitted).
    message: Vec<String>,

    /// What to do when the text is wider than the terminal.
    #[arg(short, long, value_enum, default_value_t = Overflow::Scroll)]
    overflow: Overflow,

    /// Scroll speed in (approximate) letters per second.
    #[arg(short, long, default_value_t = 2.0, value_parser = positive_f64)]
    speed: f64,

    /// Character used to draw the letters.
    #[arg(short, long, default_value_t = '█', value_parser = drawable_char)]
    char: char,

    /// Horizontal stretch: how many columns wide a font pixel is, relative to
    /// how many rows tall it is. 2.0 gives roughly square pixels on most
    /// terminals, whose cells are about twice as tall as they are wide.
    #[arg(short, long, default_value_t = 2.0, value_parser = positive_f64)]
    aspect: f64,

    /// Blank columns between the end of the text and its next repetition when
    /// scrolling. Defaults to the terminal width (the text fully leaves the
    /// screen before coming back).
    #[arg(short, long)]
    gap: Option<usize>,

    /// Animation frames per second.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    fps: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Overflow {
    /// Cut the text off at the right edge of the terminal.
    Truncate,
    /// Scroll the text from right to left in a loop.
    Scroll,
}

fn positive_f64(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(v) if v.is_finite() && v > 0.0 => Ok(v),
        _ => Err(format!("`{s}` is not a positive number")),
    }
}

fn drawable_char(s: &str) -> Result<char, String> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !c.is_control() && !c.is_whitespace() => Ok(c),
        _ => Err(format!("`{s}` must be a single visible character")),
    }
}

// ---------------------------------------------------------------------------
// Bitmap construction
// ---------------------------------------------------------------------------

/// Width (in font pixels) of a space / glyph with no ink.
const SPACE_WIDTH: usize = 4;
/// Blank font pixels between adjacent glyphs.
const LETTER_SPACING: usize = 1;
/// Approximate width of a letter including spacing, used to convert the
/// letters-per-second speed into columns per second.
const NOMINAL_ADVANCE: f64 = 7.0;

/// A monochrome image of the whole message, `rows x cols` font pixels.
#[derive(Debug, Clone, PartialEq)]
struct Bitmap {
    rows: usize,
    cols: usize,
    /// Row-major, `rows * cols` entries.
    pixels: Vec<bool>,
}

impl Bitmap {
    fn get(&self, row: usize, col: usize) -> bool {
        self.pixels[row * self.cols + col]
    }
}

fn lookup_glyph(c: char) -> [u8; 8] {
    font8x8::BASIC_FONTS
        .get(c)
        .or_else(|| font8x8::LATIN_FONTS.get(c))
        .or_else(|| font8x8::GREEK_FONTS.get(c))
        .or_else(|| font8x8::BOX_FONTS.get(c))
        .or_else(|| font8x8::BLOCK_FONTS.get(c))
        .or_else(|| font8x8::MISC_FONTS.get(c))
        .unwrap_or_else(|| font8x8::BASIC_FONTS.get('?').unwrap())
}

/// In font8x8 each byte is one row, and bit 0 is the leftmost pixel.
fn glyph_pixel(glyph: &[u8; 8], row: usize, col: usize) -> bool {
    glyph[row] & (1 << col) != 0
}

/// Render `text` into a bitmap using a proportional version of the 8x8 font:
/// each glyph is trimmed to its inked columns, and the whole message is
/// trimmed to the rows that contain ink so the letters fill the full height.
fn build_bitmap(text: &str) -> Bitmap {
    let glyphs: Vec<[u8; 8]> = text.chars().map(lookup_glyph).collect();

    // Rows that contain ink anywhere in the message.
    let inked_rows: Vec<usize> = (0..8)
        .filter(|&r| glyphs.iter().any(|g| g[r] != 0))
        .collect();
    let (top, bottom) = match (inked_rows.first(), inked_rows.last()) {
        (Some(&t), Some(&b)) => (t, b),
        _ => (0, 7), // all blank: keep the full cell
    };
    let rows = bottom - top + 1;

    // Column range of each glyph: inked columns, or a fixed-width blank.
    // (glyph, inclusive inked column range or None if blank)
    type Span<'a> = (&'a [u8; 8], Option<(usize, usize)>);
    let spans: Vec<Span> = glyphs
        .iter()
        .map(|g| {
            let inked: Vec<usize> = (0..8)
                .filter(|&c| (0..8).any(|r| glyph_pixel(g, r, c)))
                .collect();
            (g, inked.first().map(|&l| (l, *inked.last().unwrap())))
        })
        .collect();

    let width_of = |span: &Option<(usize, usize)>| match span {
        Some((l, r)) => r - l + 1,
        None => SPACE_WIDTH,
    };
    let cols = spans.iter().map(|(_, s)| width_of(s)).sum::<usize>()
        + LETTER_SPACING * spans.len().saturating_sub(1);

    let mut pixels = vec![false; rows * cols];
    let mut x = 0;
    for (i, (g, span)) in spans.iter().enumerate() {
        if i > 0 {
            x += LETTER_SPACING;
        }
        if let Some((left, right)) = *span {
            for r in 0..rows {
                for c in left..=right {
                    if glyph_pixel(g, top + r, c) {
                        pixels[r * cols + x + c - left] = true;
                    }
                }
            }
        }
        x += width_of(span);
    }
    debug_assert_eq!(x, cols);

    Bitmap { rows, cols, pixels }
}

// ---------------------------------------------------------------------------
// Layout and rendering (pure, unit-tested)
// ---------------------------------------------------------------------------

/// How the bitmap maps onto a terminal of a given size.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Layout {
    term_w: usize,
    term_h: usize,
    /// Terminal columns per font pixel (always >= 1, uniform for all pixels).
    pixel_w: usize,
    /// Total width of the scaled text in terminal columns.
    text_w: usize,
}

impl Layout {
    fn new(bitmap: &Bitmap, term_w: usize, term_h: usize, aspect: f64) -> Self {
        // Vertically the text fills exactly `term_h` rows, so one font pixel
        // is term_h / rows rows tall (possibly fractional). Horizontally we
        // use a whole number of columns per pixel so every pixel column is
        // the same width.
        let pixel_h = term_h as f64 / bitmap.rows as f64;
        let pixel_w = ((pixel_h * aspect).round() as usize).max(1);
        Layout {
            term_w,
            term_h,
            pixel_w,
            text_w: bitmap.cols * pixel_w,
        }
    }

    fn overflows(&self) -> bool {
        self.text_w > self.term_w
    }
}

/// What part of the text to show.
#[derive(Debug, Clone, Copy, PartialEq)]
enum View {
    /// Text fits: center it horizontally.
    Centered,
    /// Text overflows: show columns 0..term_w and cut off the rest.
    Truncated,
    /// Text overflows: the text repeats every `period` columns
    /// (`text_w + gap`) and the left edge of the screen is at `offset`.
    Scrolled { offset: usize, period: usize },
}

/// Which bitmap row is shown on terminal row `y` (nearest-neighbour). Spreads
/// the remainder evenly so the text covers every row of the terminal.
fn bitmap_row(bitmap: &Bitmap, layout: &Layout, y: usize) -> usize {
    y * bitmap.rows / layout.term_h
}

/// Which text column (in scaled terminal columns, 0..text_w) is shown on
/// terminal column `x`, or None if that column is blank.
fn text_column(layout: &Layout, view: View, x: usize) -> Option<usize> {
    match view {
        View::Centered => {
            let pad = (layout.term_w.saturating_sub(layout.text_w)) / 2;
            x.checked_sub(pad).filter(|&t| t < layout.text_w)
        }
        View::Truncated => Some(x).filter(|&t| t < layout.text_w),
        View::Scrolled { offset, period } => {
            Some((offset + x) % period).filter(|&t| t < layout.text_w)
        }
    }
}

/// Render the full screen as `term_h` lines of exactly `term_w` characters.
fn render(bitmap: &Bitmap, layout: &Layout, view: View, ink: char) -> Vec<String> {
    // Column mapping is the same for every row, so compute it once.
    let cols: Vec<Option<usize>> = (0..layout.term_w)
        .map(|x| text_column(layout, view, x).map(|t| t / layout.pixel_w))
        .collect();

    (0..layout.term_h)
        .map(|y| {
            let row = bitmap_row(bitmap, layout, y);
            cols.iter()
                .map(|c| match c {
                    Some(c) if bitmap.get(row, *c) => ink,
                    _ => ' ',
                })
                .collect()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Terminal handling
// ---------------------------------------------------------------------------

/// Puts the terminal into full-screen mode and restores it on drop (including
/// when unwinding from a panic).
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = TerminalGuard;
        let mut out = io::stdout();
        queue!(
            out,
            terminal::EnterAlternateScreen,
            terminal::DisableLineWrap,
            cursor::Hide
        )?;
        out.flush()?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = io::stdout();
        let _ = queue!(
            out,
            cursor::Show,
            terminal::EnableLineWrap,
            terminal::LeaveAlternateScreen
        );
        let _ = out.flush();
        let _ = terminal::disable_raw_mode();
    }
}

fn draw(out: &mut impl Write, lines: &[String]) -> io::Result<()> {
    for (y, line) in lines.iter().enumerate() {
        queue!(out, cursor::MoveTo(0, y as u16))?;
        out.write_all(line.as_bytes())?;
    }
    out.flush()
}

fn is_quit(ev: &Event) -> bool {
    match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
        }
        _ => false,
    }
}

fn run(args: &Args, bitmap: &Bitmap) -> io::Result<()> {
    let _guard = TerminalGuard::enter()?;
    let mut out = io::BufWriter::new(io::stdout());
    let frame = Duration::from_secs_f64(1.0 / args.fps as f64);

    // Scroll position as a fraction of the period, so it survives resizes
    // (which change the scale and therefore the period in columns).
    let mut progress: f64 = 0.0;
    let mut last_tick = Instant::now();

    loop {
        let (w, h) = terminal::size()?;
        let layout = Layout::new(bitmap, w as usize, h as usize, args.aspect);
        let scrolling = args.overflow == Overflow::Scroll && layout.overflows();

        let view = if !layout.overflows() {
            View::Centered
        } else if scrolling {
            let period = layout.text_w + args.gap.unwrap_or(layout.term_w);
            View::Scrolled {
                offset: ((progress * period as f64) as usize).min(period - 1),
                period,
            }
        } else {
            View::Truncated
        };

        if layout.term_w > 0 && layout.term_h > 0 {
            draw(&mut out, &render(bitmap, &layout, view, args.char))?;
        }

        if scrolling {
            let View::Scrolled { period, .. } = view else {
                unreachable!()
            };
            // Wait until the next frame is due, handling input meanwhile.
            let deadline = last_tick + frame;
            while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                if event::poll(remaining)? {
                    let ev = event::read()?;
                    if is_quit(&ev) {
                        return Ok(());
                    }
                    if matches!(ev, Event::Resize(..)) {
                        break; // redraw immediately at the new size
                    }
                }
            }
            let now = Instant::now();
            let cols_per_sec = args.speed * NOMINAL_ADVANCE * layout.pixel_w as f64;
            progress += now.duration_since(last_tick).as_secs_f64() * cols_per_sec / period as f64;
            progress = progress.fract();
            last_tick = now;
        } else {
            // Static image: block until a key or resize.
            let ev = event::read()?;
            if is_quit(&ev) {
                return Ok(());
            }
            // Don't count time spent idle as scroll progress.
            last_tick = Instant::now();
        }
    }
}

fn read_message(args: &Args) -> io::Result<String> {
    let raw = if args.message.is_empty() {
        if io::stdin().is_terminal() {
            return Ok(String::new());
        }
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        s
    } else {
        args.message.join(" ")
    };
    // Collapse all whitespace (newlines, tabs, runs of spaces) to single
    // spaces and drop other control characters.
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect();
    Ok(cleaned.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn main() {
    let args = Args::parse();

    let message = match read_message(&args) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("bigtext: failed to read stdin: {e}");
            std::process::exit(1);
        }
    };
    if message.is_empty() {
        eprintln!("bigtext: no message given (pass it as an argument or pipe it on stdin)");
        std::process::exit(2);
    }
    if !io::stdout().is_terminal() {
        eprintln!("bigtext: stdout is not a terminal");
        std::process::exit(1);
    }

    let bitmap = build_bitmap(&message);
    if let Err(e) = run(&args, &bitmap) {
        eprintln!("bigtext: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap_to_strings(b: &Bitmap) -> Vec<String> {
        (0..b.rows)
            .map(|r| {
                (0..b.cols)
                    .map(|c| if b.get(r, c) { '#' } else { '.' })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn font_bit_order_is_lsb_left() {
        // 'L' has its vertical stroke on the left, a full-width foot at the
        // bottom, and nothing in the top-right corner.
        let b = build_bitmap("L");
        let s = bitmap_to_strings(&b);
        assert!(s.iter().all(|row| row[..3].contains('#')), "{s:#?}");
        assert!(s.last().unwrap().chars().all(|c| c == '#'), "{s:#?}");
        assert!(s[..3].iter().all(|row| row.ends_with('.')), "{s:#?}");
    }

    #[test]
    fn bitmap_is_trimmed_and_spaced() {
        let b = build_bitmap("I");
        // Every edge of a trimmed single glyph has ink.
        assert!((0..b.cols).any(|c| b.get(0, c)));
        assert!((0..b.cols).any(|c| b.get(b.rows - 1, c)));
        assert!((0..b.rows).any(|r| b.get(r, 0)));
        assert!((0..b.rows).any(|r| b.get(r, b.cols - 1)));

        let one = build_bitmap("I").cols;
        assert_eq!(build_bitmap("II").cols, 2 * one + LETTER_SPACING);
        assert_eq!(
            build_bitmap("I I").cols,
            2 * one + SPACE_WIDTH + 2 * LETTER_SPACING
        );
    }

    #[test]
    fn descenders_extend_height() {
        assert!(build_bitmap("Hg").rows > build_bitmap("H").rows);
    }

    #[test]
    fn blank_message_does_not_panic() {
        let b = build_bitmap(" ");
        assert_eq!(b.rows, 8);
        assert_eq!(b.cols, SPACE_WIDTH);
        let l = Layout::new(&b, 10, 5, 2.0);
        let lines = render(&b, &l, View::Truncated, '#');
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn fills_every_row_exactly() {
        let b = build_bitmap("H"); // H has ink on its top and bottom rows
        for h in 1..100 {
            let l = Layout::new(&b, 1000, h, 2.0);
            let lines = render(&b, &l, View::Centered, '#');
            assert_eq!(lines.len(), h);
            assert!(lines.iter().all(|s| s.chars().count() == 1000));
            if h >= b.rows {
                // Every bitmap row appears, the first on row 0 and the last
                // on the final row, and each gets floor or ceil of h/rows.
                assert_eq!(bitmap_row(&b, &l, 0), 0);
                assert_eq!(bitmap_row(&b, &l, h - 1), b.rows - 1);
                let mut counts = vec![0; b.rows];
                for y in 0..h {
                    counts[bitmap_row(&b, &l, y)] += 1;
                }
                let (lo, hi) = (h / b.rows, h.div_ceil(b.rows));
                assert!(
                    counts.iter().all(|&c| c == lo || c == hi),
                    "h={h} {counts:?}"
                );
                // Both the top and bottom terminal rows contain ink.
                assert!(lines[0].contains('#') && lines[h - 1].contains('#'));
            }
        }
    }

    #[test]
    fn pixel_width_scales_with_height_and_aspect() {
        let b = build_bitmap("A");
        let l = Layout::new(&b, 80, b.rows * 3, 2.0);
        assert_eq!(l.pixel_w, 6);
        assert_eq!(l.text_w, b.cols * 6);
        let l = Layout::new(&b, 80, 1, 0.1);
        assert_eq!(l.pixel_w, 1); // never zero
    }

    #[test]
    fn centered_when_fits() {
        let b = build_bitmap("I");
        let l = Layout::new(&b, 50, b.rows, 1.0);
        assert!(!l.overflows());
        let pad = (50 - l.text_w) / 2;
        assert_eq!(text_column(&l, View::Centered, pad - 1), None);
        assert_eq!(text_column(&l, View::Centered, pad), Some(0));
        assert_eq!(
            text_column(&l, View::Centered, pad + l.text_w - 1),
            Some(l.text_w - 1)
        );
        assert_eq!(text_column(&l, View::Centered, pad + l.text_w), None);
    }

    #[test]
    fn exact_fit_is_not_overflow() {
        let b = build_bitmap("HI");
        let l = Layout::new(&b, 1, b.rows, 1.0);
        let exact = Layout {
            term_w: l.text_w,
            ..l
        };
        assert!(!exact.overflows());
        assert!(
            Layout {
                term_w: l.text_w - 1,
                ..l
            }
            .overflows()
        );
        // Exact fit uses every column with no padding.
        assert_eq!(text_column(&exact, View::Centered, 0), Some(0));
    }

    #[test]
    fn truncate_shows_prefix() {
        let b = build_bitmap("HELLO WORLD");
        let l = Layout::new(&b, 40, 24, 2.0);
        assert!(l.overflows());
        let lines = render(&b, &l, View::Truncated, '#');
        let full = render(
            &b,
            &Layout {
                term_w: l.text_w,
                ..l
            },
            View::Truncated,
            '#',
        );
        for (t, f) in lines.iter().zip(&full) {
            assert_eq!(t.chars().count(), 40);
            assert_eq!(*t, f.chars().take(40).collect::<String>());
        }
    }

    #[test]
    fn scroll_wraps_with_gap() {
        let b = build_bitmap("HELLO");
        let l = Layout::new(&b, 30, 16, 2.0);
        assert!(l.overflows());
        let gap = 7;
        let period = l.text_w + gap;
        let view = |offset| View::Scrolled { offset, period };

        // offset 0 matches truncation.
        for x in 0..l.term_w {
            assert_eq!(
                text_column(&l, view(0), x),
                text_column(&l, View::Truncated, x)
            );
        }
        // Just past the end of the text there are exactly `gap` blanks, then
        // the text starts again from column 0.
        let off = l.text_w - 1;
        assert_eq!(text_column(&l, view(off), 0), Some(l.text_w - 1));
        for x in 1..=gap {
            assert_eq!(text_column(&l, view(off), x), None);
        }
        assert_eq!(text_column(&l, view(off), gap + 1), Some(0));
        // Shifting the offset by one shifts the image left by one column.
        for off in 0..period {
            for x in 1..l.term_w {
                assert_eq!(
                    text_column(&l, view(off), x),
                    text_column(&l, view((off + 1) % period), x - 1)
                );
            }
        }
    }

    #[test]
    fn rendered_lines_have_exact_width() {
        let b = build_bitmap("Ünïcödé λ ░");
        for (w, h) in [(1, 1), (7, 3), (80, 24), (233, 61)] {
            let l = Layout::new(&b, w, h, 2.0);
            for view in [
                View::Centered,
                View::Truncated,
                View::Scrolled {
                    offset: 5,
                    period: l.text_w + w,
                },
            ] {
                let lines = render(&b, &l, view, '█');
                assert_eq!(lines.len(), h);
                assert!(lines.iter().all(|s| s.chars().count() == w));
            }
        }
    }
}
