//! Resume PDFs: Typst templates compiled in-process against bundled fonts.
//!
//! The resume never touches Typst markup. It is serialized to JSON and served as the
//! virtual file `/data.json`, which each template reads with `json(...)`, so user text
//! is always plain content and can't inject markup or code.
use std::sync::OnceLock;

use anyhow::{anyhow, Context};
use serde_json::json;
use typst::diag::{FileError, FileResult, SourceDiagnostic};
use typst::foundations::{Bytes, Datetime, Duration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_layout::PagedDocument;

use crate::resume::{Header, Resume};

pub struct Template {
    pub id: &'static str,
    pub name: &'static str,
    pub description: &'static str,
}

pub const TEMPLATES: &[Template] = &[
    Template {
        id: "classic",
        name: "Classic",
        description: "Large name over a teal-to-purple rule, teal section titles, purple company names.",
    },
    Template {
        id: "modern",
        name: "Modern",
        description: "Bold name, small-caps titles with an accent bar, competencies as subtle chips.",
    },
    Template {
        id: "compact",
        name: "Compact",
        description: "Dense 10pt layout that keeps long careers within two pages.",
    },
    Template {
        id: "executive",
        name: "Executive",
        description: "Serif type, centered name, restrained black and grey with generous spacing.",
    },
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PageSize {
    #[default]
    A4,
    Letter,
}

impl PageSize {
    fn paper(self) -> &'static str {
        match self {
            PageSize::A4 => "a4",
            PageSize::Letter => "us-letter",
        }
    }
}

/// Typst sources, served at the virtual project root.
const SOURCES: &[(&str, &str)] = &[
    ("/common.typ", include_str!("../templates/common.typ")),
    ("/classic.typ", include_str!("../templates/classic.typ")),
    ("/modern.typ", include_str!("../templates/modern.typ")),
    ("/compact.typ", include_str!("../templates/compact.typ")),
    ("/executive.typ", include_str!("../templates/executive.typ")),
];

const FONTS: &[&[u8]] = &[
    include_bytes!("../templates/fonts/SourceSans3-Regular.otf"),
    include_bytes!("../templates/fonts/SourceSans3-It.otf"),
    include_bytes!("../templates/fonts/SourceSans3-Semibold.otf"),
    include_bytes!("../templates/fonts/SourceSans3-Bold.otf"),
    include_bytes!("../templates/fonts/IBMPlexSans-Regular.otf"),
    include_bytes!("../templates/fonts/IBMPlexSans-Italic.otf"),
    include_bytes!("../templates/fonts/IBMPlexSans-SemiBold.otf"),
    include_bytes!("../templates/fonts/IBMPlexSans-Bold.otf"),
    include_bytes!("../templates/fonts/LibertinusSerif-Regular.otf"),
    include_bytes!("../templates/fonts/LibertinusSerif-Italic.otf"),
    include_bytes!("../templates/fonts/LibertinusSerif-Semibold.otf"),
    include_bytes!("../templates/fonts/LibertinusSerif-Bold.otf"),
];

/// Everything that is the same for every render, parsed once per process.
struct Assets {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    sources: Vec<Source>,
    data_id: FileId,
}

fn file_id(path: &str) -> FileId {
    let vpath = VirtualPath::new(path).expect("valid virtual path");
    RootedPath::new(VirtualRoot::Project, vpath).intern()
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let fonts: Vec<Font> = FONTS
            .iter()
            .flat_map(|data| Font::iter(Bytes::new(*data)))
            .collect();
        Assets {
            library: LazyHash::new(Library::default()),
            book: LazyHash::new(FontBook::from_fonts(&fonts)),
            fonts,
            sources: SOURCES
                .iter()
                .map(|(path, text)| Source::new(file_id(path), (*text).to_string()))
                .collect(),
            data_id: file_id("/data.json"),
        }
    })
}

struct ResumeWorld {
    main: FileId,
    data: Bytes,
}

impl World for ResumeWorld {
    fn library(&self) -> &LazyHash<Library> {
        &assets().library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &assets().book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        let assets = assets();
        assets
            .sources
            .iter()
            .find(|s| s.id() == id)
            .cloned()
            .ok_or_else(|| FileError::NotFound(id.get().vpath().get_with_slash().into()))
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if id == assets().data_id {
            Ok(self.data.clone())
        } else {
            Err(FileError::NotFound(id.get().vpath().get_with_slash().into()))
        }
    }

