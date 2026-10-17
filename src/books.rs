//! Books and comics in the inspector. An EPUB is a zip of XHTML chapters: its title,
//! author, cover and the chapters' text are turned into Markdown and shown by the
//! Markdown preview, headings and paragraphs kept. A CBZ or CBR is an archive of
//! pictures, shown page by page as a PDF is. Both are read through the archive code
//! (bsdtar), one member at a time.

use std::path::{Path, PathBuf};

/// Text of a book shown at most; a novel fits.
const MAX_TEXT: usize = 400 << 10;
/// A chapter file read at most.
const MAX_CHAPTER: usize = 4 << 20;

pub fn is_book(ext: &str) -> bool {
    ext == "epub"
}

pub fn is_comic(ext: &str) -> bool {
    matches!(ext, "cbz" | "cbr")
}

fn is_picture(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    [".jpg", ".jpeg", ".png", ".webp", ".gif", ".bmp"].iter().any(|e| l.ends_with(e))
}

/// A comic's pages: the pictures in it, in the order their names count (page 2 before
/// page 10), skipping the hidden files some tools leave in.
pub fn comic_pages(archive: &Path) -> Result<Vec<PathBuf>, String> {
    let all = crate::archive::list(archive)?;
    let pics: Vec<PathBuf> = all
        .iter()
        .filter(|i| !i.is_dir && is_picture(&i.path.to_string_lossy()) && !i.path.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.') || c.as_os_str() == "__MACOSX"))
        .map(|i| i.path.clone())
        .collect();
    let keys: Vec<String> = pics.iter().map(|p| p.to_string_lossy().to_lowercase()).collect();
    let mut order: Vec<u32> = (0..pics.len() as u32).collect();
    crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| crate::tab::natural_cmp(&keys[*a as usize], &keys[*b as usize]));
    Ok(order.iter().map(|&i| pics[i as usize].clone()).collect())
}

/// One page of a comic, decoded and scaled so its longer side is at most `px`.
pub fn comic_page(archive: &Path, member: &Path, px: u32) -> Option<crate::imgdec::Rgba> {
    let b = crate::archive::read_member(archive, member, 64 << 20).ok()?;
    crate::imgdec::decode_fit(&b, px)
}

// ------------------------------------------------------------------ XML, just enough

/// The tags and text of an XML or XHTML document, in order.
#[derive(Debug, PartialEq)]
pub enum Tok<'a> {
    /// Name (lower case, without a namespace prefix), its attributes as written, closing,
    /// and self-closing.
    Tag { name: String, attrs: &'a str, close: bool, empty: bool },
    Text(&'a str),
}

pub fn tokens(s: &str) -> Vec<Tok<'_>> {
    let mut out = vec![];
    let mut rest = s;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push(Tok::Text(rest));
            break;
        };
        if lt > 0 {
            out.push(Tok::Text(&rest[..lt]));
        }
        rest = &rest[lt..];
        // Comments, CDATA, declarations and processing instructions.
        let skip = [("<!--", "-->"), ("<![CDATA[", "]]>"), ("<!", ">"), ("<?", "?>")].iter().find(|(a, _)| rest.starts_with(a)).map(|(a, z)| (a.len(), *z));
        if let Some((open, close)) = skip {
            if let Some(cdata) = rest.strip_prefix("<![CDATA[") {
                let end = cdata.find("]]>").unwrap_or(cdata.len());
                out.push(Tok::Text(&cdata[..end]));
            }
            rest = rest[open..].find(close).map_or("", |e| &rest[open + e + close.len()..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let inner = &rest[1..gt];
        rest = &rest[gt + 1..];
        let close = inner.starts_with('/');
        let inner = inner.trim_start_matches('/');
        let empty = inner.ends_with('/');
        let inner = inner.trim_end_matches('/');
        let end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
        let full = &inner[..end];
        let name = full.rsplit(':').next().unwrap_or(full).to_ascii_lowercase();
        out.push(Tok::Tag { name, attrs: &inner[end..], close, empty });
    }
    out
}

/// An attribute's value (entities decoded), matched without its namespace prefix.
pub fn attr(attrs: &str, key: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let k = &attrs[ks..i];
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'=') {
            i += 1;
        }
        let (q, vs) = match b.get(i) {
            Some(&c) if c == b'"' || c == b'\'' => (c, i + 1),
            _ => (b' ', i),
        };
        let mut e = vs;
        while e < b.len() && b[e] != q && !(q == b' ' && b[e].is_ascii_whitespace()) {
            e += 1;
        }
        let v = &attrs[vs..e.min(attrs.len())];
        i = e + 1;
        if k.rsplit(':').next().is_some_and(|k| k.eq_ignore_ascii_case(key)) {
            return Some(crate::markdown::decode_entities(v).into_owned());
        }
    }
    None
}

