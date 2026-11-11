// SPDX-License-Identifier: EUPL-1.2
//! LaTeX math in messages, rendered for the sandboxed math worker; never run
//! this in the UI process.
//!
//! [`latex::to_typst`] turns LaTeX math into Typst math markup that cannot
//! contain code, and [`Renderer`] lays it out with Typst and rasterises it.
//! Typst runs in a world without files, packages, a clock or system fonts:
//! the one font is New Computer Modern Math, compiled in, and the standard
//! library lacks everything that reads data (see [`restricted_library`]).
//! The result is a coverage mask the interface tints with its text colour,
//! so a theme change needs no new rendering. See docs/math.md.

pub mod latex;

use typst::{
    Library, LibraryExt, World,
    diag::{FileError, FileResult},
    foundations::{Binding, Bytes, Datetime, Duration, Module, Scope},
    introspection::Tag,
    layout::{Abs, Frame, FrameItem, Point},
    syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot},
    text::{Font, FontBook},
    utils::{LazyHash, Scalar},
};
use typst_layout::PagedDocument;

/// New Computer Modern Math (GUST Font License, see fonts/README.md).
static MATH_FONT: &[u8] = include_bytes!("../fonts/NewCMMath-Regular.otf");

/// The largest image rendered, in device pixels: wider or taller formulas
/// are refused rather than drawn (an interface at 300 % still fits a long
/// display formula).
pub const MAX_WIDTH: u32 = 4096;
pub const MAX_HEIGHT: u32 = 1024;
pub const MAX_PIXELS: u32 = 2 * 1024 * 1024;
/// Device pixels per em: from a tiny text at 50 % to a large one at 300 %.
pub const SIZES: std::ops::RangeInclusive<f32> = 4.0..=128.0;

/// The size the template sets, in points; the raster scale follows from it.
const TEXT_PT: f64 = 10.0;
/// Room around the ink, so antialiasing at the edges is not cut off.
const MARGIN_PT: f64 = 1.0;

/// A rendered formula: one byte of coverage per pixel, row by row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Formula {
    pub width: u32,
    pub height: u32,
    /// The text baseline, in pixels from the top.
    pub baseline: u32,
    pub coverage: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The LaTeX was not understood; the reason is for a tooltip.
    Latex(String),
    /// Typst rejected the converted formula.
    Typst(String),
    /// The formula is too large to show.
    TooLarge,
    /// A size outside [`SIZES`].
    Size,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Latex(reason) => write!(f, "{reason}"),
            Error::Typst(reason) => write!(f, "{reason}"),
            Error::TooLarge => f.write_str("too large to show"),
            Error::Size => f.write_str("unsupported size"),
        }
    }
}

/// Typst's library, fonts and the main file id, set up once per worker.
pub struct Renderer {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: FileId,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

/// Global definitions that read files or turn bytes into documents, data or
/// running code. Math markup from [`latex`] cannot reach code at all; this
/// is the second line, should it ever.
const REMOVED: &[&str] = &[
    "read",
    "plugin",
    "image",
    "json",
    "yaml",
    "toml",
    "csv",
    "cbor",
    "xml",
    "bibliography",
    "cite",
    "eval",
    "sys",
    "pdf",
    "html",
    "std",
];

/// Typst's standard library without [`REMOVED`] (also in `std`, which is
/// rebuilt from the rest).
pub fn restricted_library() -> Library {
    let mut library = Library::default();
    let mut scope = Scope::new();
    for (name, binding) in library.global.scope().iter() {
        if !REMOVED.contains(&name.as_str()) {
            scope.bind(name.clone(), binding.clone());
        }
    }
    let global = Module::new("global", scope);
    library.global = global.clone();
    library.std = Binding::detached(global);
    library
}

impl Renderer {
    /// Also limits Typst's thread pool to one thread: a formula is small,
    /// and a hostile one should not occupy every core until its deadline.
    pub fn new() -> Self {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global();
        let fonts: Vec<Font> = Font::iter(Bytes::new(MATH_FONT)).collect();
        let main = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new("/formula.typ").expect("a valid constant path"),
        )
        .intern();
        Self {
            library: LazyHash::new(restricted_library()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            main,
        }
    }

