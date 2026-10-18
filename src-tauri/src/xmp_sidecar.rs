use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use quick_xml::XmlVersion;
use quick_xml::escape::{escape, unescape};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{NamespaceResolver, PrefixDeclaration, ResolveResult};
use quick_xml::reader::NsReader;
use serde::{Deserialize, Serialize};

use crate::file_management::write_atomic;
use crate::image_processing::{ImageFlag, ImageMetadata, default_adjustments};
use crate::tagging::{COLOR_TAG_PREFIX, USER_TAG_PREFIX};

const NS_RDF: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/";
const NS_DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const NS_X: &[u8] = b"adobe:ns:meta/";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct XmpFields {
    pub rating: Option<i8>,
    pub label: Option<String>,
    pub subject: Vec<String>,
    pub rating_invalid: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Meta,
    Rdf,
    Desc,
    Rating,
    Label,
    Subject,
    Container,
    Li,
    Ignored,
}

type Ns = Option<Vec<u8>>;

fn bound(res: ResolveResult) -> Result<Ns, String> {
    match res {
        ResolveResult::Bound(ns) => Ok(Some(ns.into_inner().to_vec())),
        ResolveResult::Unbound => Ok(None),
        ResolveResult::Unknown(prefix) => Err(format!(
            "unbound namespace prefix '{}'",
            String::from_utf8_lossy(&prefix)
        )),
    }
}

fn is_name(ns: &Ns, uri: &[u8], local: &[u8], expected: &[&[u8]]) -> bool {
    ns.as_deref() == Some(uri) && expected.contains(&local)
}

#[derive(Default)]
struct Collector {
    fields: XmpFields,
    stack: Vec<Ctx>,
    text: String,
    root_seen: bool,
    root_closed: bool,
    desc_props: Vec<Ctx>,
    rating_unparsed: bool,
}

fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

impl Collector {
    fn set_rating(&mut self, value: &str) {
        if self.fields.rating.is_none() {
            match value.trim().parse::<i8>() {
                Ok(rating) => self.fields.rating = Some(rating),
                Err(_) => self.rating_unparsed = true,
            }
        }
    }

    fn set_label(&mut self, value: &str) {
        let value = value.trim();
        if self.fields.label.is_none() && !value.is_empty() {
            self.fields.label = Some(value.to_string());
        }
    }

    fn mark(&mut self, prop: Ctx) -> Result<(), String> {
        if self.desc_props.contains(&prop) {
            return Err("duplicate property in rdf:Description".to_string());
        }
        self.desc_props.push(prop);
        Ok(())
    }

    fn open(
        &mut self,
        resolver: &NamespaceResolver,
        ns: &Ns,
        e: &BytesStart,
    ) -> Result<(), String> {
        let local_name = e.local_name();
        let local = local_name.as_ref();
        let ctx = match self.stack.last().copied() {
            None => {
                if self.root_seen {
                    return Err("more than one root element".to_string());
                }
                self.root_seen = true;
                if is_name(ns, NS_X, local, &[b"xmpmeta", b"xapmeta"]) {
                    Ctx::Meta
                } else if is_name(ns, NS_RDF, local, &[b"RDF"]) {
                    Ctx::Rdf
                } else {
                    return Err("not an XMP packet".to_string());
                }
            }
            Some(Ctx::Meta) => {
                if is_name(ns, NS_RDF, local, &[b"RDF"]) {
                    Ctx::Rdf
                } else {
                    Ctx::Ignored
                }
            }
            Some(Ctx::Rdf) => {
                if is_name(ns, NS_RDF, local, &[b"Description"]) {
                    Ctx::Desc
                } else {
                    Ctx::Ignored
                }
            }
            Some(Ctx::Desc) => {
                if is_name(ns, NS_XMP, local, &[b"Rating"]) {
                    Ctx::Rating
                } else if is_name(ns, NS_XMP, local, &[b"Label"]) {
                    Ctx::Label
                } else if is_name(ns, NS_DC, local, &[b"subject"]) {
                    Ctx::Subject
                } else {
                    Ctx::Ignored
                }
            }
            Some(Ctx::Subject) => {
                if is_name(ns, NS_RDF, local, &[b"Bag", b"Seq", b"Alt"]) {
                    Ctx::Container
                } else {
                    return Err("unsupported dc:subject content".to_string());
                }
            }
            Some(Ctx::Container) => {
                if is_name(ns, NS_RDF, local, &[b"li"]) {
                    Ctx::Li
                } else {
                    return Err("unsupported dc:subject list content".to_string());
                }
            }
            Some(Ctx::Li) | Some(Ctx::Rating) | Some(Ctx::Label) => {
                return Err("unexpected child element in a text property".to_string());
            }
            Some(Ctx::Ignored) => Ctx::Ignored,
        };

        match ctx {
            Ctx::Desc => self.desc_props.clear(),
            Ctx::Rating | Ctx::Label | Ctx::Subject => self.mark(ctx)?,
            _ => {}
        }

        for attr in e.attributes() {
            let attr = attr.map_err(|err| err.to_string())?;
            let key = attr.key.as_ref();
            if key == b"xmlns" || key.starts_with(b"xmlns:") {
                continue;
            }
            let (res, local_attr) = resolver.resolve_attribute(attr.key);
            let attr_ns = bound(res)?;
            let local_attr = local_attr.as_ref();
            let value = attr
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|err| err.to_string())?;
            match ctx {
                Ctx::Desc => {
                    if is_name(&attr_ns, NS_XMP, local_attr, &[b"Rating"]) {
                        self.mark(Ctx::Rating)?;
                        self.set_rating(&value);
                    } else if is_name(&attr_ns, NS_XMP, local_attr, &[b"Label"]) {
                        self.mark(Ctx::Label)?;
                        self.set_label(&value);
                    } else if is_name(&attr_ns, NS_DC, local_attr, &[b"subject"]) {
                        return Err("dc:subject as an attribute".to_string());
                    }
                }
                Ctx::Rating | Ctx::Label | Ctx::Subject | Ctx::Container | Ctx::Li
                    if attr_ns.as_deref() == Some(NS_RDF) =>
                {
                    return Err("unsupported rdf attribute on a property".to_string());
                }
                _ => {}
            }
        }

        self.stack.push(ctx);
        if matches!(ctx, Ctx::Rating | Ctx::Label | Ctx::Li) {
            self.text.clear();
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), String> {
        let ctx = self
            .stack
            .pop()
            .ok_or_else(|| "unexpected end tag".to_string())?;
        if matches!(ctx, Ctx::Rating | Ctx::Label | Ctx::Li) {
            let raw = std::mem::take(&mut self.text);
            let decoded = unescape(&raw).map_err(|err| err.to_string())?;
            let value = decoded.trim();
            if !value.is_empty() {
                match ctx {
                    Ctx::Rating => self.set_rating(value),
                    Ctx::Label => self.set_label(value),
                    _ => {
                        if !self.fields.subject.iter().any(|t| t == value) {
                            self.fields.subject.push(value.to_string());
                        }
                    }
                }
            }
        }
        if self.stack.is_empty() {
            self.root_closed = true;
        }
        Ok(())
    }