    fn font(&self, index: usize) -> Option<Font> {
        assets().fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

/// Only web, mail and phone links are made clickable; bare domains and emails get a scheme.
fn safe_url(url: &str) -> String {
    let u = url.trim();
    let lower = u.to_ascii_lowercase();
    if ["https://", "http://", "mailto:", "tel:"].iter().any(|p| lower.starts_with(p)) {
        u.to_string()
    } else if u.is_empty() || u.contains(char::is_whitespace) || u.contains(':') {
        String::new()
    } else if u.contains('@') {
        format!("mailto:{u}")
    } else if u.contains('.') {
        format!("https://{u}")
    } else {
        String::new()
    }
}

fn data_json(resume: &Resume, header: &Header, page: PageSize) -> serde_json::Result<Vec<u8>> {
    let contacts: Vec<_> = header
        .contacts
        .iter()
        .filter(|c| !c.text.trim().is_empty())
        .map(|c| json!({ "text": c.text.trim(), "url": safe_url(&c.url) }))
        .collect();
    serde_json::to_vec(&json!({
        "paper": page.paper(),
        "header": { "name": header.name.trim(), "headline": header.headline.trim(), "contacts": contacts },
        "resume": resume.for_prompt(),
    }))
}

fn diagnostics(errors: &[SourceDiagnostic]) -> String {
    errors.iter().map(|e| e.message.to_string()).collect::<Vec<_>>().join("; ")
}

fn layout(resume: &Resume, header: &Header, template_id: &str, page: PageSize) -> anyhow::Result<PagedDocument> {
    let template = TEMPLATES.iter().find(|t| t.id == template_id).unwrap_or(&TEMPLATES[0]);
    let world = ResumeWorld {
        main: file_id(&format!("/{}.typ", template.id)),
        data: Bytes::new(data_json(resume, header, page)?),
    };
    let result = typst::compile::<PagedDocument>(&world).output;
    // Drop memoized results from older renders so the cache stays small.
    typst::comemo::evict(10);
    result.map_err(|e| anyhow!("resume layout failed: {}", diagnostics(&e)))
}

/// Renders the resume to PDF bytes. An unknown template id falls back to the first template.
pub fn render(resume: &Resume, header: &Header, template_id: &str, page: PageSize) -> anyhow::Result<Vec<u8>> {
    let document = layout(resume, header, template_id, page)?;
    typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default())
        .map_err(|e| anyhow!("PDF export failed: {}", diagnostics(&e)))
}

/// A rendered page: RGBA pixels (premultiplied; pages are opaque white, so equal to straight RGBA).
pub struct PageImage {
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// The resume's pages as images, exactly as the PDF prints them, at `pixels_per_pt` (1.0 = 72 dpi).
/// `max_pages` limits work for thumbnails.
pub fn render_pages(resume: &Resume, header: &Header, template_id: &str, page: PageSize, pixels_per_pt: f64,
                    max_pages: usize) -> anyhow::Result<Vec<PageImage>> {
    let document = layout(resume, header, template_id, page)?;
    let opts = typst_render::RenderOptions { pixel_per_pt: typst::utils::Scalar::new(pixels_per_pt), ..Default::default() };
    Ok(document.pages().iter().take(max_pages).map(|p| {
        let pixmap = typst_render::render(p, &opts);
        PageImage { width: pixmap.width(), height: pixmap.height(), rgba: pixmap.take() }
    }).collect())
}

/// The plain text an ATS would parse from the PDF, and its page count.
pub fn extract_text(pdf: &[u8]) -> anyhow::Result<(String, usize)> {
    let doc = pdf_extract::Document::load_mem(pdf).context("could not read the PDF")?;
    let mut out = TextLayout::default();
    pdf_extract::output_doc(&doc, &mut out).context("could not read the PDF text")?;
    Ok((out.text.trim().to_string(), out.pages))
}

/// Rebuilds reading text from glyph positions the way PDF-to-text tools do: a new line
/// when the baseline moves, a space when there's a visible gap between glyphs.
#[derive(Default)]
struct TextLayout {
    text: String,
    pages: usize,
    flip_y: f64,
    last: Option<(f64, f64)>, // baseline y and the x where the previous glyph ended
}

impl pdf_extract::OutputDev for TextLayout {
    fn begin_page(&mut self, _: u32, media: &pdf_extract::MediaBox, _: Option<(f64, f64, f64, f64)>) -> Result<(), pdf_extract::OutputError> {
        if self.pages > 0 {
            self.text.push_str("\n\n");
        }
        self.pages += 1;
        self.flip_y = media.ury;
        self.last = None;
        Ok(())
    }

    fn output_character(&mut self, trm: &pdf_extract::Transform, width: f64, _: f64, font_size: f64, ch: &str) -> Result<(), pdf_extract::OutputError> {
        let size = font_size * (trm.m11 * trm.m22 - trm.m12 * trm.m21).abs().sqrt();
        let (x, y) = (trm.m31, self.flip_y - trm.m32);
        if let Some((last_y, last_end)) = self.last {
            let at_space = self.text.ends_with(char::is_whitespace) || ch.starts_with(char::is_whitespace);
            if (y - last_y).abs() > size * 0.4 {
                self.text.truncate(self.text.trim_end_matches(' ').len());
                self.text.push('\n');
            } else if !at_space && (x > last_end + size * 0.15 || x < last_end - size) {
                self.text.push(' ');
            }
        }
        self.text.push_str(ch);
        self.last = Some((y, x + width * size));
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), pdf_extract::OutputError> { Ok(()) }
    fn begin_word(&mut self) -> Result<(), pdf_extract::OutputError> { Ok(()) }
    fn end_word(&mut self) -> Result<(), pdf_extract::OutputError> { Ok(()) }
    fn end_line(&mut self) -> Result<(), pdf_extract::OutputError> { Ok(()) }
}