    /// Render LaTeX math (`tex`, without delimiters) at `size` device pixels
    /// per em, inline or as a display formula.
    pub fn render(&self, tex: &str, display: bool, size: f32) -> Result<Formula, Error> {
        if !SIZES.contains(&size) {
            return Err(Error::Size);
        }
        let typst = latex::to_typst(tex).map_err(|error| Error::Latex(error.0))?;
        let result = self.compile(&typst, display);
        // Memoized results of earlier formulas would only accumulate.
        comemo::evict(8);
        let document = result?;
        let page = document.pages().first().ok_or(Error::TooLarge)?;
        let pixel_per_pt = f64::from(size) / TEXT_PT;
        let pixels = |length: Abs| (length.to_pt() * pixel_per_pt).round();
        let (width, height) = (pixels(page.frame.width()), pixels(page.frame.height()));
        if !(1.0..=f64::from(MAX_WIDTH)).contains(&width)
            || !(1.0..=f64::from(MAX_HEIGHT)).contains(&height)
            || width * height > f64::from(MAX_PIXELS)
        {
            return Err(Error::TooLarge);
        }
        let baseline = equation_baseline(&page.frame, Point::zero())
            .unwrap_or(page.frame.height() - Abs::pt(MARGIN_PT));
        let options = typst_render::RenderOptions {
            pixel_per_pt: Scalar::new(pixel_per_pt),
            render_bleed: false,
        };
        let pixmap = typst_render::render(page, &options);
        let coverage = pixmap.data().iter().skip(3).step_by(4).copied().collect();
        Ok(Formula {
            width: pixmap.width(),
            height: pixmap.height(),
            baseline: (pixels(baseline) as u32).min(pixmap.height()),
            coverage,
        })
    }

    fn compile(&self, math: &str, display: bool) -> Result<PagedDocument, Error> {
        // The text is set in the math font: it is the only one.
        let (open, close) = if display { ("$ ", " $") } else { ("$", "$") };
        let text = format!(
            "#set page(width: auto, height: auto, margin: {MARGIN_PT}pt, fill: none)\n\
             #set text(font: \"New Computer Modern Math\", size: {TEXT_PT}pt, \
             top-edge: \"bounds\", bottom-edge: \"bounds\", fallback: false)\n\
             {open}{math}{close}"
        );
        let job = Job {
            renderer: self,
            source: Source::new(self.main, text),
        };
        let compiled = typst::compile::<PagedDocument>(&job);
        compiled.output.map_err(|errors| {
            let message = errors
                .first()
                .map_or_else(|| "Typst failed".into(), |error| error.message.to_string());
            Error::Typst(message.chars().take(200).collect())
        })
    }
}

/// Where the equation starts, which is on its baseline.
fn equation_baseline(frame: &Frame, origin: Point) -> Option<Abs> {
    for (position, item) in frame.items() {
        match item {
            FrameItem::Tag(Tag::Start(content, _)) if content.elem().name() == "equation" => {
                return Some(origin.y + position.y);
            }
            FrameItem::Group(group) => {
                if let Some(baseline) = equation_baseline(&group.frame, origin + *position) {
                    return Some(baseline);
                }
            }
            _ => {}
        }
    }
    None
}

/// One compilation: the formula is the only file.
struct Job<'a> {
    renderer: &'a Renderer,
    source: Source,
}