    fn text(&mut self, raw: &str) -> Result<(), String> {
        let allowed: fn(char) -> bool = match self.stack.last() {
            Some(Ctx::Rating | Ctx::Label | Ctx::Li) => {
                self.text.push_str(raw);
                return Ok(());
            }
            Some(Ctx::Subject | Ctx::Container) => is_xml_space,
            None if self.root_closed => |c| is_xml_space(c) || c == '\0',
            None => |c| is_xml_space(c) || c == '\u{feff}',
            _ => return Ok(()),
        };
        if raw.chars().all(allowed) {
            Ok(())
        } else {
            Err("unexpected text".to_string())
        }
    }
}

pub fn parse_xmp(content: &str) -> Result<XmpFields, String> {
    let mut reader = NsReader::from_str(content);
    let mut state = Collector::default();

    loop {
        let (res, event) = reader.read_resolved_event().map_err(|e| e.to_string())?;
        let ns = bound(res)?;
        match event {
            Event::Start(e) => state.open(reader.resolver(), &ns, &e)?,
            Event::Empty(e) => {
                state.open(reader.resolver(), &ns, &e)?;
                state.close()?;
            }
            Event::End(_) => state.close()?,
            Event::Text(e) => state.text(&e.decode().map_err(|err| err.to_string())?)?,
            Event::CData(e) => {
                let decoded = e.decode().map_err(|err| err.to_string())?;
                state.text(&escape(decoded))?;
            }
            Event::GeneralRef(e) => {
                let name = e.decode().map_err(|err| err.to_string())?;
                let entity = format!("&{};", name);
                unescape(&entity).map_err(|err| err.to_string())?;
                state.text(&entity)?;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !state.root_seen {
        return Err("not an XMP packet".to_string());
    }
    if !state.root_closed || !state.stack.is_empty() {
        return Err("unexpected end of file".to_string());
    }
    state.fields.rating_invalid = match state.fields.rating {
        Some(rating) => rating > 5,
        None => state.rating_unparsed,
    };
    Ok(state.fields)
}

pub fn read_xmp_file(path: &Path) -> Result<XmpFields, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    parse_xmp(&content).map_err(|e| format!("failed to parse {}: {}", path.display(), e))
}

pub const XMP_REJECTED_RATING: i8 = -1;

pub fn xmp_rating(metadata: &ImageMetadata) -> String {
    if metadata.flag == Some(ImageFlag::Reject) {
        XMP_REJECTED_RATING.to_string()
    } else {
        metadata.rating.to_string()
    }
}

pub fn xmp_label(tags: &[String]) -> Option<String> {
    tags.iter()
        .rev()
        .find_map(|t| t.strip_prefix(COLOR_TAG_PREFIX))
        .map(|color| {
            let mut c = color.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
            }
        })
}

pub fn xmp_subject_bag<S: AsRef<str>>(items: &[S]) -> String {
    let mut bag = String::from("<dc:subject>\n    <rdf:Bag>\n");
    for item in items {
        bag.push_str(&format!("     <rdf:li>{}</rdf:li>\n", item.as_ref()));
    }
    bag.push_str("    </rdf:Bag>\n   </dc:subject>");
    bag
}

pub fn resolve_xmp_path(image_path: &Path) -> Option<PathBuf> {
    let xmp_path = image_path.with_extension("xmp");
    let xmp_path_upper = image_path.with_extension("XMP");
    if xmp_path.exists() {
        Some(xmp_path)
    } else if xmp_path_upper.exists() {
        Some(xmp_path_upper)
    } else {
        None
    }
}

const RAPIDRAW_COLORS: [&str; 5] = ["red", "yellow", "green", "blue", "purple"];

const NS_XMP_STR: &str = "http://ns.adobe.com/xap/1.0/";
const NS_DC_STR: &str = "http://purl.org/dc/elements/1.1/";

const NEW_XMP: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RapidRAW">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

pub(crate) fn normalize_label(value: &str) -> Option<String> {
    let label = value.trim().to_lowercase();
    RAPIDRAW_COLORS.contains(&label.as_str()).then_some(label)
}

fn valid_xml_text(value: &str) -> bool {
    value.chars().all(|c| {
        (c >= ' ' || matches!(c, '\t' | '\n' | '\r')) && c != '\u{fffe}' && c != '\u{ffff}'
    })
}

pub(crate) struct XmpValues {
    pub rating: i8,
    pub label: Option<String>,
    pub keywords: Vec<String>,
}

impl XmpValues {
    pub(crate) fn from_metadata(metadata: &ImageMetadata) -> Self {
        let tags = metadata.tags.clone().unwrap_or_default();
        let label = xmp_label(&tags).filter(|label| {
            if label.trim().is_empty() {
                return false;
            }
            let valid = valid_xml_text(label);
            if !valid {
                log::warn!("Dropping XMP label with characters invalid in XML");
            }
            valid
        });
        XmpValues {
            rating: xmp_rating(metadata).parse().unwrap_or(0),
            label,
            keywords: xmp_keywords(&tags),
        }
    }
}

pub(crate) fn xmp_keywords(tags: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut keywords = Vec::new();
    for tag in tags {
        if tag.starts_with(COLOR_TAG_PREFIX) {
            continue;
        }
        let keyword = tag.strip_prefix(USER_TAG_PREFIX).unwrap_or(tag).trim();
        if keyword.is_empty() {
            continue;
        }
        if !valid_xml_text(keyword) {
            log::warn!("Dropping XMP keyword with characters invalid in XML");
            continue;
        }
        if seen.insert(keyword.to_string()) {
            keywords.push(keyword.to_string());
        }
    }
    keywords
}

#[derive(Debug)]
pub(crate) enum XmpWriteError {
    Parse(String),
    Unsupported(String),
    PrefixClash(String),
    Verify(String),
}

impl std::fmt::Display for XmpWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XmpWriteError::Parse(m) => write!(f, "cannot parse: {}", m),
            XmpWriteError::Unsupported(m) => write!(f, "unsupported shape: {}", m),
            XmpWriteError::PrefixClash(m) => write!(f, "namespace prefix clash: {}", m),
            XmpWriteError::Verify(m) => write!(f, "output check failed: {}", m),
        }
    }
}

type WriteResult<T> = Result<T, XmpWriteError>;

fn parse_err<E: std::fmt::Display>(e: E) -> XmpWriteError {
    XmpWriteError::Parse(e.to_string())
}

fn unsupported(msg: &str) -> XmpWriteError {
    XmpWriteError::Unsupported(msg.to_string())
}