// ------------------------------------------------------------------ EPUB

#[derive(Debug, Default, PartialEq)]
pub struct Book {
    pub title: String,
    pub author: String,
    /// The cover picture's path inside the book.
    pub cover: Option<PathBuf>,
    /// The chapters' files inside the book, in reading order.
    pub chapters: Vec<PathBuf>,
}

/// `a/b/../c d.xhtml#x` from the package's folder, as a path inside the book.
fn inside(base: &Path, href: &str) -> PathBuf {
    let href = href.split('#').next().unwrap_or("");
    let mut out = PathBuf::new();
    for c in base.join(&*crate::markdown::percent_decode(href)).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out
}

/// Where container.xml says the package document is.
pub fn package_path(container: &str) -> Option<PathBuf> {
    tokens(container).into_iter().find_map(|t| match t {
        Tok::Tag { name, attrs, close: false, .. } if name == "rootfile" => attr(attrs, "full-path").map(PathBuf::from),
        _ => None,
    })
}

/// The package document (OPF): title, author, cover and spine, with paths inside the
/// book (the OPF lives at `opf`).
pub fn package(opf_text: &str, opf: &Path) -> Book {
    let base = opf.parent().unwrap_or(Path::new(""));
    let toks = tokens(opf_text);
    let mut items: Vec<(String, String, String, String)> = vec![];
    let mut spine: Vec<String> = vec![];
    let mut book = Book::default();
    let mut cover_id = None;
    let mut field: Option<&'static str> = None;
    for t in &toks {
        match t {
            Tok::Tag { name, attrs, close: false, .. } => match name.as_str() {
                "item" => items.push((attr(attrs, "id").unwrap_or_default(), attr(attrs, "href").unwrap_or_default(), attr(attrs, "media-type").unwrap_or_default(), attr(attrs, "properties").unwrap_or_default())),
                "itemref" => spine.extend(attr(attrs, "idref")),
                "meta" if attr(attrs, "name").as_deref() == Some("cover") => cover_id = attr(attrs, "content"),
                "title" if book.title.is_empty() => field = Some("title"),
                "creator" if book.author.is_empty() => field = Some("creator"),
                _ => {}
            },
            Tok::Tag { close: true, .. } => field = None,
            Tok::Text(s) => match field {
                Some("title") => book.title.push_str(crate::markdown::decode_entities(s).trim()),
                Some("creator") => book.author.push_str(crate::markdown::decode_entities(s).trim()),
                _ => {}
            },
        }
    }
    let cover = items.iter().find(|i| i.3.split_whitespace().any(|p| p == "cover-image")).or_else(|| cover_id.and_then(|c| items.iter().find(|i| i.0 == c))).filter(|i| i.2.starts_with("image/") || is_picture(&i.1));
    book.cover = cover.map(|i| inside(base, &i.1));
    book.chapters = spine.iter().filter_map(|id| items.iter().find(|i| &i.0 == id)).filter(|i| i.2.contains("html") || i.1.ends_with("html") || i.1.ends_with(".htm")).map(|i| inside(base, &i.1)).collect();
    book
}

/// Text made safe for Markdown: nothing in it read as emphasis, code, a link, a formula
/// or a block's start.
fn md_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if matches!(c, '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '$' | '#' | '|' | '~') {
            out.push('\\');
        }
        out.push(c);
    }
    // "1. ", "- " and "+ " at the start of a paragraph would make a list.
    let lead = out.trim_start();
    let digits = lead.chars().take_while(|c| c.is_ascii_digit()).count();
    if lead.starts_with("- ") || lead.starts_with("+ ") || (digits > 0 && lead[digits..].starts_with(". ")) {
        let at = out.len() - lead.len() + if digits > 0 { digits } else { 0 };
        out.insert(at, '\\');
    }
    out
}

