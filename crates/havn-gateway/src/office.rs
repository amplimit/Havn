//! The text of Word and spreadsheet files (issue #106): a file sent in a
//! chat or read by `web_fetch`, as Markdown the agent can read. These
//! parsers read untrusted archives, so they run only in the gateway's
//! converter child (see [`crate::web_fetch`]), under its memory, CPU and
//! time limits; the archive limits here keep a zip bomb from using them up
//! before anything is read.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io::{Cursor, Read as _};

use calamine::{Data, Reader as _, SheetType, SheetVisible};
use quick_xml::Reader;
use quick_xml::events::{BytesRef, BytesStart, Event};

/// Entries an archive may hold.
const MAX_ENTRIES: usize = 10_000;
/// What its entries may unpack to, all told, by their headers.
const MAX_UNPACKED_BYTES: u64 = 512 * 1024 * 1024;
/// What one part read here may unpack to.
const MAX_PART_BYTES: u64 = 64 * 1024 * 1024;
/// Rows and columns of a sheet shown.
const MAX_ROWS: usize = 1000;
const MAX_COLUMNS: usize = 60;
/// Characters of a cell shown.
const MAX_CELL_CHARS: usize = 500;
/// Formulas of a sheet listed.
const MAX_FORMULAS: usize = 500;
const MAX_SHEETS: usize = 100;
/// Characters of a comment's anchor quoted.
const MAX_ANCHOR_CHARS: usize = 200;
/// Bytes of text a spreadsheet's sheets are written to, at most about:
/// what the converter passes on.
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

type Archive<'a> = zip::ZipArchive<Cursor<&'a [u8]>>;

/// `input` as a zip archive, refused when its headers say it holds more
/// than the limits.
fn open(input: &[u8]) -> Result<Archive<'_>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(input))
        .map_err(|e| format!("not a readable Office file: {e}"))?;
    if archive.len() > MAX_ENTRIES {
        return Err(format!(
            "the file holds more than {MAX_ENTRIES} parts, more than an Office file has"
        ));
    }
    let mut unpacked: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| format!("not a readable Office file: {e}"))?;
        unpacked = unpacked.saturating_add(entry.size());
    }
    if unpacked > MAX_UNPACKED_BYTES {
        return Err(format!(
            "the file unpacks to more than {} MB",
            MAX_UNPACKED_BYTES / (1024 * 1024)
        ));
    }
    Ok(archive)
}

/// Part `name` of `archive` as text, `None` when it has none.
fn part(archive: &mut Archive<'_>, name: &str) -> Result<Option<String>, String> {
    let entry = match archive.by_name(name) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(format!("{name} cannot be read: {e}")),
    };
    let mut bytes = Vec::new();
    entry
        .take(MAX_PART_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name} cannot be read: {e}"))?;
    if bytes.len() as u64 > MAX_PART_BYTES {
        return Err(format!(
            "{name} unpacks to more than {} MB",
            MAX_PART_BYTES / (1024 * 1024)
        ));
    }
    let text = String::from_utf8_lossy(&bytes);
    Ok(Some(text.trim_start_matches('\u{feff}').to_string()))
}