struct AttrLoc {
    whole: Range<usize>,
    value: Range<usize>,
}

enum ElemBody {
    Content(Range<usize>),
    EmptyEnd(Range<usize>),
}

struct ElemLoc {
    whole: Range<usize>,
    body: ElemBody,
    qname: String,
}

enum Loc {
    Attr(AttrLoc),
    Elem(ElemLoc),
}

struct Occ {
    loc: Loc,
    decoded: String,
}

struct SubjectOcc {
    elem: ElemLoc,
    values: Vec<String>,
    desc: usize,
}

#[derive(Default)]
struct DescInfo {
    tag_end: usize,
    attrs_end: usize,
    self_closing: bool,
    end_tag: Option<Range<usize>>,
    qname: String,
    multiline: bool,
    indent: Option<String>,
    attr_indent: Option<String>,
    child_indent: Option<String>,
    quote: u8,
    rdf: Option<String>,
    xmp: Option<String>,
    dc: Option<String>,
    xmp_clash: bool,
    dc_clash: bool,
}

#[derive(Default)]
struct RdfInfo {
    prefix: Option<String>,
    end_tag: Option<Range<usize>>,
}

#[derive(Default)]
struct Scan {
    descs: Vec<DescInfo>,
    ratings: Vec<Occ>,
    labels: Vec<Occ>,
    subjects: Vec<SubjectOcc>,
    rdf: Option<RdfInfo>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Meta,
    Rdf,
    Desc,
    Rating,
    Label,
    Subject,
    Container,
    Li,
    Other,
}

fn offset(content: &str, slice: &[u8]) -> WriteResult<usize> {
    let base = content.as_ptr() as usize;
    let start = slice.as_ptr() as usize;
    if start < base || start + slice.len() > base + content.len() {
        return Err(parse_err("slice does not borrow from the input"));
    }
    Ok(start - base)
}

fn line_indent(content: &str, pos: usize) -> Option<String> {
    let bytes = content.as_bytes();
    let mut i = pos;
    while i > 0 && matches!(bytes[i - 1], b' ' | b'\t') {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] == b'\n' {
        Some(content[i..pos].to_string())
    } else {
        None
    }
}

fn find_gt(content: &str, from: usize) -> WriteResult<usize> {
    content[from..]
        .find('>')
        .map(|p| from + p + 1)
        .ok_or_else(|| parse_err("unterminated tag"))
}

fn utf8(bytes: &[u8]) -> WriteResult<&str> {
    std::str::from_utf8(bytes).map_err(parse_err)
}

fn normalize_values<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        let value = value.trim();
        if !value.is_empty() && seen.insert(value.to_string()) {
            out.push(value.to_string());
        }
    }
    out
}

struct Cursor {
    start: usize,
    tag_end: usize,
    qname: String,
    desc: usize,
    text: String,
    values: Vec<String>,
}

struct Scanner<'a> {
    content: &'a str,
    scan: Scan,
    stack: Vec<Kind>,
    cur: Option<Cursor>,
}

impl<'a> Scanner<'a> {
    fn open(
        &mut self,
        resolver: &NamespaceResolver,
        ns: &Ns,
        e: &BytesStart<'a>,
        empty: bool,
    ) -> WriteResult<()> {
        let content = self.content;
        let name_start = offset(content, e.name().as_ref())?;
        let name_end = name_start + e.name().as_ref().len();
        let tag_start = name_start - 1;
        let qname = utf8(e.name().as_ref())?.to_string();
        let local_name = e.local_name();
        let local = local_name.as_ref();

        let kind = match self.stack.last().copied() {
            None => {
                if is_name(ns, NS_RDF, local, &[b"RDF"]) {
                    Kind::Rdf
                } else {
                    Kind::Meta
                }
            }
            Some(Kind::Meta) => {
                if is_name(ns, NS_RDF, local, &[b"RDF"]) {
                    Kind::Rdf
                } else {
                    Kind::Other
                }
            }
            Some(Kind::Rdf) => {
                if is_name(ns, NS_RDF, local, &[b"Description"]) {
                    Kind::Desc
                } else {
                    Kind::Other
                }
            }
            Some(Kind::Desc) => {
                if is_name(ns, NS_XMP, local, &[b"Rating"]) {
                    Kind::Rating
                } else if is_name(ns, NS_XMP, local, &[b"Label"]) {
                    Kind::Label
                } else if is_name(ns, NS_DC, local, &[b"subject"]) {
                    Kind::Subject
                } else {
                    Kind::Other
                }
            }
            Some(Kind::Subject) => Kind::Container,
            Some(Kind::Container) => Kind::Li,
            Some(Kind::Rating | Kind::Label | Kind::Li) => {
                return Err(unsupported("child element in a text property"));
            }
            Some(Kind::Other) => Kind::Other,
        };

        if self.stack.last() == Some(&Kind::Desc) && self.scan_child_indent_missing() {
            let indent = line_indent(content, tag_start);
            if let Some(desc) = self.scan.descs.last_mut() {
                desc.child_indent = indent;
            }
        }

        match kind {
            Kind::Rdf => {
                if empty {
                    return Err(unsupported("self-closing rdf:RDF"));
                }
                if self.scan.rdf.is_none() {
                    let prefix = qname.split_once(':').map(|(p, _)| p.to_string());
                    self.scan.rdf = Some(RdfInfo {
                        prefix,
                        end_tag: None,
                    });
                }
            }
            Kind::Desc => self.open_desc(resolver, e, tag_start, name_end, qname, empty)?,
            Kind::Rating | Kind::Label | Kind::Subject => {
                let attrs_end = self.attrs_end(e, name_end)?;
                let tag_end = find_gt(content, attrs_end)?;
                self.cur = Some(Cursor {
                    start: tag_start,
                    tag_end,
                    qname,
                    desc: self.scan.descs.len().saturating_sub(1),
                    text: String::new(),
                    values: Vec::new(),
                });
            }
            Kind::Li => {
                if let Some(cur) = self.cur.as_mut() {
                    cur.text.clear();
                }
            }
            _ => {}
        }

        self.stack.push(kind);
        Ok(())
    }

    fn scan_child_indent_missing(&self) -> bool {
        self.scan
            .descs
            .last()
            .is_some_and(|d| d.child_indent.is_none())
    }

    fn attrs_end(&self, e: &BytesStart<'a>, name_end: usize) -> WriteResult<usize> {
        let mut end = name_end;
        for attr in e.attributes() {
            let attr = attr.map_err(parse_err)?;
            let value = offset(self.content, &attr.value)?;
            end = value + attr.value.len() + 1;
        }
        Ok(end)
    }