/// A chapter's XHTML as Markdown: headings, paragraphs, list items and quotes, the
/// words of everything else; script, style and the head left out.
pub fn chapter_markdown(xhtml: &str) -> String {
    let mut out = String::new();
    let mut para = String::new();
    let mut skip = 0usize;
    let mut heading: Option<u8> = None;
    let mut quote = 0usize;
    let mut item = false;
    let flush = |out: &mut String, para: &mut String, heading: Option<u8>, quote: usize, item: bool| {
        let text = para.split_whitespace().collect::<Vec<_>>().join(" ");
        para.clear();
        if text.is_empty() {
            return;
        }
        let prefix = match heading {
            Some(n) => format!("{} ", "#".repeat(n.clamp(1, 6) as usize)),
            None if item => "- ".to_string(),
            None => String::new(),
        };
        let q = if quote > 0 { "> " } else { "" };
        out.push_str(&format!("{q}{prefix}{}\n\n", md_escape(&text)));
    };
    for t in tokens(xhtml) {
        match t {
            Tok::Tag { name, close, empty, .. } => {
                let n = name.as_str();
                if matches!(n, "head" | "script" | "style" | "title" | "svg") {
                    if !empty {
                        if close { skip = skip.saturating_sub(1) } else { skip += 1 }
                    }
                    continue;
                }
                if skip > 0 {
                    continue;
                }
                match n {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush(&mut out, &mut para, heading, quote, item);
                        heading = if close { None } else { n[1..].parse().ok() };
                    }
                    "p" | "div" | "section" | "article" | "tr" | "dt" | "dd" | "figcaption" | "pre" | "table" | "ul" | "ol" => flush(&mut out, &mut para, heading, quote, item),
                    "li" => {
                        flush(&mut out, &mut para, heading, quote, item);
                        item = !close;
                    }
                    "blockquote" => {
                        flush(&mut out, &mut para, heading, quote, item);
                        quote = if close { quote.saturating_sub(1) } else { quote + 1 };
                    }
                    "br" => para.push(' '),
                    "hr" => {
                        flush(&mut out, &mut para, heading, quote, item);
                        out.push_str("---\n\n");
                    }
                    "td" | "th" => para.push(' '),
                    _ => {}
                }
            }
            Tok::Text(s) if skip == 0 => para.push_str(&crate::markdown::decode_entities(s).replace('\u{a0}', " ")),
            Tok::Text(_) => {}
        }
    }
    flush(&mut out, &mut para, heading, quote, item);
    out
}

/// A whole book as Markdown: its title, author and cover (`cover` names where the
/// picture was put on the disk), then the chapters, cut short past `MAX_TEXT`.
pub fn book_markdown(book: &Book, cover: Option<&Path>, chapters: &mut dyn Iterator<Item = String>) -> String {
    let mut out = String::new();
    if !book.title.is_empty() {
        out.push_str(&format!("# {}\n\n", md_escape(&book.title)));
    }
    if !book.author.is_empty() {
        out.push_str(&format!("*{}*\n\n", md_escape(&book.author)));
    }
    if let Some(c) = cover {
        // A % in the path would be read as an escape.
        out.push_str(&format!("<img src=\"{}\" alt=\"Cover\">\n\n", c.to_string_lossy().replace('%', "%25").replace('"', "%22")));
    }
    let mut first = true;
    for ch in chapters {
        if ch.trim().is_empty() {
            continue;
        }
        if !first {
            out.push_str("---\n\n");
        }
        first = false;
        out.push_str(&ch);
        if out.len() > MAX_TEXT {
            out.push_str("\n*The rest of the book is not shown here.*\n");
            break;
        }
    }
    out
}

/// Read an EPUB for its preview: Markdown lines, or why it can't be shown.
pub fn read_book(path: &Path) -> Result<String, String> {
    let read = |inner: &Path| crate::archive::read_member(path, inner, MAX_CHAPTER).map(|b| String::from_utf8_lossy(&b).into_owned());
    let container = read(Path::new("META-INF/container.xml")).map_err(|_| "Not an EPUB book".to_string())?;
    let opf = package_path(&container).ok_or("Not an EPUB book")?;
    let book = package(&read(&opf)?, &opf);
    let cover = book.cover.as_ref().and_then(|c| crate::archive::extract_for_open(path, c).ok());
    let mut total = 0usize;
    let mut chapters = book.chapters.iter().map_while(|c| {
        // Past the limit, the remaining chapters aren't read at all.
        (total <= MAX_TEXT).then(|| {
            let md = read(c).map(|x| chapter_markdown(&x)).unwrap_or_default();
            total += md.len();
            md
        })
    });
    Ok(book_markdown(&book, cover.as_deref(), &mut chapters))
}