/// Attribute `name` of `e`, by its local name.
fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// The character an entity or character reference stands for.
fn entity(r: &BytesRef<'_>) -> Option<char> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c);
    }
    match &**r {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

/// `text` for a Markdown table cell.
fn cell_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(MAX_CELL_CHARS * 4));
    for (i, c) in text.trim().chars().enumerate() {
        if i == MAX_CELL_CHARS {
            out.push('…');
            break;
        }
        match c {
            '|' => out.push_str("\\|"),
            '\n' => out.push_str("<br>"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// A Markdown table of `rows`, the first one its header.
fn table(rows: &[Vec<String>]) -> String {
    let width = rows
        .iter()
        .map(Vec::len)
        .max()
        .unwrap_or(0)
        .min(MAX_COLUMNS);
    let mut out = String::new();
    if width == 0 {
        return out;
    }
    for (i, row) in rows.iter().enumerate() {
        out.push('|');
        for c in 0..width {
            let _ = write!(out, " {} |", row.get(c).map(String::as_str).unwrap_or(""));
        }
        out.push('\n');
        if i == 0 {
            out.push('|');
            out.push_str(&" --- |".repeat(width));
            out.push('\n');
        }
    }
    out
}

/// A part a relationship names.
struct Relationship {
    id: String,
    kind: String,
    target: String,
    external: bool,
}

fn relationships(xml: &str) -> Vec<Relationship> {
    let mut out = Vec::new();
    let mut reader = Reader::from_str(xml);
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(target)) = (attr(&e, "Id"), attr(&e, "Target")) {
                    out.push(Relationship {
                        id,
                        kind: attr(&e, "Type").unwrap_or_default(),
                        target,
                        external: attr(&e, "TargetMode").as_deref() == Some("External"),
                    });
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

/// The heading level of each paragraph style of `styles.xml` that is one:
/// by its name ("heading 2", "Title") or its outline level, so that a
/// document whose styles have other ids (a Chinese Word's "1", "2") keeps
/// its headings.
fn heading_styles(xml: &str) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut style: Option<String> = None;
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Start(e) if e.local_name().as_ref() == "style" => {
                style = attr(&e, "styleId");
            }
            Event::End(e) if e.local_name().as_ref() == "style" => style = None,
            Event::Start(e) | Event::Empty(e) => {
                let Some(id) = &style else { continue };
                let level = match e.local_name().as_ref() {
                    "name" => attr(&e, "val").and_then(|name| {
                        let name = name.to_ascii_lowercase();
                        if name == "title" {
                            Some(1)
                        } else {
                            name.strip_prefix("heading ")?.trim().parse::<usize>().ok()
                        }
                    }),
                    "outlineLvl" => attr(&e, "val")
                        .and_then(|v| v.parse::<usize>().ok())
                        .filter(|l| *l < 9)
                        .map(|l| l + 1),
                    _ => None,
                };
                if let Some(level) = level {
                    out.entry(id.clone()).or_insert(level);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

/// A footnote, endnote or comment.
#[derive(Debug, Default)]
struct Note {
    author: Option<String>,
    text: String,
}

/// The `element`s of a notes part (`footnote`, `endnote`, `comment`) by
/// id, without Word's separator notes.
fn notes(xml: &str, element: &str) -> HashMap<String, Note> {
    let mut out = HashMap::new();
    let mut reader = Reader::from_str(xml);
    let mut current: Option<(String, Note)> = None;
    let mut in_text = false;
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Start(e) if e.local_name().as_ref() == element => {
                let normal = attr(&e, "type").is_none_or(|t| t == "normal");
                current = attr(&e, "id").filter(|_| normal).map(|id| {
                    (
                        id,
                        Note {
                            author: attr(&e, "author"),
                            text: String::new(),
                        },
                    )
                });
            }
            Event::End(e) if e.local_name().as_ref() == element => {
                if let Some((id, mut note)) = current.take() {
                    note.text = note.text.split_whitespace().collect::<Vec<_>>().join(" ");
                    out.insert(id, note);
                }
            }
            Event::Start(e) if e.local_name().as_ref() == "t" => in_text = true,
            Event::End(e) if e.local_name().as_ref() == "t" => in_text = false,
            Event::End(e) if e.local_name().as_ref() == "p" => {
                if let Some((_, note)) = &mut current {
                    note.text.push(' ');
                }
            }
            Event::Text(t) if in_text => {
                if let Some((_, note)) = &mut current {
                    note.text.push_str(&t);
                }
            }
            Event::GeneralRef(r) if in_text => {
                if let (Some((_, note)), Some(c)) = (&mut current, entity(&r)) {
                    note.text.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

/// A table being read: its rows, the row and cell being read, and how many
/// paragraphs were open when it started (one inside a text box).
#[derive(Default)]
struct Table {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: String,
    span: usize,
    depth: usize,
}

/// A paragraph being read: its text, heading level and list level.
#[derive(Default)]
struct Para {
    text: String,
    heading: Option<usize>,
    list: Option<usize>,
}

/// What a Word document's body has been read into so far.
struct Body<'a> {
    links: &'a HashMap<String, String>,
    headings: &'a HashMap<String, usize>,
    out: String,
    /// The paragraphs open, more than one inside a text box.
    paras: Vec<Para>,
    last_list: bool,
    tables: Vec<Table>,
    /// Depth inside an element whose content is not shown.
    skip: usize,
    in_text: bool,
    /// Runs open, more than one inside a text box.
    runs: usize,
    in_tabs: bool,
    /// The open hyperlink's target, the paragraph it is in and where its
    /// text starts.
    link: Option<(String, usize, usize)>,
    /// Text of the comments whose range is open, by id.
    anchors: Vec<(String, String)>,
    anchored: HashMap<String, String>,
    footnotes: Vec<String>,
    endnotes: Vec<String>,
    comments: Vec<String>,
    insertions: usize,
    deletions: usize,
}

/// Comment ranges open at once whose text is quoted.
const MAX_OPEN_ANCHORS: usize = 100;

impl<'a> Body<'a> {
    fn new(links: &'a HashMap<String, String>, headings: &'a HashMap<String, usize>) -> Self {
        Self {
            links,
            headings,
            out: String::new(),
            paras: Vec::new(),
            last_list: false,
            tables: Vec::new(),
            skip: 0,
            in_text: false,
            runs: 0,
            in_tabs: false,
            link: None,
            anchors: Vec::new(),
            anchored: HashMap::new(),
            footnotes: Vec::new(),
            endnotes: Vec::new(),
            comments: Vec::new(),
            insertions: 0,
            deletions: 0,
        }
    }

    fn push(&mut self, text: &str) {
        if let Some(p) = self.paras.last_mut() {
            p.text.push_str(text);
        }
        for (_, anchor) in &mut self.anchors {
            let room = MAX_ANCHOR_CHARS.saturating_sub(anchor.chars().count());
            anchor.extend(text.chars().take(room));
        }
    }

    fn read(&mut self, xml: &str) -> Result<(), String> {
        let mut reader = Reader::from_str(xml);
        loop {
            let event = reader
                .read_event()
                .map_err(|e| format!("not a readable Word document: {e}"))?;
            if self.skip > 0 {
                match event {
                    Event::Start(_) => self.skip += 1,
                    Event::End(_) => self.skip -= 1,
                    Event::Eof => break,
                    _ => {}
                }
                continue;
            }
            match event {
                Event::Start(e) => self.start(&e, false),
                Event::Empty(e) => self.start(&e, true),
                Event::End(e) => self.end(e.local_name().as_ref()),
                Event::Text(t) if self.in_text => self.push(&t),
                Event::GeneralRef(r) if self.in_text => {
                    if let Some(c) = entity(&r) {
                        self.push(c.encode_utf8(&mut [0; 4]));
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        Ok(())
    }

    fn start(&mut self, e: &BytesStart<'_>, empty: bool) {
        match e.local_name().as_ref() {
            // What a document showed before its tracked deletions and
            // formatting changes, and what an app that cannot read the
            // content above it shows.
            "del" | "moveFrom" if !empty => {
                self.deletions += 1;
                self.skip = 1;
            }
            "pPrChange" | "rPrChange" | "tcPrChange" | "trPrChange" | "tblPrChange"
            | "tblPrExChange" | "sectPrChange" | "numberingChange" | "Fallback"
                if !empty =>
            {
                self.skip = 1;
            }
            "ins" | "moveTo" if !empty => self.insertions += 1,
            "p" => {
                self.paras.push(Para::default());
                if empty {
                    self.end_paragraph();
                }
            }
            "pStyle" => {
                let level = attr(e, "val").and_then(|s| self.headings.get(&s).copied());
                if let Some(p) = self.paras.last_mut() {
                    p.heading = level;
                }
            }
            "outlineLvl" => {
                if let (Some(p), Some(level)) = (
                    self.paras.last_mut(),
                    attr(e, "val").and_then(|v| v.parse::<usize>().ok()),
                ) {
                    p.heading = (level < 9).then_some(level + 1);
                }
            }
            "numPr" => {
                if let Some(p) = self.paras.last_mut() {
                    p.list = Some(0);
                }
            }
            "ilvl" => {
                let level = attr(e, "val")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0)
                    .min(8);
                if let Some(p) = self.paras.last_mut() {
                    p.list = Some(level);
                }
            }
            "numId" if attr(e, "val").as_deref() == Some("0") => {
                if let Some(p) = self.paras.last_mut() {
                    p.list = None;
                }
            }
            "r" if !empty => self.runs += 1,
            "t" if !empty => self.in_text = true,
            "tabs" if !empty => self.in_tabs = true,
            "tab" if self.runs > 0 && !self.in_tabs => self.push("\t"),
            "br" | "cr" if self.runs > 0 => self.push("\n"),
            "noBreakHyphen" if self.runs > 0 => self.push("-"),
            "tbl" if !empty => self.tables.push(Table {
                depth: self.paras.len(),
                ..Table::default()
            }),
            "tr" if !empty => {
                if let Some(t) = self.tables.last_mut() {
                    t.row.clear();
                }
            }
            "tc" if !empty => {
                if let Some(t) = self.tables.last_mut() {
                    t.cell.clear();
                    t.span = 1;
                }
            }
            "gridSpan" => {
                if let Some(t) = self.tables.last_mut() {
                    t.span = attr(e, "val")
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .clamp(1, MAX_COLUMNS);
                }
            }
            "hyperlink" if !empty => {
                let target = attr(e, "id").and_then(|id| self.links.get(&id).cloned());
                let start = self.paras.last().map_or(0, |p| p.text.len());
                self.link = target.map(|t| (t, self.paras.len(), start));
            }
            "docPr" => {
                let alt = attr(e, "descr")
                    .or_else(|| attr(e, "title"))
                    .map(|a| a.split_whitespace().collect::<Vec<_>>().join(" "))
                    .filter(|a| !a.is_empty());
                match alt {
                    Some(alt) => self.push(&format!("[image: {alt}]")),
                    None => self.push("[image]"),
                }
            }
            "footnoteReference" => {
                if let Some(id) = attr(e, "id") {
                    self.push(&format!("[^{id}]"));
                    self.footnotes.push(id);
                }
            }
            "endnoteReference" => {
                if let Some(id) = attr(e, "id") {
                    self.push(&format!("[^e{id}]"));
                    self.endnotes.push(id);
                }
            }
            "commentRangeStart" if self.anchors.len() < MAX_OPEN_ANCHORS => {
                if let Some(id) = attr(e, "id") {
                    self.anchors.push((id, String::new()));
                }
            }
            "commentRangeEnd" => {
                if let Some(id) = attr(e, "id")
                    && let Some(i) = self.anchors.iter().position(|(a, _)| *a == id)
                {
                    let (id, text) = self.anchors.remove(i);
                    self.anchored.insert(id, text);
                }
            }
            "commentReference" => {
                if let Some(id) = attr(e, "id") {
                    self.push(&format!("[comment {id}]"));
                    self.comments.push(id);
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "t" => self.in_text = false,
            "r" => self.runs = self.runs.saturating_sub(1),
            "tabs" => self.in_tabs = false,
            "hyperlink" => {
                if let Some((target, depth, start)) = self.link.take()
                    && depth == self.paras.len()
                    && let Some(p) = self.paras.last_mut()
                    && start <= p.text.len()
                    && p.text.is_char_boundary(start)
                {
                    let text = p.text.split_off(start);
                    if text.trim().is_empty() || text.trim() == target {
                        p.text.push_str(&text);
                    } else {
                        let _ = write!(p.text, "[{text}]({target})");
                    }
                }
            }
            "p" => self.end_paragraph(),
            "tc" => {
                if let Some(t) = self.tables.last_mut() {
                    let cell = cell_text(&t.cell);
                    t.row.push(cell);
                    for _ in 1..t.span {
                        t.row.push(String::new());
                    }
                }
            }
            "tr" => {
                if let Some(t) = self.tables.last_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            "tbl" => {
                let Some(done) = self.tables.pop() else {
                    return;
                };
                // A table in a text box goes into its paragraph, one in a
                // cell into the cell, flattened.
                let in_para = match self.tables.last() {
                    Some(outer) => self.paras.len() > outer.depth,
                    None => !self.paras.is_empty(),
                };
                let flat = || {
                    done.rows
                        .iter()
                        .map(|r| r.join(" | "))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                if in_para {
                    if let Some(p) = self.paras.last_mut() {
                        if !p.text.is_empty() {
                            p.text.push('\n');
                        }
                        p.text.push_str(&flat());
                    }
                } else if let Some(outer) = self.tables.last_mut() {
                    if !outer.cell.is_empty() {
                        outer.cell.push('\n');
                    }
                    outer.cell.push_str(&flat());
                } else {
                    if !self.out.is_empty() && !self.out.ends_with("\n\n") {
                        self.out.push('\n');
                    }
                    self.out.push_str(&table(&done.rows));
                    self.out.push('\n');
                    self.last_list = false;
                }
            }
            _ => {}
        }
    }

    /// The paragraph that just ended: into the table cell it is in, the
    /// paragraph whose text box holds it, or the output.
    fn end_paragraph(&mut self) {
        let Some(para) = self.paras.pop() else {
            return;
        };
        let text = para.text.trim_end();
        if let Some(t) = self.tables.last_mut()
            && t.depth == self.paras.len()
        {
            let text = match para.list {
                Some(_) if !text.is_empty() => format!("- {text}"),
                _ => text.to_string(),
            };
            if !t.cell.is_empty() && !text.is_empty() {
                t.cell.push('\n');
            }
            t.cell.push_str(&text);
            return;
        }
        if let Some(outer) = self.paras.last_mut() {
            if !text.is_empty() {
                outer.text.push('\n');
                outer.text.push_str(text);
            }
            return;
        }
        if text.trim().is_empty() {
            self.out.push('\n');
            self.last_list = false;
            return;
        }
        match (para.heading, para.list) {
            (Some(level), _) => {
                let _ = write!(
                    self.out,
                    "\n{} {}\n\n",
                    "#".repeat(level.min(6)),
                    text.trim()
                );
                self.last_list = false;
            }
            (None, Some(level)) => {
                if !self.last_list {
                    self.out.push('\n');
                }
                let _ = writeln!(self.out, "{}- {}", "  ".repeat(level), text.trim_start());
                self.last_list = true;
            }
            (None, None) => {
                if self.last_list {
                    self.out.push('\n');
                }
                let _ = write!(self.out, "{text}\n\n");
                self.last_list = false;
            }
        }
    }
}

/// The part a package's relationship of type `…/officeDocument` names, by
/// its path in the archive.
fn main_part(archive: &mut Archive<'_>) -> Result<String, String> {
    let named = part(archive, "_rels/.rels")?.and_then(|rels| {
        relationships(&rels)
            .into_iter()
            .find(|r| r.kind.ends_with("/officeDocument") && !r.external)
            .map(|r| r.target.trim_start_matches('/').to_string())
    });
    Ok(named.unwrap_or_else(|| "word/document.xml".to_string()))
}

/// The text of Word document `input` (.docx) as Markdown: headings, lists,
/// tables, links, image descriptions, then its footnotes, endnotes and
/// comments with the text they are on. Tracked changes show accepted.
pub fn docx_text(input: &[u8]) -> Result<String, String> {
    let mut archive = open(input)?;
    let document = main_part(&mut archive)?;
    let xml =
        part(&mut archive, &document)?.ok_or("not a Word document: it has no document part")?;
    let (dir, file) = document.rsplit_once('/').unwrap_or(("", &document));
    let rels_path = if dir.is_empty() {
        format!("_rels/{file}.rels")
    } else {
        format!("{dir}/_rels/{file}.rels")
    };
    let in_dir = |name: &str| {
        if dir.is_empty() {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    };
    let rels = part(&mut archive, &rels_path)?
        .map(|x| relationships(&x))
        .unwrap_or_default();
    let links: HashMap<String, String> = rels
        .iter()
        .filter(|r| r.external && r.kind.ends_with("/hyperlink"))
        .map(|r| (r.id.clone(), r.target.clone()))
        .collect();
    // The parts the document names (a target from the package root when
    // it starts with `/`), at their usual names otherwise.
    let named = |archive: &mut Archive<'_>, suffix: &str, usual: &str| {
        let target = rels
            .iter()
            .find(|r| r.kind.ends_with(suffix) && !r.external)
            .map(|r| match r.target.strip_prefix('/') {
                Some(root) => root.to_string(),
                None => in_dir(&r.target),
            });
        let usual = in_dir(usual);
        match target.filter(|t| *t != usual) {
            Some(target) => match part(archive, &target)? {
                Some(found) => Ok(Some(found)),
                None => part(archive, &usual),
            },
            None => part(archive, &usual),
        }
    };
    let headings = named(&mut archive, "/styles", "styles.xml")?
        .map(|x| heading_styles(&x))
        .unwrap_or_default();
    let footnotes = named(&mut archive, "/footnotes", "footnotes.xml")?
        .map(|x| notes(&x, "footnote"))
        .unwrap_or_default();
    let endnotes = named(&mut archive, "/endnotes", "endnotes.xml")?
        .map(|x| notes(&x, "endnote"))
        .unwrap_or_default();
    let comments = named(&mut archive, "/comments", "comments.xml")?
        .map(|x| notes(&x, "comment"))
        .unwrap_or_default();

    let mut body = Body::new(&links, &headings);
    body.read(&xml)?;
    let mut out = String::new();
    if body.insertions + body.deletions > 0 {
        let _ = write!(
            out,
            "> This document has tracked changes ({} insertions, {} deletions); the text shows them accepted.\n\n",
            body.insertions, body.deletions
        );
    }
    out.push_str(body.out.trim());
    out.push('\n');
    let mut section = |title: &str, ids: &[String], notes: &HashMap<String, Note>, label: &str| {
        let lines: Vec<String> = ids
            .iter()
            .filter_map(|id| {
                notes
                    .get(id)
                    .map(|n| format!("[^{label}{id}]: {}", n.text.trim()))
            })
            .collect();
        if !lines.is_empty() {
            let _ = write!(out, "\n## {title}\n\n{}\n", lines.join("\n"));
        }
    };
    section("Footnotes", &body.footnotes, &footnotes, "");
    section("Endnotes", &body.endnotes, &endnotes, "e");
    let commented: Vec<String> = body
        .comments
        .iter()
        .filter_map(|id| {
            let note = comments.get(id)?;
            let mut line = format!("- [comment {id}]");
            if let Some(author) = note.author.as_deref().filter(|a| !a.trim().is_empty()) {
                let _ = write!(line, " {}", author.trim());
            }
            if let Some(anchor) = body
                .anchored
                .get(id)
                .map(|a| a.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|a| !a.is_empty())
            {
                let _ = write!(line, ", on \"{anchor}\"");
            }
            let _ = write!(line, ": {}", note.text.trim());
            Some(line)
        })
        .collect();
    if !commented.is_empty() {
        let _ = write!(out, "\n## Comments\n\n{}\n", commented.join("\n"));
    }
    Ok(out)
}

/// Column `c` (from 0) as a spreadsheet names it: A, B, …, Z, AA.
fn column(c: u32) -> String {
    let mut n = u64::from(c) + 1;
    let mut letters = Vec::new();
    while n > 0 {
        n -= 1;
        letters.push(b'A' + (n % 26) as u8);
        n /= 26;
    }
    letters.reverse();
    String::from_utf8(letters).unwrap_or_default()
}

/// A cell's value as text: dates as dates, times of day as times,
/// durations as h:mm:ss.
fn value(data: &Data) -> String {
    match data {
        Data::DateTime(t) if t.is_duration() => match t.as_duration() {
            Some(d) => {
                let s = d.num_seconds();
                let sign = if s < 0 { "-" } else { "" };
                let s = s.unsigned_abs();
                format!("{sign}{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
            }
            None => t.to_string(),
        },
        Data::DateTime(t) => match t.as_datetime() {
            Some(dt) if (0.0..1.0).contains(&t.as_f64()) => dt.format("%H:%M:%S").to_string(),
            Some(dt) if dt.time() == chrono::NaiveTime::MIN => dt.format("%Y-%m-%d").to_string(),
            Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            None => t.to_string(),
        },
        other => other.to_string(),
    }
}

/// Data columns a sheet's table shows, next to its row numbers.
const DATA_COLUMNS: u32 = MAX_COLUMNS as u32 - 1;

/// What is shown of a sheet: the cells in its first [`MAX_ROWS`] rows and
/// [`DATA_COLUMNS`] columns, its formulas, and how far it reaches.
#[derive(Default)]
struct Grid {
    /// The first row and column shown.
    origin: Option<(u32, u32)>,
    /// The first and last used cell seen.
    used: Option<((u32, u32), (u32, u32))>,
    cells: BTreeMap<(u32, u32), String>,
    formulas: Vec<String>,
    more_formulas: usize,
}

impl Grid {
    fn new(origin: Option<(u32, u32)>) -> Self {
        Self {
            origin,
            ..Self::default()
        }
    }

    /// Add a cell; false once it is past the rows shown, as cells come in
    /// row order where they are read as a stream.
    fn add(&mut self, (row, col): (u32, u32), data: &Data, formula: Option<&str>) -> bool {
        let formula = formula.map(str::trim).filter(|f| !f.is_empty());
        if matches!(data, Data::Empty) && formula.is_none() {
            return true;
        }
        let (top, left) = *self.origin.get_or_insert((row, 0));
        self.used = Some(match self.used {
            None => ((row, col), (row, col)),
            Some(((t, l), (b, r))) => ((t.min(row), l.min(col)), (b.max(row), r.max(col))),
        });
        if row >= top.saturating_add(MAX_ROWS as u32) {
            return false;
        }
        if row >= top
            && col >= left
            && col < left.saturating_add(DATA_COLUMNS)
            && !matches!(data, Data::Empty)
        {
            self.cells.insert((row, col), cell_text(&value(data)));
        }
        if let Some(formula) = formula {
            if self.formulas.len() == MAX_FORMULAS {
                self.more_formulas += 1;
            } else {
                let saved = if matches!(data, Data::Empty) {
                    "(no saved value: the file was saved without calculating it)".to_string()
                } else {
                    cell_text(&value(data))
                };
                self.formulas.push(format!(
                    "- {}{}: ={} -> {saved}",
                    column(col),
                    row + 1,
                    cell_text(formula.trim_start_matches('='))
                ));
            }
        }
        true
    }

    /// Write the sheet to `out`, at most until it holds [`MAX_TEXT_BYTES`];
    /// `cut` when the cells were read only up to the rows shown, and
    /// `declared` the extent the file states for the sheet.
    fn render(
        &self,
        out: &mut String,
        title: &str,
        cut: bool,
        declared: Option<((u32, u32), (u32, u32))>,
    ) {
        let _ = write!(out, "## Sheet: {title}\n\n");
        let (Some((top, left)), Some(((first_row, first_col), (last_row, last_col)))) =
            (self.origin, self.used)
        else {
            out.push_str("(empty)\n\n");
            return;
        };
        // Past the rows read, the file's own extent says how far it goes.
        let (last_row, last_col) = match declared {
            Some((_, (r, c))) if cut => (last_row.max(r), last_col.max(c)),
            _ => (last_row, last_col),
        };
        let _ = write!(
            out,
            "Cells {}{}:{}{}, {} rows by {} columns.\n\n",
            column(first_col),
            first_row + 1,
            column(last_col),
            last_row + 1,
            last_row - first_row + 1,
            last_col - first_col + 1
        );
        let right = last_col.min(left.saturating_add(DATA_COLUMNS - 1));
        let shown: Vec<u32> = (left.max(first_col)..=right).collect();
        let mut rows: Vec<Vec<String>> = vec![
            std::iter::once(String::new())
                .chain(shown.iter().map(|c| column(*c)))
                .collect(),
        ];
        let mut row: Option<u32> = None;
        let mut stopped = false;
        let mut size = out.len();
        for &(r, _) in self.cells.keys() {
            if row == Some(r) {
                continue;
            }
            row = Some(r);
            let line: Vec<String> = std::iter::once((r + 1).to_string())
                .chain(
                    shown
                        .iter()
                        .map(|c| self.cells.get(&(r, *c)).cloned().unwrap_or_default()),
                )
                .collect();
            size += line.iter().map(|c| c.len() + 3).sum::<usize>();
            if size > MAX_TEXT_BYTES {
                stopped = true;
                break;
            }
            rows.push(line);
        }
        out.push_str(&table(&rows));
        if stopped {
            out.push_str("\n[the rest of the sheet is not shown: the text is too long]\n");
        } else if cut && declared.is_none() {
            let _ = writeln!(
                out,
                "\n[rows from {} on not shown]",
                top + MAX_ROWS as u32 + 1
            );
        } else if last_row >= top.saturating_add(MAX_ROWS as u32) {
            let _ = writeln!(
                out,
                "\n[rows {} to {} not shown]",
                top + MAX_ROWS as u32 + 1,
                last_row + 1
            );
        }
        if last_col > right {
            let _ = writeln!(
                out,
                "\n[columns {} to {} not shown]",
                column(right + 1),
                column(last_col)
            );
        }
        out.push('\n');
        if !self.formulas.is_empty() {
            let _ = write!(out, "Formulas:\n\n{}\n", self.formulas.join("\n"));
            if self.more_formulas > 0 {
                let _ = writeln!(out, "[{} more formulas not shown]", self.more_formulas);
            }
            out.push('\n');
        }
    }
}

fn sheet_title(name: &str, visible: SheetVisible) -> String {
    match visible {
        SheetVisible::Visible => name.to_string(),
        _ => format!("{name} (hidden)"),
    }
}

/// An .xlsx or .xlsm workbook, read as a stream of cells: a sheet's rows
/// past those shown are never held, however far its cells reach.
fn xlsx_text(input: &[u8]) -> Option<String> {
    let mut book: calamine::Xlsx<_> = calamine::open_workbook_from_rs(Cursor::new(input)).ok()?;
    let sheets: Vec<(String, SheetType, SheetVisible)> = book
        .sheets_metadata()
        .iter()
        .map(|s| (s.name.clone(), s.typ, s.visible))
        .collect();
    let mut out = String::new();
    for (i, (name, kind, visible)) in sheets.iter().enumerate() {
        if i == MAX_SHEETS || out.len() > MAX_TEXT_BYTES {
            let _ = writeln!(out, "[{} more sheets not shown]", sheets.len() - i);
            break;
        }
        let title = sheet_title(name, *visible);
        if *kind != SheetType::WorkSheet {
            let _ = write!(
                out,
                "## Sheet: {title} (a chart or macro sheet, not shown)\n\n"
            );
            continue;
        }
        let mut reader = match book.worksheet_cells_reader(name) {
            Ok(reader) => reader,
            Err(e) => {
                let _ = write!(out, "## Sheet: {title}\n\n(could not be read: {e})\n\n");
                continue;
            }
        };
        let dims = reader.dimensions();
        let declared = (dims.end != (0, 0)).then_some((dims.start, dims.end));
        let mut grid = Grid::new(declared.map(|(start, _)| (start.0, start.1)));
        let mut cut = false;
        let mut failed = None;
        loop {
            match reader.next_cell_with_formula() {
                Ok(Some(cell)) => {
                    let value = Data::from(cell.value);
                    if !grid.add(cell.pos, &value, cell.formula.as_deref()) {
                        cut = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    failed = Some(e.to_string());
                    break;
                }
            }
        }
        grid.render(&mut out, &title, cut, declared);
        if let Some(e) = failed {
            let _ = write!(out, "[the rest of the sheet could not be read: {e}]\n\n");
        }
    }
    Some(out)
}

/// The text of spreadsheet `input` (.xlsx, .xlsm, .xls, .ods) as Markdown:
/// each sheet a table with its row numbers and column letters, its first
/// [`MAX_ROWS`] rows, then its formulas with their saved values.
pub fn sheet_text(input: &[u8]) -> Result<String, String> {
    // Anything that may be a zip, wherever its archive starts, is held to
    // the archive limits; an .xls file is not one.
    const XLS_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
    if !input.starts_with(XLS_MAGIC) {
        open(input)?;
        if let Some(text) = xlsx_text(input) {
            return Ok(text);
        }
    }
    // .xls, .xlsb and .ods: read whole, then shown as far as the limits.
    let mut book = calamine::open_workbook_auto_from_rs(Cursor::new(input))
        .map_err(|e| format!("not a readable spreadsheet: {e}"))?;
    let sheets: Vec<(String, SheetType, SheetVisible)> = book
        .sheets_metadata()
        .iter()
        .map(|s| (s.name.clone(), s.typ, s.visible))
        .collect();
    let mut out = String::new();
    for (i, (name, kind, visible)) in sheets.iter().enumerate() {
        if i == MAX_SHEETS || out.len() > MAX_TEXT_BYTES {
            let _ = writeln!(out, "[{} more sheets not shown]", sheets.len() - i);
            break;
        }
        let title = sheet_title(name, *visible);
        if *kind != SheetType::WorkSheet {
            let _ = write!(
                out,
                "## Sheet: {title} (a chart or macro sheet, not shown)\n\n"
            );
            continue;
        }
        let values = match book.worksheet_range(name) {
            Ok(values) => values,
            Err(e) => {
                let _ = write!(out, "## Sheet: {title}\n\n(could not be read: {e})\n\n");
                continue;
            }
        };
        let formulas = book.worksheet_formula(name).ok();
        let mut grid = Grid::new(values.start());
        let (top, left) = values.start().unwrap_or((0, 0));
        for (r, c, data) in values.used_cells() {
            grid.add((top + r as u32, left + c as u32), data, None);
        }
        if let Some(formulas) = &formulas {
            let (top, left) = formulas.start().unwrap_or((0, 0));
            for (r, c, formula) in formulas.used_cells() {
                let pos = (top + r as u32, left + c as u32);
                let saved = values.get_value(pos).cloned().unwrap_or(Data::Empty);
                if !grid.add(pos, &saved, Some(formula)) {
                    continue;
                }
            }
        }
        grid.render(&mut out, &title, false, None);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::io::Write as _;

    use super::*;

    fn archive(parts: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in parts {
            zip.start_file(*name, options).expect("start");
            zip.write_all(body.as_bytes()).expect("write");
        }
        zip.finish().expect("finish").into_inner()
    }

    const W: &str = r#"xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:wp="http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing" xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006""#;

    #[test]
    fn a_word_document_reads_as_markdown() {
        let document = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document {W}><w:body>
<w:p><w:pPr><w:pStyle w:val="1"/></w:pPr><w:r><w:t>季度报告</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">Sales &amp; costs rose</w:t></w:r><w:r><w:footnoteReference w:id="2"/></w:r><w:r><w:t>.</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>first</w:t></w:r></w:p>
<w:p><w:pPr><w:numPr><w:ilvl w:val="1"/><w:numId w:val="3"/></w:numPr></w:pPr><w:r><w:t>nested</w:t></w:r></w:p>
<w:p><w:r><w:t xml:space="preserve">See </w:t></w:r><w:hyperlink r:id="rId9"><w:r><w:t>the site</w:t></w:r></w:hyperlink></w:p>
<w:p><w:commentRangeStart w:id="0"/><w:r><w:t>Profit</w:t></w:r><w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r>
<w:ins w:id="5" w:author="A"><w:r><w:t xml:space="preserve"> doubled</w:t></w:r></w:ins><w:del w:id="6" w:author="A"><w:r><w:delText> halved</w:delText></w:r></w:del></w:p>
<w:tbl><w:tr><w:tc><w:p><w:r><w:t>Region</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>Q1</w:t></w:r></w:p></w:tc></w:tr>
<w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/></w:tcPr><w:p><w:r><w:t>a|b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
<w:p><w:r><mc:AlternateContent><mc:Choice Requires="wps"><w:drawing><wp:inline><wp:docPr id="1" name="Picture 1" descr="A bar chart"/></wp:inline></w:drawing></mc:Choice><mc:Fallback><w:pict><w:t>again</w:t></w:pict></mc:Fallback></mc:AlternateContent></w:r></w:p>
</w:body></w:document>"#
        );
        let styles = format!(
            r#"<w:styles {W}><w:style w:type="paragraph" w:styleId="1"><w:name w:val="heading 1"/></w:style></w:styles>"#
        );
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId9" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/" TargetMode="External"/></Relationships>"#;
        let footnotes = format!(
            r#"<w:footnotes {W}><w:footnote w:type="separator" w:id="-1"><w:p><w:r><w:t>sep</w:t></w:r></w:p></w:footnote><w:footnote w:id="2"><w:p><w:r><w:t>Unaudited.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let comments = format!(
            r#"<w:comments {W}><w:comment w:id="0" w:author="Li Wei"><w:p><w:r><w:t>Check this.</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let file = archive(&[
            ("word/document.xml", &document),
            ("word/styles.xml", &styles),
            ("word/_rels/document.xml.rels", rels),
            ("word/footnotes.xml", &footnotes),
            ("word/comments.xml", &comments),
        ]);
        let text = docx_text(&file).expect("text");
        assert!(
            text.starts_with("> This document has tracked changes (1 insertions, 1 deletions)"),
            "{text}"
        );
        assert!(text.contains("# 季度报告\n"), "{text}");
        assert!(text.contains("Sales & costs rose[^2]."), "{text}");
        assert!(text.contains("\n- first\n  - nested\n"), "{text}");
        assert!(
            text.contains("See [the site](https://example.com/)"),
            "{text}"
        );
        assert!(text.contains("Profit[comment 0] doubled"), "{text}");
        assert!(!text.contains("halved"), "{text}");
        assert!(
            text.contains("| Region | Q1 |\n| --- | --- |\n| a\\|b |  |\n"),
            "{text}"
        );
        assert!(text.contains("[image: A bar chart]"), "{text}");
        assert!(!text.contains("again"), "the fallback is not read: {text}");
        assert!(text.contains("## Footnotes\n\n[^2]: Unaudited."), "{text}");
        assert!(!text.contains("sep"), "{text}");
        assert!(
            text.contains("- [comment 0] Li Wei, on \"Profit\": Check this."),
            "{text}"
        );
    }

    #[test]
    fn a_spreadsheet_reads_as_tables_with_its_formulas() {
        let workbook = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Budget" sheetId="1" r:id="rId1"/><sheet name="Secret" sheetId="2" state="hidden" r:id="rId2"/></sheets></workbook>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/></Relationships>"#;
        let sheet1 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
<row r="1"><c r="A1" t="inlineStr"><is><t>Item</t></is></c><c r="B1" t="inlineStr"><is><t>Cost</t></is></c></row>
<row r="2"><c r="A2" t="inlineStr"><is><t>Rent | office</t></is></c><c r="B2"><v>1200</v></c></row>
<row r="3"><c r="A3" t="inlineStr"><is><t>Total</t></is></c><c r="B3"><f>SUM(B2:B2)</f><v>1200</v></c><c r="C3"><f>B3*2</f></c></row>
</sheetData></worksheet>"#;
        let sheet2 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData/></worksheet>"#;
        let types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
        let root = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        let file = archive(&[
            ("[Content_Types].xml", types),
            ("_rels/.rels", root),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/worksheets/sheet1.xml", sheet1),
            ("xl/worksheets/sheet2.xml", sheet2),
        ]);
        let text = sheet_text(&file).expect("text");
        assert!(
            text.contains("## Sheet: Budget\n\nCells A1:C3, 3 rows by 3 columns."),
            "{text}"
        );
        assert!(
            text.contains("|  | A | B | C |\n| --- | --- | --- | --- |\n| 1 | Item | Cost |  |\n| 2 | Rent \\| office | 1200 |  |\n| 3 | Total | 1200 |  |\n"),
            "{text}"
        );
        assert!(text.contains("- B3: =SUM(B2:B2) -> 1200"), "{text}");
        assert!(text.contains("- C3: =B3*2 -> (no saved value"), "{text}");
        assert!(
            text.contains("## Sheet: Secret (hidden)\n\n(empty)"),
            "{text}"
        );
    }

    #[test]
    fn an_archive_past_the_limits_is_refused() {
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("f{i}")).collect();
        let parts: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "")).collect();
        let err = docx_text(&archive(&parts)).expect_err("too many parts");
        assert!(err.contains("more than"), "{err}");
        assert!(docx_text(b"not a zip").is_err());
        assert!(sheet_text(b"not a sheet").is_err());
        assert_eq!(column(0), "A");
        assert_eq!(column(25), "Z");
        assert_eq!(column(26), "AA");
        assert_eq!(column(701), "ZZ");
        assert_eq!(column(702), "AAA");
    }

    fn word(body: &str, extra: &[(&str, &str)]) -> Vec<u8> {
        let document = format!("<w:document {W}><w:body>{body}</w:body></w:document>");
        let mut parts: Vec<(&str, &str)> = vec![("word/document.xml", &document)];
        parts.extend_from_slice(extra);
        archive(&parts)
    }

    #[test]
    fn tracked_formatting_changes_show_accepted() {
        let styles = format!(
            r#"<w:styles {W}><w:style w:type="paragraph" w:styleId="2"><w:name w:val="heading 2"/></w:style></w:styles>"#
        );
        let body = r#"<w:p><w:pPr><w:pStyle w:val="2"/><w:pPrChange w:id="1" w:author="A"><w:pPr><w:numPr><w:ilvl w:val="0"/><w:numId w:val="4"/></w:numPr></w:pPr></w:pPrChange></w:pPr><w:r><w:t>Now a heading</w:t></w:r></w:p>
<w:tbl><w:tr><w:tc><w:tcPr><w:gridSpan w:val="2"/><w:tcPrChange w:id="2"><w:tcPr><w:gridSpan w:val="5"/></w:tcPr></w:tcPrChange></w:tcPr><w:p><w:r><w:t>wide</w:t></w:r></w:p></w:tc></w:tr></w:tbl>"#;
        let text = docx_text(&word(body, &[("word/styles.xml", &styles)])).expect("text");
        assert!(text.contains("## Now a heading\n"), "{text}");
        assert!(!text.contains("- Now"), "{text}");
        assert!(text.contains("| wide |  |\n| --- | --- |\n"), "{text}");
    }

    #[test]
    fn a_text_box_keeps_its_table_and_the_run_around_it() {
        let body = r#"<w:p><w:r><w:t>Before</w:t></w:r><w:r><w:pict><w:txbxContent><w:p><w:pPr><w:tabs><w:tab w:val="left" w:pos="720"/></w:tabs></w:pPr><w:r><w:t>Boxed</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>In box</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:txbxContent></w:pict><w:tab/><w:t>After</w:t></w:r></w:p>"#;
        let text = docx_text(&word(body, &[])).expect("text");
        assert_eq!(text.trim(), "Before\nBoxed\nIn box\tAfter", "{text}");
    }

    #[test]
    fn parts_named_from_the_package_root_are_found() {
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/footnotes" Target="/word/notes.xml"/></Relationships>"#;
        let notes = format!(
            r#"<w:footnotes {W}><w:footnote w:id="1"><w:p><w:r><w:t>From the root.</w:t></w:r></w:p></w:footnote></w:footnotes>"#
        );
        let body =
            r#"<w:p><w:r><w:t>Text</w:t></w:r><w:r><w:footnoteReference w:id="1"/></w:r></w:p>"#;
        let text = docx_text(&word(
            body,
            &[
                ("word/_rels/document.xml.rels", rels),
                ("word/notes.xml", &notes),
            ],
        ))
        .expect("text");
        assert!(text.contains("[^1]: From the root."), "{text}");
    }

    #[test]
    fn a_comment_quotes_at_most_the_start_of_its_text() {
        let long = "x".repeat(5000);
        let body = format!(
            r#"<w:p><w:commentRangeStart w:id="0"/><w:r><w:t>{long}</w:t></w:r><w:commentRangeEnd w:id="0"/><w:r><w:commentReference w:id="0"/></w:r></w:p>"#
        );
        let comments = format!(
            r#"<w:comments {W}><w:comment w:id="0" w:author="B"><w:p><w:r><w:t>Long.</w:t></w:r></w:p></w:comment></w:comments>"#
        );
        let text = docx_text(&word(&body, &[("word/comments.xml", &comments)])).expect("text");
        let line = text
            .lines()
            .find(|l| l.starts_with("- [comment 0]"))
            .expect("comment");
        assert_eq!(
            line,
            format!(
                "- [comment 0] B, on \"{}\": Long.",
                "x".repeat(MAX_ANCHOR_CHARS)
            )
        );
    }

    fn workbook(sheet: &str) -> Vec<u8> {
        let workbook = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let types = r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/></Types>"#;
        let sheet = format!(
            r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">{sheet}</worksheet>"#
        );
        let root = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#;
        archive(&[
            ("[Content_Types].xml", types),
            ("_rels/.rels", root),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/worksheets/sheet1.xml", &sheet),
        ])
    }

    #[test]
    fn a_sheet_shows_its_first_columns_and_rows_and_says_what_it_leaves_out() {
        let cells: String = (0..60)
            .map(|c| format!(r#"<c r="{}1"><v>{c}</v></c>"#, column(c)))
            .collect();
        let text = sheet_text(&workbook(&format!(
            r#"<sheetData><row r="1">{cells}</row></sheetData>"#
        )))
        .expect("text");
        assert!(text.contains("| BG |\n"), "{text}");
        assert!(text.contains("| 58 |\n"), "{text}");
        assert!(text.contains("[columns BH to BH not shown]"), "{text}");

        let rows: String = (1..=1200)
            .map(|r| format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>"#))
            .collect();
        let declared = sheet_text(&workbook(&format!(
            r#"<dimension ref="A1:A1200"/><sheetData>{rows}</sheetData>"#
        )))
        .expect("text");
        assert!(
            declared.contains("Cells A1:A1200, 1200 rows by 1 columns."),
            "{declared}"
        );
        assert!(declared.contains("| 1000 | 1000 |\n"), "{declared}");
        assert!(!declared.contains("| 1001 |"), "{declared}");
        assert!(
            declared.contains("[rows 1001 to 1200 not shown]"),
            "{declared}"
        );
        let undeclared =
            sheet_text(&workbook(&format!("<sheetData>{rows}</sheetData>"))).expect("text");
        assert!(
            undeclared.contains("[rows from 1001 on not shown]"),
            "{undeclared}"
        );
    }

    #[test]
    fn times_dates_and_durations_read_as_such() {
        use calamine::{ExcelDateTime, ExcelDateTimeType};
        let at = |v: f64, kind: ExcelDateTimeType| {
            value(&Data::DateTime(ExcelDateTime::new(v, kind, false)))
        };
        assert_eq!(at(0.375, ExcelDateTimeType::DateTime), "09:00:00");
        assert_eq!(at(45000.0, ExcelDateTimeType::DateTime), "2023-03-15");
        assert_eq!(
            at(45000.5, ExcelDateTimeType::DateTime),
            "2023-03-15 12:00:00"
        );
        assert_eq!(at(1.5, ExcelDateTimeType::TimeDelta), "36:00:00");
    }

    #[test]
    fn a_zip_after_other_bytes_is_held_to_the_limits() {
        let names: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("f{i}")).collect();
        let parts: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "")).collect();
        let mut file = b"XX".to_vec();
        file.extend(archive(&parts));
        let err = sheet_text(&file).expect_err("too many parts");
        assert!(err.contains("more than"), "{err}");
    }
}