    fn open_desc(
        &mut self,
        resolver: &NamespaceResolver,
        e: &BytesStart<'a>,
        tag_start: usize,
        name_end: usize,
        qname: String,
        empty: bool,
    ) -> WriteResult<()> {
        let content = self.content;
        let mut desc = DescInfo {
            self_closing: empty,
            qname,
            quote: b'"',
            indent: line_indent(content, tag_start),
            ..DescInfo::default()
        };

        let mut attrs_end = name_end;
        for attr in e.attributes() {
            let attr = attr.map_err(parse_err)?;
            let key_start = offset(content, attr.key.as_ref())?;
            let value_start = offset(content, &attr.value)?;
            let value_end = value_start + attr.value.len();
            desc.quote = content.as_bytes()[value_start - 1];
            if let Some(indent) = line_indent(content, key_start) {
                desc.attr_indent = Some(indent);
            }
            attrs_end = value_end + 1;

            let key = attr.key.as_ref();
            if key == b"xmlns" || key.starts_with(b"xmlns:") {
                continue;
            }
            let (res, local) = resolver.resolve_attribute(attr.key);
            let attr_ns = bound(res).map_err(parse_err)?;
            let decoded = unescape(utf8(&attr.value)?).map_err(parse_err)?;
            let occ = Occ {
                loc: Loc::Attr(AttrLoc {
                    whole: key_start..value_end + 1,
                    value: value_start..value_end,
                }),
                decoded: decoded.trim().to_string(),
            };
            if is_name(&attr_ns, NS_XMP, local.as_ref(), &[b"Rating"]) {
                self.scan.ratings.push(occ);
            } else if is_name(&attr_ns, NS_XMP, local.as_ref(), &[b"Label"]) {
                self.scan.labels.push(occ);
            }
        }

        desc.attrs_end = attrs_end;
        desc.tag_end = find_gt(content, attrs_end)?;
        desc.multiline = content[tag_start..desc.tag_end].contains('\n');

        for (decl, namespace) in resolver.bindings() {
            let PrefixDeclaration::Named(prefix) = decl else {
                continue;
            };
            let prefix = utf8(prefix)?.to_string();
            let uri = namespace.into_inner();
            if uri == NS_RDF && desc.rdf.is_none() {
                desc.rdf = Some(prefix.clone());
            }
            if uri == NS_XMP && desc.xmp.is_none() {
                desc.xmp = Some(prefix.clone());
            }
            if uri == NS_DC && desc.dc.is_none() {
                desc.dc = Some(prefix.clone());
            }
            if prefix == "xmp" && uri != NS_XMP {
                desc.xmp_clash = true;
            }
            if prefix == "dc" && uri != NS_DC {
                desc.dc_clash = true;
            }
        }

        self.scan.descs.push(desc);
        Ok(())
    }