impl World for Job<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.renderer.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.renderer.book
    }

    fn main(&self) -> FileId {
        self.renderer.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.renderer.main {
            Ok(self.source.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }

    fn file(&self, _: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.renderer.fonts.get(index).cloned()
    }

    fn today(&self, _: Option<Duration>) -> Option<Datetime> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Error, Renderer};

    #[test]
    fn renders_inline_and_display_formulas_on_their_baseline() {
        let renderer = Renderer::new();
        let x = renderer.render("x", false, 20.).unwrap();
        assert_eq!(x.coverage.len(), (x.width * x.height) as usize);
        assert!(x.coverage.iter().any(|&alpha| alpha > 200));
        // An x sits on the baseline: nothing below it but the margin.
        assert!(x.baseline > x.height / 2 && x.baseline < x.height);
        let fraction = renderer.render("\\frac{a}{b}", false, 20.).unwrap();
        assert!(fraction.height > x.height);
        // The fraction bar is on the math axis, above the baseline.
        assert!(fraction.baseline > fraction.height / 3);
        let sum = renderer.render("\\sum_{i=1}^n i", true, 20.).unwrap();
        let inline = renderer.render("\\sum_{i=1}^n i", false, 20.).unwrap();
        assert!(
            sum.height > inline.height,
            "display style puts limits above and below"
        );
        // Twice the size, twice the pixels.
        let large = renderer.render("x", false, 40.).unwrap();
        assert!(large.width >= 2 * x.width - 2 && large.width <= 2 * x.width + 2);
    }

    /// Writes PNGs of sample formulas to look at:
    /// `NUSCHL_MATH_SAMPLES=dir cargo test -p nuschl-math samples`.
    #[test]
    fn samples() {
        let Some(directory) = std::env::var_os("NUSCHL_MATH_SAMPLES") else {
            return;
        };
        let renderer = Renderer::new();
        for (index, (tex, display)) in [
            ("x", false),
            ("x^2 + y^2 = z^2", false),
            ("\\frac{a}{b} + \\sqrt[3]{x}", false),
            ("\\sum_{i=1}^n i = \\frac{n(n+1)}{2}", true),
            (
                "\\int_0^\\infty e^{-x^2}\\,dx = \\frac{\\sqrt{\\pi}}{2}",
                true,
            ),
            ("\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}", true),
            (
                "f(x) = \\begin{cases} 1 & x > 0 \\\\ 0 & \\text{otherwise} \\end{cases}",
                true,
            ),
            (
                "\\left( \\frac{a}{b} \\right)^2 \\left\\{ x \\middle| x > 0 \\right\\}",
                true,
            ),
            (
                "\\mathbb{R} \\mathcal{L} \\mathbf{v} \\hat{x} \\vec{v} \\bar{z} \\xrightarrow{f}",
                false,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let formula = renderer.render(tex, display, 30.).unwrap();
            let mut pgm = format!("P5 {} {} 255\n", formula.width, formula.height).into_bytes();
            pgm.extend(formula.coverage.iter().map(|alpha| 255 - alpha));
            std::fs::write(
                std::path::Path::new(&directory).join(format!("{index}.pgm")),
                pgm,
            )
            .unwrap();
        }
    }

    #[test]
    fn reports_errors_and_refuses_what_is_too_large() {
        let renderer = Renderer::new();
        assert_eq!(
            renderer.render("\\foo", false, 20.),
            Err(Error::Latex("unknown command \\foo".into()))
        );
        assert_eq!(renderer.render("x", false, 1000.), Err(Error::Size));
        let wide = "x".repeat(2000);
        assert_eq!(renderer.render(&wide, true, 40.), Err(Error::TooLarge));
        let tall = vec!["x"; 200].join("\\\\");
        let tall = format!("\\begin{{matrix}}{tall}\\end{{matrix}}");
        assert_eq!(renderer.render(&tall, true, 40.), Err(Error::TooLarge));
    }

    #[test]
    fn typst_code_never_runs() {
        let renderer = Renderer::new();
        // Reached only if the converter let code through: the library has
        // no file access, and the world no files.
        let library = super::restricted_library();
        for name in ["read", "plugin", "image", "json", "eval", "std"] {
            assert!(library.global.scope().get(name).is_none(), "{name}");
        }
        assert!(library.global.scope().get("text").is_some());
        let compiled = renderer.compile("#read(\"/etc/passwd\")", false);
        assert!(matches!(compiled, Err(Error::Typst(message)) if message.contains("read")));
        let compiled = renderer.compile("#std.read(\"/etc/passwd\")", false);
        assert!(compiled.is_err());
        let compiled = renderer.compile("#import \"@preview/mitex:0.2.4\": *", false);
        assert!(compiled.is_err());
    }
}