    fn close(
        &mut self,
        end: Option<(usize, usize)>,
        empty_end: Option<Range<usize>>,
    ) -> WriteResult<()> {
        let kind = self
            .stack
            .pop()
            .ok_or_else(|| parse_err("unexpected end tag"))?;
        match kind {
            Kind::Rdf => {
                if let (Some((start, stop)), Some(rdf)) = (end, self.scan.rdf.as_mut())
                    && rdf.end_tag.is_none()
                {
                    rdf.end_tag = Some(start..stop);
                }
            }
            Kind::Desc => {
                if let (Some((start, stop)), Some(desc)) = (end, self.scan.descs.last_mut()) {
                    desc.end_tag = Some(start..stop);
                }
            }
            Kind::Li => {
                if let Some(cur) = self.cur.as_mut() {
                    let raw = std::mem::take(&mut cur.text);
                    let decoded = unescape(&raw).map_err(parse_err)?;
                    cur.values.push(decoded.to_string());
                }
            }
            Kind::Rating | Kind::Label | Kind::Subject => {
                let cur = self
                    .cur
                    .take()
                    .ok_or_else(|| parse_err("property without cursor"))?;
                let (whole, body) = match (end, empty_end) {
                    (Some((start, stop)), _) => {
                        (cur.start..stop, ElemBody::Content(cur.tag_end..start))
                    }
                    (None, Some(range)) => (cur.start..range.end, ElemBody::EmptyEnd(range)),
                    (None, None) => return Err(parse_err("property without end")),
                };
                let elem = ElemLoc {
                    whole,
                    body,
                    qname: cur.qname,
                };
                match kind {
                    Kind::Subject => self.scan.subjects.push(SubjectOcc {
                        elem,
                        values: normalize_values(cur.values.iter().map(String::as_str)),
                        desc: cur.desc,
                    }),
                    _ => {
                        let raw = unescape(&cur.text).map_err(parse_err)?;
                        let occ = Occ {
                            loc: Loc::Elem(elem),
                            decoded: raw.trim().to_string(),
                        };
                        if kind == Kind::Rating {
                            self.scan.ratings.push(occ);
                        } else {
                            self.scan.labels.push(occ);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str) {
        if matches!(
            self.stack.last(),
            Some(Kind::Rating | Kind::Label | Kind::Li)
        ) && let Some(cur) = self.cur.as_mut()
        {
            cur.text.push_str(text);
        }
    }
}

fn scan_xmp(content: &str) -> WriteResult<Scan> {
    let mut reader = NsReader::from_str(content);
    let mut scanner = Scanner {
        content,
        scan: Scan::default(),
        stack: Vec::new(),
        cur: None,
    };

    loop {
        let (res, event) = reader.read_resolved_event().map_err(parse_err)?;
        let ns = bound(res).map_err(parse_err)?;
        if matches!(scanner.stack.last(), Some(Kind::Rating | Kind::Label))
            && matches!(event, Event::Comment(_) | Event::PI(_) | Event::DocType(_))
        {
            return Err(unsupported("markup inside xmp:Rating or xmp:Label"));
        }
        match event {
            Event::Decl(d) => {
                if let Some(enc) = d.encoding() {
                    let enc = enc.map_err(parse_err)?;
                    if !enc.eq_ignore_ascii_case(b"utf-8") {
                        return Err(unsupported("encoding other than UTF-8"));
                    }
                }
            }
            Event::Start(e) => scanner.open(reader.resolver(), &ns, &e, false)?,
            Event::Empty(e) => {
                scanner.open(reader.resolver(), &ns, &e, true)?;
                let name_end = offset(content, e.name().as_ref())? + e.name().as_ref().len();
                let attrs_end = scanner.attrs_end(&e, name_end)?;
                let tag_end = find_gt(content, attrs_end)?;
                scanner.close(None, Some(attrs_end..tag_end))?;
            }
            Event::End(e) => {
                let name_start = offset(content, e.name().as_ref())?;
                let end = find_gt(content, name_start + e.name().as_ref().len())?;
                scanner.close(Some((name_start - 2, end)), None)?;
            }
            Event::Text(t) => scanner.push_text(&t.decode().map_err(parse_err)?),
            Event::CData(t) => {
                let decoded = t.decode().map_err(parse_err)?;
                scanner.push_text(&escape(decoded));
            }
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(parse_err)?;
                scanner.push_text(&format!("&{};", name));
            }
            Event::Eof => break,
            _ => {}
        }
    }

    Ok(scanner.scan)
}

struct Edit {
    range: Range<usize>,
    text: String,
}

struct Editor<'a> {
    content: &'a str,
    edits: Vec<Edit>,
}

impl Editor<'_> {
    fn replace(&mut self, range: Range<usize>, text: String) {
        self.edits.push(Edit { range, text });
    }

    fn set_value(&mut self, loc: &Loc, text: &str) {
        match loc {
            Loc::Attr(a) => self.replace(a.value.clone(), text.to_string()),
            Loc::Elem(el) => match &el.body {
                ElemBody::Content(range) => self.replace(range.clone(), text.to_string()),
                ElemBody::EmptyEnd(range) => {
                    self.replace(range.clone(), format!(">{}</{}>", text, el.qname))
                }
            },
        }
    }

    fn remove(&mut self, range: Range<usize>) {
        let bytes = self.content.as_bytes();
        let mut start = range.start;
        while start > 0 && matches!(bytes[start - 1], b' ' | b'\t') {
            start -= 1;
        }
        if start > 0 && bytes[start - 1] == b'\n' {
            start -= 1;
            if start > 0 && bytes[start - 1] == b'\r' {
                start -= 1;
            }
        }
        self.edits.push(Edit {
            range: start..range.end,
            text: String::new(),
        });
    }

    fn apply(mut self) -> WriteResult<String> {
        self.edits.sort_by_key(|e| (e.range.start, e.range.end));
        for pair in self.edits.windows(2) {
            if pair[0].range.end > pair[1].range.start {
                return Err(XmpWriteError::Verify("overlapping edits".to_string()));
            }
        }
        let mut out = self.content.to_string();
        for edit in self.edits.iter().rev() {
            out.replace_range(edit.range.clone(), &edit.text);
        }
        Ok(out)
    }
}

fn render_subject(qname: &str, rdf: &str, items: &[String], indent: &str, nl: &str) -> String {
    let mut out = format!("<{}>{}{} <{}:Bag>{}", qname, nl, indent, rdf, nl);
    for item in items {
        out.push_str(&format!(
            "{}  <{}:li>{}</{}:li>{}",
            indent,
            rdf,
            escape(item.as_str()),
            rdf,
            nl
        ));
    }
    out.push_str(&format!(
        "{} </{}:Bag>{}{}</{}>",
        indent, rdf, nl, indent, qname
    ));
    out
}

fn same_set(a: &[String], b: &[String]) -> bool {
    let a: HashSet<&String> = a.iter().collect();
    let b: HashSet<&String> = b.iter().collect();
    a == b
}

pub(crate) fn update_xmp(content: &str, values: &XmpValues) -> WriteResult<Option<String>> {
    let before = parse_xmp(content).map_err(XmpWriteError::Parse)?;
    let scan = scan_xmp(content)?;
    let nl = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut editor = Editor {
        content,
        edits: Vec::new(),
    };

    let target = scan.descs.first();
    let (xmp_prefix, xmp_decl, xmp_clash) = match target {
        Some(t) => match &t.xmp {
            Some(p) => (p.clone(), false, false),
            None => ("xmp".to_string(), true, t.xmp_clash),
        },
        None => ("xmp".to_string(), true, false),
    };
    let quote = target.map_or('"', |t| t.quote as char);
    let attr = |name: &str, value: &str| format!("{}={}{}{}", name, quote, value, quote);

    let mut needs_xmp = false;
    let mut needs_dc = false;
    let mut rating_insert = None;
    let mut label_insert = None;
    let mut subject_insert = false;

    let rating_text = values.rating.to_string();
    if scan.ratings.is_empty() && values.rating != 0 {
        needs_xmp = true;
        rating_insert = Some(attr(&format!("{}:Rating", xmp_prefix), &rating_text));
    }
    for occ in &scan.ratings {
        if occ.decoded.parse::<i8>() != Ok(values.rating) {
            editor.set_value(&occ.loc, &rating_text);
        }
    }

    let mut kept_label: Option<String> = None;
    match &values.label {
        Some(label) => {
            let escaped = escape(label.as_str()).to_string();
            if scan.labels.is_empty() {
                needs_xmp = true;
                label_insert = Some(attr(&format!("{}:Label", xmp_prefix), &escaped));
            }
            for occ in &scan.labels {
                if occ.decoded.to_lowercase() != label.to_lowercase() {
                    editor.set_value(&occ.loc, &escaped);
                }
            }
        }
        None => {
            for occ in &scan.labels {
                if normalize_label(&occ.decoded).is_some() {
                    match &occ.loc {
                        Loc::Attr(a) => editor.remove(a.whole.clone()),
                        Loc::Elem(el) => editor.remove(el.whole.clone()),
                    }
                } else if kept_label.is_none() && !occ.decoded.is_empty() {
                    kept_label = Some(occ.decoded.clone());
                }
            }
        }
    }

    if needs_xmp && xmp_clash {
        return Err(XmpWriteError::PrefixClash(
            "prefix xmp is bound to another namespace".to_string(),
        ));
    }

    let mut subject_items: Option<Vec<String>> = None;
    if values.keywords.is_empty() {
        for occ in &scan.subjects {
            editor.remove(occ.elem.whole.clone());
        }
    } else if scan.subjects.len() == 1 && same_set(&scan.subjects[0].values, &values.keywords) {
    } else if let Some(first) = scan.subjects.first() {
        let desc = &scan.descs[first.desc];
        let rdf = desc
            .rdf
            .clone()
            .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
        let indent = line_indent(content, first.elem.whole.start)
            .or_else(|| desc.child_indent.clone())
            .unwrap_or_else(|| format!("{} ", desc.indent.clone().unwrap_or_default()));
        editor.replace(
            first.elem.whole.clone(),
            render_subject(&first.elem.qname, &rdf, &values.keywords, &indent, nl),
        );
        for occ in &scan.subjects[1..] {
            editor.remove(occ.elem.whole.clone());
        }
    } else {
        subject_insert = true;
        needs_dc = true;
        subject_items = Some(values.keywords.clone());
    }

    match target {
        Some(t) => {
            let dc_prefix = match &t.dc {
                Some(p) => p.clone(),
                None if needs_dc && t.dc_clash => {
                    return Err(XmpWriteError::PrefixClash(
                        "prefix dc is bound to another namespace".to_string(),
                    ));
                }
                None => "dc".to_string(),
            };
            let dc_decl = t.dc.is_none() && needs_dc;
            let mut attrs: Vec<String> = Vec::new();
            if needs_xmp && xmp_decl {
                attrs.push(attr("xmlns:xmp", NS_XMP_STR));
            }
            if dc_decl {
                attrs.push(attr("xmlns:dc", NS_DC_STR));
            }
            attrs.extend(rating_insert);
            attrs.extend(label_insert);
            let attr_indent = t
                .attr_indent
                .clone()
                .unwrap_or_else(|| format!("{}  ", t.indent.clone().unwrap_or_default()));
            let mut attr_text = String::new();
            for a in &attrs {
                if t.multiline {
                    attr_text.push_str(nl);
                    attr_text.push_str(&attr_indent);
                } else {
                    attr_text.push(' ');
                }
                attr_text.push_str(a);
            }

            let desc_indent = t.indent.clone().unwrap_or_default();
            let child_indent = t
                .child_indent
                .clone()
                .unwrap_or_else(|| format!("{} ", desc_indent));
            let block = if subject_insert {
                let rdf = t
                    .rdf
                    .clone()
                    .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                let items = subject_items.take().unwrap_or_default();
                Some(format!(
                    "{}{}",
                    child_indent,
                    render_subject(
                        &format!("{}:subject", dc_prefix),
                        &rdf,
                        &items,
                        &child_indent,
                        nl
                    )
                ))
            } else {
                None
            };

            if t.self_closing {
                if let Some(block) = block {
                    editor.replace(
                        t.attrs_end..t.tag_end,
                        format!(">{nl}{block}{nl}{desc_indent}</{}>", t.qname,),
                    );
                    if !attr_text.is_empty() {
                        editor.replace(t.attrs_end..t.attrs_end, attr_text);
                    }
                } else if !attr_text.is_empty() {
                    editor.replace(t.attrs_end..t.attrs_end, attr_text);
                }
            } else {
                if !attr_text.is_empty() {
                    editor.replace(t.attrs_end..t.attrs_end, attr_text);
                }
                if let (Some(block), Some(end)) = (block, &t.end_tag) {
                    match line_indent(content, end.start) {
                        Some(indent) => editor.replace(
                            end.start - indent.len()..end.start - indent.len(),
                            format!("{block}{nl}"),
                        ),
                        None => editor.replace(
                            end.start..end.start,
                            format!("{nl}{block}{nl}{desc_indent}"),
                        ),
                    }
                }
            }
        }
        None => {
            if rating_insert.is_some() || label_insert.is_some() || subject_insert {
                let rdf = scan.rdf.as_ref().ok_or_else(|| parse_err("no rdf:RDF"))?;
                let prefix = rdf
                    .prefix
                    .clone()
                    .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                let end = rdf
                    .end_tag
                    .clone()
                    .ok_or_else(|| parse_err("rdf:RDF without end tag"))?;
                let rdf_indent = line_indent(content, end.start);
                let base = rdf_indent.clone().unwrap_or_default();
                let desc_indent = format!("{} ", base);
                let child_indent = format!("{} ", desc_indent);
                let mut open = format!(
                    "<{0}:Description {0}:about=\"\" xmlns:xmp=\"{1}\" xmlns:dc=\"{2}\"",
                    prefix, NS_XMP_STR, NS_DC_STR
                );
                if let Some(a) = &rating_insert {
                    open.push(' ');
                    open.push_str(a);
                }
                if let Some(a) = &label_insert {
                    open.push(' ');
                    open.push_str(a);
                }
                let body = if subject_insert {
                    let items = subject_items.take().unwrap_or_default();
                    format!(
                        "{open}>{nl}{child_indent}{}{nl}{desc_indent}</{prefix}:Description>",
                        render_subject("dc:subject", &prefix, &items, &child_indent, nl)
                    )
                } else {
                    format!("{open}/>")
                };
                match rdf_indent {
                    Some(indent) => {
                        let at = end.start - indent.len();
                        editor.replace(at..at, format!("{desc_indent}{body}{nl}"));
                    }
                    None => {
                        editor.replace(end.start..end.start, format!("{nl}{desc_indent}{body}{nl}"))
                    }
                }
            }
        }
    }

    let out = editor.apply()?;
    if out == content {
        return Ok(None);
    }

    let after = parse_xmp(&out).map_err(XmpWriteError::Verify)?;
    let rating_ok = after.rating == Some(values.rating)
        || (values.rating == 0 && after.rating.is_none() && before.rating.is_none());
    if !rating_ok {
        return Err(XmpWriteError::Verify("rating differs".to_string()));
    }
    let label_ok = match (&after.label, &values.label, &kept_label) {
        (Some(a), Some(v), _) => a.to_lowercase() == v.to_lowercase(),
        (None, Some(_), _) => false,
        (Some(a), None, Some(k)) => a.to_lowercase() == k.to_lowercase(),
        (None, None, None) => true,
        _ => false,
    };
    if !label_ok {
        return Err(XmpWriteError::Verify("label differs".to_string()));
    }
    if !same_set(&after.subject, &values.keywords) {
        return Err(XmpWriteError::Verify("keywords differ".to_string()));
    }
    Ok(Some(out))
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct XmpSyncStateRaw {
    rating: i8,
    label: Option<String>,
    keywords: Vec<String>,
}

impl From<XmpSyncStateRaw> for XmpSyncState {
    fn from(raw: XmpSyncStateRaw) -> Self {
        XmpSyncState::new(raw.rating, raw.label, raw.keywords)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(from = "XmpSyncStateRaw")]
pub(crate) struct XmpSyncState {
    pub rating: i8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmpSyncBase {
    pub file: String,
    pub state: XmpSyncState,
}

#[derive(Serialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct XmpConflictInfo {
    pub rr: XmpSyncState,
    pub xmp: XmpSyncState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncAction {
    InSync,
    Import,
    Write,
    Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConflictChoice {
    Import,
    Write,
}

fn normalize_keyword(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix(USER_TAG_PREFIX).unwrap_or(value).trim()
}

pub(crate) fn normalize_keywords<S: AsRef<str>>(keywords: &[S]) -> Vec<String> {
    let set: BTreeSet<String> = keywords
        .iter()
        .map(|k| normalize_keyword(k.as_ref()))
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    set.into_iter().collect()
}

impl XmpSyncState {
    pub(crate) fn new(rating: i8, label: Option<String>, keywords: Vec<String>) -> Self {
        XmpSyncState {
            rating: if rating < 0 {
                XMP_REJECTED_RATING
            } else {
                rating.min(5)
            },
            label: label.as_deref().and_then(normalize_label),
            keywords: normalize_keywords(&keywords),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rating == 0 && self.label.is_none() && self.keywords.is_empty()
    }

    pub(crate) fn to_values(&self) -> XmpValues {
        XmpValues {
            rating: self.rating,
            label: self.label.as_deref().map(|label| {
                let mut chars = label.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }),
            keywords: self.keywords.clone(),
        }
    }
}

pub(crate) fn rr_state(metadata: &ImageMetadata) -> XmpSyncState {
    let values = XmpValues::from_metadata(metadata);
    XmpSyncState::new(values.rating, values.label, values.keywords)
}

pub(crate) fn xmp_state(fields: &XmpFields, base: &XmpSyncState) -> XmpSyncState {
    let rating = if fields.rating_invalid {
        base.rating
    } else {
        fields.rating.unwrap_or(0)
    };
    XmpSyncState::new(rating, fields.label.clone(), fields.subject.clone())
}

pub(crate) fn classify(
    base: Option<&XmpSyncState>,
    rr: &XmpSyncState,
    xmp: &XmpSyncState,
) -> SyncAction {
    if rr == xmp {
        return SyncAction::InSync;
    }
    match base {
        Some(base) if rr == base => SyncAction::Import,
        Some(base) if xmp == base => SyncAction::Write,
        Some(_) => SyncAction::Conflict,
        None if rr.is_empty() => SyncAction::Import,
        None if xmp.is_empty() => SyncAction::Write,
        None => SyncAction::Conflict,
    }
}

fn set_rating(metadata: &mut ImageMetadata, rating: u8) {
    metadata.rating = rating;
    if !metadata.adjustments.is_object() {
        metadata.adjustments = default_adjustments();
    }
    if let Some(obj) = metadata.adjustments.as_object_mut() {
        obj.insert("rating".to_string(), serde_json::json!(rating));
    }
}

fn has_tag(tags: &[String], keyword: &str) -> bool {
    tags.iter()
        .any(|t| !t.starts_with(COLOR_TAG_PREFIX) && normalize_keyword(t) == keyword)
}

fn is_synced_keyword(name: &str) -> bool {
    !name.is_empty() && valid_xml_text(name)
}

pub(crate) fn apply_state(
    metadata: &mut ImageMetadata,
    state: &XmpSyncState,
    legacy_names: Option<&BTreeSet<String>>,
) -> bool {
    let before_rating = metadata.rating;
    let before_flag = metadata.flag;
    let before_tags = metadata.tags.clone();

    if state.rating < 0 {
        metadata.flag = Some(ImageFlag::Reject);
    } else {
        let rating = state.rating as u8;
        if metadata.rating != rating {
            set_rating(metadata, rating);
        }
        if metadata.flag == Some(ImageFlag::Reject) {
            metadata.flag = None;
        }
    }

    let original = metadata.tags.clone().unwrap_or_default();
    let mut tags = original.clone();

    let label_tag = state
        .label
        .as_ref()
        .map(|label| format!("{}{}", COLOR_TAG_PREFIX, label));
    let color_tags: Vec<&String> = tags
        .iter()
        .filter(|t| t.starts_with(COLOR_TAG_PREFIX))
        .collect();
    let label_ok = match &label_tag {
        Some(label_tag) => color_tags.len() == 1 && color_tags[0] == label_tag,
        None => color_tags.is_empty(),
    };
    if !label_ok {
        tags.retain(|t| !t.starts_with(COLOR_TAG_PREFIX));
        if let Some(label_tag) = label_tag {
            tags.push(label_tag);
        }
    }

    let wanted: BTreeSet<&str> = state.keywords.iter().map(String::as_str).collect();
    tags.retain(|t| {
        if t.starts_with(COLOR_TAG_PREFIX) {
            return true;
        }
        let name = normalize_keyword(t);
        !is_synced_keyword(name) || wanted.contains(name)
    });
    if let Some(legacy) = legacy_names {
        for tag in tags.iter_mut() {
            if tag.starts_with(COLOR_TAG_PREFIX) || tag.starts_with(USER_TAG_PREFIX) {
                continue;
            }
            let name = normalize_keyword(tag);
            if legacy.contains(name) {
                *tag = format!("{}{}", USER_TAG_PREFIX, name);
            }
        }
    }
    let mut seen = HashSet::new();
    tags.retain(|t| seen.insert(t.clone()));
    for keyword in &state.keywords {
        if !has_tag(&tags, keyword) {
            tags.push(format!("{}{}", USER_TAG_PREFIX, keyword));
        }
    }

    if tags != original {
        metadata.tags = if tags.is_empty() { None } else { Some(tags) };
    }

    metadata.rating != before_rating || metadata.flag != before_flag || metadata.tags != before_tags
}

fn decode_legacy_tags(metadata: &mut ImageMetadata, subject: &[String]) -> bool {
    let Some(tags) = metadata.tags.as_mut() else {
        return false;
    };
    let mut replaced = false;
    for tag in tags.iter_mut() {
        if tag.starts_with(COLOR_TAG_PREFIX) || !tag.contains('&') {
            continue;
        }
        let prefix = if tag.trim().starts_with(USER_TAG_PREFIX) {
            USER_TAG_PREFIX
        } else {
            ""
        };
        let name = normalize_keyword(tag);
        let decoded = match unescape(name) {
            Ok(decoded) if decoded != name && subject.iter().any(|s| s.trim() == decoded) => {
                format!("{}{}", prefix, decoded)
            }
            _ => continue,
        };
        *tag = decoded;
        replaced = true;
    }
    if replaced {
        let mut seen = HashSet::new();
        tags.retain(|t| seen.insert(t.clone()));
    }
    replaced
}

fn sidecar_file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn matching_base(metadata: &ImageMetadata, file: &str) -> Option<XmpSyncState> {
    metadata
        .xmp_sync
        .as_ref()
        .filter(|base| base.file.to_lowercase() == file.to_lowercase())
        .map(|base| base.state.clone())
}

fn set_base(metadata: &mut ImageMetadata, file: &str, state: &XmpSyncState) {
    metadata.xmp_sync = (!state.is_empty()).then(|| XmpSyncBase {
        file: file.to_string(),
        state: state.clone(),
    });
}

fn legacy_names(state: &XmpSyncState) -> BTreeSet<String> {
    state.keywords.iter().cloned().collect()
}

fn snapshot(
    metadata: &ImageMetadata,
) -> (
    u8,
    Option<ImageFlag>,
    Option<Vec<String>>,
    Option<XmpSyncBase>,
    bool,
) {
    (
        metadata.rating,
        metadata.flag,
        metadata.tags.clone(),
        metadata.xmp_sync.clone(),
        metadata.xmp_conflict,
    )
}

fn import_state(metadata: &mut ImageMetadata, state: &XmpSyncState, first_sync: bool) -> bool {
    let legacy = first_sync.then(|| legacy_names(state));
    apply_state(metadata, state, legacy.as_ref())
}

struct ReadSidecar {
    path: PathBuf,
    file: String,
    content: String,
    base: Option<XmpSyncState>,
    xmp: XmpSyncState,
}

impl ReadSidecar {
    fn new(path: PathBuf, content: String, fields: &XmpFields, metadata: &ImageMetadata) -> Self {
        let file = sidecar_file_name(&path);
        let base = matching_base(metadata, &file);
        let xmp = xmp_state(fields, &base.clone().unwrap_or_default());
        ReadSidecar {
            path,
            file,
            content,
            base,
            xmp,
        }
    }
}

fn read_sidecar(path: PathBuf, metadata: &mut ImageMetadata) -> Result<ReadSidecar, String> {
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    let fields =
        parse_xmp(&content).map_err(|e| format!("failed to parse {}: {}", path.display(), e))?;
    decode_legacy_tags(metadata, &fields.subject);
    Ok(ReadSidecar::new(path, content, &fields, metadata))
}

fn sync_action(
    metadata: &ImageMetadata,
    base: Option<&XmpSyncState>,
    rr: &XmpSyncState,
    xmp: &XmpSyncState,
) -> SyncAction {
    match classify(base, rr, xmp) {
        SyncAction::Import | SyncAction::Write if metadata.xmp_conflict => SyncAction::Conflict,
        action => action,
    }
}

pub(crate) fn sync_on_load(source_path: &Path, metadata: &mut ImageMetadata) -> bool {
    let before = snapshot(metadata);
    let Some(path) = resolve_xmp_path(source_path) else {
        metadata.xmp_conflict = false;
        return snapshot(metadata) != before;
    };
    let side = match read_xmp_file(&path) {
        Ok(fields) => {
            decode_legacy_tags(metadata, &fields.subject);
            ReadSidecar::new(path, String::new(), &fields, metadata)
        }
        Err(e) => {
            log::warn!("Skipping XMP sync: {}", e);
            return false;
        }
    };

    let rr = rr_state(metadata);
    let first_sync = side.base.is_none();
    match sync_action(metadata, side.base.as_ref(), &rr, &side.xmp) {
        SyncAction::InSync => {
            if first_sync {
                import_state(metadata, &side.xmp, true);
            }
            set_base(metadata, &side.file, &side.xmp);
            metadata.xmp_conflict = false;
        }
        SyncAction::Import => {
            import_state(metadata, &side.xmp, first_sync);
            set_base(metadata, &side.file, &side.xmp);
            metadata.xmp_conflict = false;
            log::info!("Imported XMP values from {}", side.path.display());
        }
        SyncAction::Write => {
            metadata.xmp_conflict = false;
        }
        SyncAction::Conflict => {
            if !metadata.xmp_conflict {
                log::info!("XMP conflict for {}", source_path.display());
            }
            metadata.xmp_conflict = true;
        }
    }
    snapshot(metadata) != before
}

pub(crate) fn sync_on_save(
    source_path: &Path,
    metadata: &mut ImageMetadata,
    create_if_missing: bool,
) -> bool {
    let Some(path) = resolve_xmp_path(source_path) else {
        if !create_if_missing {
            return false;
        }
        let rr = rr_state(metadata);
        let target = source_path.with_extension("xmp");
        let out = match update_xmp(NEW_XMP, &rr.to_values()) {
            Ok(out) => out.unwrap_or_else(|| NEW_XMP.to_string()),
            Err(e) => {
                log::warn!("Not writing XMP for {}: {}", source_path.display(), e);
                return false;
            }
        };
        if let Err(e) = write_atomic(&target, out.as_bytes()) {
            log::warn!("Not writing XMP for {}: {}", source_path.display(), e);
            return false;
        }
        set_base(metadata, &sidecar_file_name(&target), &rr);
        metadata.xmp_conflict = false;
        return false;
    };
    let tags_before = metadata.tags.clone();
    let side = match read_sidecar(path, metadata) {
        Ok(side) => side,
        Err(e) => {
            log::warn!("Not writing XMP for {}: {}", source_path.display(), e);
            return false;
        }
    };
    let decoded = metadata.tags != tags_before;

    let rr = rr_state(metadata);
    let first_sync = side.base.is_none();
    let imported = match sync_action(metadata, side.base.as_ref(), &rr, &side.xmp) {
        SyncAction::Conflict => {
            if !metadata.xmp_conflict {
                log::info!("XMP conflict for {}", source_path.display());
            }
            metadata.xmp_conflict = true;
            false
        }
        SyncAction::Import => {
            let changed = import_state(metadata, &side.xmp, first_sync);
            set_base(metadata, &side.file, &side.xmp);
            metadata.xmp_conflict = false;
            log::info!("Imported XMP values from {}", side.path.display());
            changed
        }
        SyncAction::InSync => {
            let changed = first_sync && import_state(metadata, &side.xmp, true);
            set_base(metadata, &side.file, &side.xmp);
            metadata.xmp_conflict = false;
            changed
        }
        SyncAction::Write => {
            let written = match update_xmp(&side.content, &rr.to_values()) {
                Ok(None) => Ok(()),
                Ok(Some(out)) => {
                    write_atomic(&side.path, out.as_bytes()).map_err(|e| e.to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            match written {
                Ok(()) => {
                    set_base(metadata, &side.file, &rr);
                    metadata.xmp_conflict = false;
                }
                Err(e) => log::warn!("Not writing XMP for {}: {}", source_path.display(), e),
            }
            false
        }
    };
    imported || decoded
}

pub(crate) fn resolve_xmp_conflict(
    source_path: &Path,
    metadata: &mut ImageMetadata,
    choice: ConflictChoice,
) -> Result<(), String> {
    let path = resolve_xmp_path(source_path)
        .ok_or_else(|| format!("no XMP sidecar for {}", source_path.display()))?;
    let side = read_sidecar(path, metadata)?;
    match choice {
        ConflictChoice::Import => {
            import_state(metadata, &side.xmp, side.base.is_none());
            set_base(metadata, &side.file, &side.xmp);
        }
        ConflictChoice::Write => {
            let rr = rr_state(metadata);
            if let Some(out) = update_xmp(&side.content, &rr.to_values())
                .map_err(|e| format!("cannot write {}: {}", side.path.display(), e))?
            {
                write_atomic(&side.path, out.as_bytes())
                    .map_err(|e| format!("cannot write {}: {}", side.path.display(), e))?;
            }
            set_base(metadata, &side.file, &rr);
        }
    }
    metadata.xmp_conflict = false;
    Ok(())
}

pub(crate) fn xmp_conflict_info(
    source_path: &Path,
    metadata: &ImageMetadata,
) -> Result<Option<XmpConflictInfo>, String> {
    if !metadata.xmp_conflict {
        return Ok(None);
    }
    let Some(path) = resolve_xmp_path(source_path) else {
        return Ok(None);
    };
    let mut metadata = metadata.clone();
    let side = read_sidecar(path, &mut metadata)?;
    Ok(Some(XmpConflictInfo {
        rr: rr_state(&metadata),
        xmp: side.xmp,
    }))
}
