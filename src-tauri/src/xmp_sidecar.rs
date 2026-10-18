use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use quick_xml::XmlVersion;
use quick_xml::escape::{escape, unescape};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{NamespaceResolver, PrefixDeclaration, ResolveResult};
use quick_xml::reader::NsReader;
use serde::{Deserialize, Serialize};

use crate::app_settings::AppSettings;
use crate::file_management::{
    create_atomic, is_cloud_placeholder, replace_if_unchanged, write_atomic,
};
use crate::image_processing::{ImageFlag, ImageMetadata, default_adjustments};
use crate::tagging::{COLOR_TAG_PREFIX, USER_TAG_PREFIX};

const NS_RDF: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/";
const NS_DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const NS_X: &[u8] = b"adobe:ns:meta/";
const NS_DARKTABLE: &[u8] = b"http://darktable.sf.net/";
const NS_DIGIKAM: &[u8] = b"http://www.digikam.org/ns/1.0/";
const NS_LR: &[u8] = b"http://ns.adobe.com/lightroom/1.0/";

pub const HIERARCHY_SEPARATOR: char = '|';

pub(crate) fn is_darktable_internal(tag: &str) -> bool {
    tag.starts_with("darktable|")
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct XmpFields {
    pub rating: Option<i8>,
    pub label: Option<String>,
    pub subject: Vec<String>,
    pub hierarchical_subject: Vec<String>,
    pub rating_invalid: bool,
    pub darktable_colorlabels: Option<Vec<String>>,
    pub digikam_color_label: Option<String>,
    pub darktable_ns: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    Meta,
    Rdf,
    Desc,
    Rating,
    Label,
    Subject,
    Hierarchical,
    DtLabels,
    DkLabel,
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
                } else if is_name(ns, NS_LR, local, &[b"hierarchicalSubject"]) {
                    Ctx::Hierarchical
                } else if is_name(ns, NS_DARKTABLE, local, &[b"colorlabels"]) {
                    Ctx::DtLabels
                } else if is_name(ns, NS_DIGIKAM, local, &[b"ColorLabel"]) {
                    Ctx::DkLabel
                } else {
                    Ctx::Ignored
                }
            }
            Some(Ctx::Subject | Ctx::Hierarchical | Ctx::DtLabels) => {
                if is_name(ns, NS_RDF, local, &[b"Bag", b"Seq", b"Alt"]) {
                    Ctx::Container
                } else {
                    return Err("unsupported list property content".to_string());
                }
            }
            Some(Ctx::Container) => {
                if is_name(ns, NS_RDF, local, &[b"li"]) {
                    Ctx::Li
                } else {
                    return Err("unsupported list content".to_string());
                }
            }
            Some(Ctx::Li) | Some(Ctx::Rating) | Some(Ctx::Label) | Some(Ctx::DkLabel) => {
                return Err("unexpected child element in a text property".to_string());
            }
            Some(Ctx::Ignored) => Ctx::Ignored,
        };

        match ctx {
            Ctx::Desc => {
                self.desc_props.clear();
                if self.stack.last() == Some(&Ctx::Rdf)
                    && resolver
                        .bindings()
                        .any(|(_, namespace)| namespace.into_inner() == NS_DARKTABLE)
                {
                    self.fields.darktable_ns = true;
                }
            }
            Ctx::Rating | Ctx::Label | Ctx::Subject | Ctx::Hierarchical | Ctx::DkLabel => {
                self.mark(ctx)?
            }
            Ctx::DtLabels => {
                self.mark(ctx)?;
                self.fields
                    .darktable_colorlabels
                    .get_or_insert_with(Vec::new);
            }
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
                    } else if is_name(&attr_ns, NS_LR, local_attr, &[b"hierarchicalSubject"]) {
                        return Err("lr:hierarchicalSubject as an attribute".to_string());
                    } else if is_name(&attr_ns, NS_DIGIKAM, local_attr, &[b"ColorLabel"]) {
                        self.mark(Ctx::DkLabel)?;
                        self.fields.digikam_color_label = Some(value.trim().to_string());
                    } else if is_name(&attr_ns, NS_DARKTABLE, local_attr, &[b"colorlabels"]) {
                        return Err("darktable:colorlabels as an attribute".to_string());
                    }
                }
                Ctx::Rating
                | Ctx::Label
                | Ctx::Subject
                | Ctx::Hierarchical
                | Ctx::DtLabels
                | Ctx::DkLabel
                | Ctx::Container
                | Ctx::Li
                    if attr_ns.as_deref() == Some(NS_RDF) =>
                {
                    return Err("unsupported rdf attribute on a property".to_string());
                }
                _ => {}
            }
        }

        self.stack.push(ctx);
        if matches!(ctx, Ctx::Rating | Ctx::Label | Ctx::DkLabel | Ctx::Li) {
            self.text.clear();
        }
        Ok(())
    }

    fn close(&mut self) -> Result<(), String> {
        let ctx = self
            .stack
            .pop()
            .ok_or_else(|| "unexpected end tag".to_string())?;
        if matches!(ctx, Ctx::Rating | Ctx::Label | Ctx::DkLabel | Ctx::Li) {
            let raw = std::mem::take(&mut self.text);
            let decoded = unescape(&raw).map_err(|err| err.to_string())?;
            let value = decoded.trim();
            if ctx == Ctx::DkLabel {
                self.fields.digikam_color_label = Some(value.to_string());
            } else if !value.is_empty() {
                match ctx {
                    Ctx::Rating => self.set_rating(value),
                    Ctx::Label => self.set_label(value),
                    _ => {
                        let owner = self.stack.iter().rev().nth(1).copied();
                        if owner == Some(Ctx::DtLabels) {
                            if let Some(labels) = self.fields.darktable_colorlabels.as_mut() {
                                labels.push(value.to_string());
                            }
                        } else if owner == Some(Ctx::Hierarchical) {
                            if !self.fields.hierarchical_subject.iter().any(|t| t == value) {
                                self.fields.hierarchical_subject.push(value.to_string());
                            }
                        } else if !is_darktable_internal(value)
                            && !self.fields.subject.iter().any(|t| t == value)
                        {
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
            Some(Ctx::Rating | Ctx::Label | Ctx::DkLabel | Ctx::Li) => {
                self.text.push_str(raw);
                return Ok(());
            }
            Some(Ctx::Subject | Ctx::Hierarchical | Ctx::DtLabels | Ctx::Container) => is_xml_space,
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

pub struct SidecarPaths {
    pub existing: Option<PathBuf>,
    pub create_at: PathBuf,
}

const EXTRA_MEMBER_EXTENSIONS: [&str; 5] = ["heic", "heif", "avif", "psd", "psb"];

#[derive(Default)]
pub struct StemSiblings {
    members: HashMap<String, Vec<String>>,
    xmp_names: HashMap<String, Vec<String>>,
}

fn split_stem(name: &str) -> Option<(&str, &str)> {
    name.rsplit_once('.').filter(|(stem, _)| !stem.is_empty())
}

fn is_stem_member(name: &str) -> bool {
    if crate::formats::is_supported_image_file(name) {
        return true;
    }
    !name.starts_with('.')
        && split_stem(name).is_some_and(|(_, ext)| {
            EXTRA_MEMBER_EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(ext))
        })
}

fn is_sidecar_raw(name: &str) -> bool {
    crate::formats::is_raw_file(name)
        && split_stem(name).is_some_and(|(_, ext)| !ext.eq_ignore_ascii_case("dng"))
}

impl StemSiblings {
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let mut siblings = StemSiblings::default();
        for name in names {
            let lower = name.to_ascii_lowercase();
            if lower.ends_with(".xmp") {
                siblings
                    .xmp_names
                    .entry(lower)
                    .or_default()
                    .push(name.to_string());
            } else if is_stem_member(name)
                && let Some((stem, _)) = split_stem(name)
            {
                siblings
                    .members
                    .entry(stem.to_ascii_lowercase())
                    .or_default()
                    .push(name.to_string());
            }
        }
        siblings
    }

    pub fn from_dir(dir: &Path) -> Option<Self> {
        let entries = fs::read_dir(dir).ok()?;
        let names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        Some(Self::from_names(names.iter().map(String::as_str)))
    }

    fn is_member(&self, name: &str) -> bool {
        split_stem(name)
            .and_then(|(stem, _)| self.members.get(&stem.to_ascii_lowercase()))
            .is_some_and(|group| group.iter().any(|m| m.eq_ignore_ascii_case(name)))
    }

    fn find_xmp(&self, wanted: &str) -> Option<&String> {
        let names = self.xmp_names.get(&wanted.to_ascii_lowercase())?;
        let upper = format!("{}.XMP", &wanted[..wanted.len() - ".xmp".len()]);
        names
            .iter()
            .find(|n| n.as_str() == wanted)
            .or_else(|| names.iter().find(|n| **n == upper))
            .or_else(|| names.iter().min())
    }
}

pub struct SiblingCache {
    dirs: Mutex<HashMap<PathBuf, Option<Arc<StemSiblings>>>>,
}

impl SiblingCache {
    pub fn new() -> Self {
        SiblingCache {
            dirs: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, dir: &Path) -> Option<Arc<StemSiblings>> {
        let mut dirs = self.dirs.lock().ok()?;
        dirs.entry(dir.to_path_buf())
            .or_insert_with(|| StemSiblings::from_dir(dir).map(Arc::new))
            .clone()
    }
}

fn log_stem_wins(image_path: &Path) {
    static SEEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    if let Ok(mut seen) = seen.lock()
        && seen.insert(image_path.to_path_buf())
    {
        log::info!(
            "Both naming schemes exist for {}, using the file without the image extension",
            image_path.display()
        );
    }
}

pub fn locate_sidecar(image_path: &Path, siblings: &StemSiblings) -> Option<SidecarPaths> {
    let name = image_path.file_name()?.to_str()?;
    let (stem, _) = split_stem(name)?;
    let group = siblings.members.get(&stem.to_ascii_lowercase())?;
    if !group.iter().any(|member| member == name) {
        return None;
    }

    let raws: Vec<&String> = group.iter().filter(|m| is_sidecar_raw(m)).collect();
    let is_owner = !siblings.is_member(stem)
        && match raws.as_slice() {
            [only] => only.as_str() == name,
            [] => group.len() == 1,
            _ => false,
        };

    let stem_name = format!("{}.xmp", stem);
    let full_name = format!("{}.xmp", name);
    let create_at = if is_owner && is_sidecar_raw(name) {
        image_path.with_file_name(&stem_name)
    } else {
        image_path.with_file_name(&full_name)
    };
    let stem_found = is_owner
        .then(|| siblings.find_xmp(&stem_name))
        .flatten()
        .map(|found| image_path.with_file_name(found));
    let full_found = siblings
        .find_xmp(&full_name)
        .map(|found| image_path.with_file_name(found));

    let existing = match (stem_found, full_found) {
        (Some(stem), Some(full)) => {
            if is_cloud_placeholder(&stem) {
                Some(stem)
            } else {
                match fs::read_to_string(&stem) {
                    Ok(content) if is_rapidraw_skeleton(&content) => Some(full),
                    _ => {
                        log_stem_wins(image_path);
                        Some(stem)
                    }
                }
            }
        }
        (stem, full) => stem.or(full),
    };
    Some(SidecarPaths {
        existing,
        create_at,
    })
}

fn skeleton_element(
    depth: usize,
    descs: &mut usize,
    resolver: &NamespaceResolver,
    ns: &Ns,
    e: &BytesStart,
) -> Option<()> {
    let local_name = e.local_name();
    let local = local_name.as_ref();
    match depth {
        0 => {
            if !is_name(ns, NS_X, local, &[b"xmpmeta"]) {
                return None;
            }
            let mut toolkit = false;
            for attr in e.attributes() {
                let attr = attr.ok()?;
                let (res, local_attr) = resolver.resolve_attribute(attr.key);
                let attr_ns = bound(res).ok()?;
                if is_name(&attr_ns, NS_X, local_attr.as_ref(), &[b"xmptk"]) {
                    toolkit = attr.value.as_ref() == b"RapidRAW";
                }
            }
            toolkit.then_some(())
        }
        1 => is_name(ns, NS_RDF, local, &[b"RDF"]).then_some(()),
        2 => {
            if !is_name(ns, NS_RDF, local, &[b"Description"]) {
                return None;
            }
            *descs += 1;
            if *descs > 1 {
                return None;
            }
            for attr in e.attributes() {
                let attr = attr.ok()?;
                let key = attr.key.as_ref();
                if key == b"xmlns" || key.starts_with(b"xmlns:") {
                    continue;
                }
                let (res, local_attr) = resolver.resolve_attribute(attr.key);
                let attr_ns = bound(res).ok()?;
                let local_attr = local_attr.as_ref();
                let allowed = is_name(&attr_ns, NS_RDF, local_attr, &[b"about"])
                    || is_name(&attr_ns, NS_XMP, local_attr, &[b"Rating", b"Label"]);
                if !allowed {
                    return None;
                }
            }
            Some(())
        }
        3 => {
            let allowed = is_name(ns, NS_XMP, local, &[b"Rating", b"Label"])
                || is_name(ns, NS_DC, local, &[b"subject"])
                || is_name(ns, NS_LR, local, &[b"hierarchicalSubject"]);
            allowed.then_some(())
        }
        _ => Some(()),
    }
}

fn check_skeleton(content: &str) -> Option<bool> {
    let mut reader = NsReader::from_str(content);
    let mut depth = 0usize;
    let mut descs = 0usize;
    loop {
        let (res, event) = reader.read_resolved_event().ok()?;
        let ns = bound(res).ok()?;
        match event {
            Event::Start(e) => {
                skeleton_element(depth, &mut descs, reader.resolver(), &ns, &e)?;
                depth += 1;
            }
            Event::Empty(e) => skeleton_element(depth, &mut descs, reader.resolver(), &ns, &e)?,
            Event::End(_) => depth = depth.checked_sub(1)?,
            Event::Eof => break,
            _ => {}
        }
    }
    Some(descs == 1)
}

pub(crate) fn is_rapidraw_skeleton(content: &str) -> bool {
    check_skeleton(content).unwrap_or(false)
}

const RAPIDRAW_COLORS: [&str; 5] = ["red", "yellow", "green", "blue", "purple"];

const NS_XMP_STR: &str = "http://ns.adobe.com/xap/1.0/";
const NS_DC_STR: &str = "http://purl.org/dc/elements/1.1/";
const NS_LR_STR: &str = "http://ns.adobe.com/lightroom/1.0/";

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

const DIGIKAM_COLORS: [(&str, &str); 5] = [
    ("red", "1"),
    ("yellow", "3"),
    ("green", "4"),
    ("blue", "5"),
    ("purple", "6"),
];

fn darktable_color(value: &str) -> Option<&'static str> {
    let index = value.trim().parse::<usize>().ok()?;
    RAPIDRAW_COLORS.get(index).copied()
}

fn digikam_color(value: &str) -> Option<&'static str> {
    let value = value.trim();
    DIGIKAM_COLORS
        .iter()
        .find(|(_, code)| *code == value)
        .map(|(color, _)| *color)
}

fn digikam_code(color: &str) -> &'static str {
    DIGIKAM_COLORS
        .iter()
        .find(|(name, _)| *name == color)
        .map_or("0", |(_, code)| code)
}

fn darktable_index(color: &str) -> Option<usize> {
    RAPIDRAW_COLORS.iter().position(|name| *name == color)
}

pub(crate) fn xmp_color(fields: &XmpFields) -> Option<&'static str> {
    if let Some(items) = &fields.darktable_colorlabels {
        return items.iter().find_map(|item| darktable_color(item));
    }
    if let Some(code) = &fields.digikam_color_label {
        return digikam_color(code);
    }
    let label = normalize_label(fields.label.as_deref()?)?;
    RAPIDRAW_COLORS.iter().copied().find(|c| *c == label)
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
        let Some(keyword) = normalize_keyword(tag.strip_prefix(USER_TAG_PREFIX).unwrap_or(tag))
        else {
            continue;
        };
        if !valid_xml_text(&keyword) {
            log::warn!("Dropping XMP keyword with characters invalid in XML");
            continue;
        }
        if seen.insert(keyword.clone()) {
            keywords.push(keyword);
        }
    }
    keywords
}

pub fn keyword_nodes(tag: &str) -> Vec<&str> {
    tag.split(HIERARCHY_SEPARATOR)
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

pub fn normalize_keyword(tag: &str) -> Option<String> {
    let nodes = keyword_nodes(tag);
    (!nodes.is_empty()).then(|| nodes.join(&HIERARCHY_SEPARATOR.to_string()))
}

pub fn normalize_user_tag(tag: &str) -> Option<String> {
    match tag.strip_prefix(USER_TAG_PREFIX) {
        Some(name) => normalize_keyword(name).map(|name| format!("{}{}", USER_TAG_PREFIX, name)),
        None => Some(tag.to_string()),
    }
}

fn is_path(keyword: &str) -> bool {
    keyword.contains(HIERARCHY_SEPARATOR)
}

fn join_nodes(nodes: &[&str]) -> String {
    nodes.join(&HIERARCHY_SEPARATOR.to_string())
}

pub fn merge_keywords(subject: &[String], hierarchical: &[String]) -> Vec<String> {
    let mut flat: Vec<String> = Vec::new();
    for item in subject {
        if is_darktable_internal(item.trim()) {
            continue;
        }
        if let Some(keyword) = normalize_keyword(item)
            && !flat.contains(&keyword)
        {
            flat.push(keyword);
        }
    }
    if flat.is_empty() {
        return Vec::new();
    }
    let known: HashSet<&str> = flat.iter().map(String::as_str).collect();

    let mut kept: Vec<String> = Vec::new();
    for entry in hierarchical {
        if is_darktable_internal(entry.trim()) {
            continue;
        }
        let nodes = keyword_nodes(entry);
        let Some(len) = (1..=nodes.len())
            .rev()
            .find(|len| known.contains(nodes[len - 1]))
        else {
            continue;
        };
        let path = join_nodes(&nodes[..len]);
        if !kept.contains(&path) {
            kept.push(path);
        }
    }

    let covered: HashSet<&str> = kept.iter().flat_map(|path| keyword_nodes(path)).collect();
    let rest: Vec<String> = flat
        .iter()
        .filter(|item| !covered.contains(item.as_str()))
        .cloned()
        .collect();
    kept.extend(rest);
    kept
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OldKeywords {
    pub subject: Vec<String>,
    pub hierarchical: Option<Vec<String>>,
    pub tags_list: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct KeywordProperties {
    pub subject: Vec<String>,
    pub hierarchical: Option<Vec<String>>,
    pub tags_list: Option<Vec<String>>,
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|v| v == value) {
        list.push(value.to_string());
    }
}

pub fn keyword_properties(keywords: &[String], old: &OldKeywords) -> KeywordProperties {
    let mut subject = Vec::new();
    for keyword in keywords {
        for node in keyword_nodes(keyword) {
            push_unique(&mut subject, node);
        }
    }

    let hierarchical =
        (old.hierarchical.is_some() || keywords.iter().any(|k| is_path(k))).then(|| {
            let old_hierarchical = old.hierarchical.as_deref().unwrap_or_default();
            let old_nodes: HashSet<&str> = old_hierarchical
                .iter()
                .filter(|entry| !is_darktable_internal(entry))
                .flat_map(|entry| keyword_nodes(entry))
                .collect();
            let old_subject: HashSet<&str> = old.subject.iter().map(|s| s.trim()).collect();
            let mut out = Vec::new();
            for keyword in keywords {
                let synonym = !is_path(keyword)
                    && old_subject.contains(keyword.as_str())
                    && !old_nodes.contains(keyword.as_str());
                if !synonym {
                    push_unique(&mut out, keyword);
                }
            }
            for entry in old_hierarchical {
                if is_darktable_internal(entry) {
                    push_unique(&mut out, entry);
                }
            }
            out
        });

    let tags_list = old.tags_list.as_ref().map(|old_list| {
        let wanted: HashSet<&str> = keywords.iter().map(String::as_str).collect();
        let read_keywords = old
            .subject
            .iter()
            .any(|item| !item.trim().is_empty() && !is_darktable_internal(item.trim()));
        let mut out: Vec<String> = Vec::new();
        let mut covered: HashSet<String> = HashSet::new();
        for entry in old_list {
            let mapped = entry.replace('/', &HIERARCHY_SEPARATOR.to_string());
            let leaf = entry.rsplit('/').next().unwrap_or(entry).trim();
            if wanted.contains(mapped.as_str()) {
                covered.insert(mapped);
            } else if !is_path(leaf) && wanted.contains(leaf) {
                covered.insert(leaf.to_string());
            } else if read_keywords {
                continue;
            }
            push_unique(&mut out, entry);
        }
        for keyword in keywords {
            if covered.contains(keyword) {
                continue;
            }
            let nodes = keyword_nodes(keyword);
            if nodes.iter().any(|node| node.contains('/')) {
                continue;
            }
            push_unique(&mut out, &nodes.join("/"));
        }
        out
    });

    KeywordProperties {
        subject,
        hierarchical,
        tags_list,
    }
}

fn plan_upgrade(current: &[String], xmp: &[String]) -> (Vec<String>, HashSet<String>) {
    let flat: HashSet<&str> = current
        .iter()
        .map(String::as_str)
        .filter(|item| !is_path(item))
        .collect();
    let mut paths: Vec<String> = Vec::new();
    let mut consumed: HashSet<&str> = HashSet::new();
    for path in xmp.iter().filter(|p| is_path(p)) {
        let nodes = keyword_nodes(path);
        let Some(len) = (2..=nodes.len())
            .rev()
            .find(|len| nodes[..*len].iter().all(|node| flat.contains(node)))
        else {
            continue;
        };
        push_unique(&mut paths, &join_nodes(&nodes[..len]));
        consumed.extend(&nodes[..len]);
    }
    let xmp_flat: HashSet<&str> = xmp
        .iter()
        .map(String::as_str)
        .filter(|item| !is_path(item))
        .collect();
    let removed = consumed
        .into_iter()
        .filter(|node| !xmp_flat.contains(node))
        .map(str::to_string)
        .collect();
    (paths, removed)
}

pub fn upgrade_keywords(current: &[String], xmp: &[String]) -> Option<Vec<String>> {
    let (paths, removed) = plan_upgrade(current, xmp);
    let mut result: Vec<String> = current
        .iter()
        .filter(|item| !removed.contains(item.as_str()))
        .cloned()
        .collect();
    for path in &paths {
        push_unique(&mut result, path);
    }
    (!same_set(&result, current)).then_some(result)
}

pub fn upgrade_tags(metadata: &mut ImageMetadata, xmp: &[String]) -> bool {
    let Some(tags) = metadata.tags.as_ref() else {
        return false;
    };
    let current = xmp_keywords(tags);
    let (paths, removed) = plan_upgrade(&current, xmp);
    if paths.is_empty() {
        return false;
    }
    let mut upgraded: Vec<String> = tags
        .iter()
        .filter(|tag| {
            tag.starts_with(COLOR_TAG_PREFIX)
                || normalize_keyword(bare_name(tag))
                    .is_none_or(|name| !removed.contains(name.as_str()))
        })
        .cloned()
        .collect();
    for path in &paths {
        if !has_tag(&upgraded, path) {
            upgraded.push(format!("{}{}", USER_TAG_PREFIX, path));
        }
    }
    let changed = upgraded != *tags;
    if changed {
        metadata.tags = Some(upgraded);
    }
    changed
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
    dt: Option<String>,
    dk: Option<String>,
    lr: Option<String>,
    xmp_clash: bool,
    dc_clash: bool,
    lr_clash: bool,
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
    hierarchical: Vec<SubjectOcc>,
    tags_lists: Vec<SubjectOcc>,
    dt_labels: Vec<SubjectOcc>,
    dk_labels: Vec<Occ>,
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
    Hierarchical,
    TagsList,
    DtLabels,
    DkLabel,
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
    stray_text: bool,
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
                } else if is_name(ns, NS_LR, local, &[b"hierarchicalSubject"]) {
                    Kind::Hierarchical
                } else if is_name(ns, NS_DIGIKAM, local, &[b"TagsList"]) {
                    Kind::TagsList
                } else if is_name(ns, NS_DARKTABLE, local, &[b"colorlabels"]) {
                    Kind::DtLabels
                } else if is_name(ns, NS_DIGIKAM, local, &[b"ColorLabel"]) {
                    Kind::DkLabel
                } else {
                    Kind::Other
                }
            }
            Some(Kind::TagsList) => {
                if is_name(ns, NS_RDF, local, &[b"Bag", b"Seq", b"Alt"]) {
                    Kind::Container
                } else {
                    return Err(unsupported("digiKam:TagsList content"));
                }
            }
            Some(Kind::Subject | Kind::Hierarchical | Kind::DtLabels) => Kind::Container,
            Some(Kind::Container) => {
                let in_tags_list =
                    self.stack.len() >= 2 && self.stack[self.stack.len() - 2] == Kind::TagsList;
                if in_tags_list && !is_name(ns, NS_RDF, local, &[b"li"]) {
                    return Err(unsupported("digiKam:TagsList content"));
                }
                Kind::Li
            }
            Some(Kind::Rating | Kind::Label | Kind::DkLabel | Kind::Li) => {
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
            Kind::Rating
            | Kind::Label
            | Kind::DkLabel
            | Kind::Subject
            | Kind::Hierarchical
            | Kind::TagsList
            | Kind::DtLabels => {
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
            } else if is_name(&attr_ns, NS_DIGIKAM, local.as_ref(), &[b"ColorLabel"]) {
                self.scan.dk_labels.push(occ);
            } else if is_name(&attr_ns, NS_DIGIKAM, local.as_ref(), &[b"TagsList"])
                || is_name(&attr_ns, NS_LR, local.as_ref(), &[b"hierarchicalSubject"])
            {
                return Err(unsupported("keyword list as an attribute"));
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
            if uri == NS_DARKTABLE && desc.dt.is_none() {
                desc.dt = Some(prefix.clone());
            }
            if uri == NS_DIGIKAM && desc.dk.is_none() {
                desc.dk = Some(prefix.clone());
            }
            if uri == NS_LR && desc.lr.is_none() {
                desc.lr = Some(prefix.clone());
            }
            if prefix == "lr" && uri != NS_LR {
                desc.lr_clash = true;
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
            Kind::Rating
            | Kind::Label
            | Kind::DkLabel
            | Kind::Subject
            | Kind::Hierarchical
            | Kind::TagsList
            | Kind::DtLabels => {
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
                    Kind::Subject | Kind::Hierarchical | Kind::TagsList | Kind::DtLabels => {
                        let occ = SubjectOcc {
                            elem,
                            values: normalize_values(cur.values.iter().map(String::as_str)),
                            desc: cur.desc,
                        };
                        match kind {
                            Kind::Subject => self.scan.subjects.push(occ),
                            Kind::Hierarchical => self.scan.hierarchical.push(occ),
                            Kind::TagsList => self.scan.tags_lists.push(occ),
                            _ => self.scan.dt_labels.push(occ),
                        }
                    }
                    _ => {
                        let raw = unescape(&cur.text).map_err(parse_err)?;
                        let occ = Occ {
                            loc: Loc::Elem(elem),
                            decoded: raw.trim().to_string(),
                        };
                        match kind {
                            Kind::Rating => self.scan.ratings.push(occ),
                            Kind::DkLabel => self.scan.dk_labels.push(occ),
                            _ => self.scan.labels.push(occ),
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn push_text(&mut self, text: &str) {
        let in_tags_list = match self.stack.last() {
            Some(Kind::TagsList) => true,
            Some(Kind::Container) => {
                self.stack.len() >= 2 && self.stack[self.stack.len() - 2] == Kind::TagsList
            }
            _ => false,
        };
        if in_tags_list && !text.chars().all(is_xml_space) {
            self.stray_text = true;
        }
        if matches!(
            self.stack.last(),
            Some(Kind::Rating | Kind::Label | Kind::DkLabel | Kind::Li)
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
        stray_text: false,
    };

    loop {
        let (res, event) = reader.read_resolved_event().map_err(parse_err)?;
        let ns = bound(res).map_err(parse_err)?;
        if matches!(
            scanner.stack.last(),
            Some(Kind::Rating | Kind::Label | Kind::DkLabel)
        ) && matches!(event, Event::Comment(_) | Event::PI(_) | Event::DocType(_))
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

    if scanner.stray_text {
        return Err(unsupported("digiKam:TagsList content"));
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

fn render_list(
    qname: &str,
    rdf: &str,
    container: &str,
    items: &[String],
    indent: &str,
    nl: &str,
) -> String {
    let mut out = format!("<{}>{}{} <{}:{}>{}", qname, nl, indent, rdf, container, nl);
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
        "{} </{}:{}>{}{}</{}>",
        indent, rdf, container, nl, indent, qname
    ));
    out
}

fn old_keywords(scan: &Scan) -> OldKeywords {
    fn union(occs: &[SubjectOcc]) -> Vec<String> {
        let mut out = Vec::new();
        for occ in occs {
            for value in &occ.values {
                push_unique(&mut out, value);
            }
        }
        out
    }
    OldKeywords {
        subject: union(&scan.subjects),
        hierarchical: (!scan.hierarchical.is_empty()).then(|| union(&scan.hierarchical)),
        tags_list: (!scan.tags_lists.is_empty()).then(|| union(&scan.tags_lists)),
    }
}

fn rewrite_list(
    editor: &mut Editor,
    scan: &Scan,
    occs: &[SubjectOcc],
    items: &[String],
    container: &str,
    nl: &str,
) -> WriteResult<bool> {
    if items.is_empty() {
        for occ in occs {
            editor.remove(occ.elem.whole.clone());
        }
        return Ok(true);
    }
    if occs.len() == 1 && same_set(&occs[0].values, items) {
        return Ok(true);
    }
    let Some(first) = occs.first() else {
        return Ok(false);
    };
    let desc = &scan.descs[first.desc];
    let rdf = desc
        .rdf
        .clone()
        .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
    let indent = line_indent(editor.content, first.elem.whole.start)
        .or_else(|| desc.child_indent.clone())
        .unwrap_or_else(|| format!("{} ", desc.indent.clone().unwrap_or_default()));
    editor.replace(
        first.elem.whole.clone(),
        render_list(&first.elem.qname, &rdf, container, items, &indent, nl),
    );
    for occ in &occs[1..] {
        editor.remove(occ.elem.whole.clone());
    }
    Ok(true)
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

    let rr_color = values.label.as_deref().and_then(normalize_label);
    let legacy_label = values.label.is_some() && rr_color.is_none();
    let file_color = xmp_color(&before);
    let mut dt_insert: Option<String> = None;
    let mut dk_insert: Option<String> = None;
    if let (true, Some(label)) = (legacy_label, &values.label) {
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
    } else if rr_color.as_deref() != file_color {
        let dt_target = target.and_then(|t| t.dt.clone());
        let dk_target = target.and_then(|t| t.dk.clone());
        for occ in &scan.dt_labels {
            match (&rr_color, &occ.desc) {
                (Some(color), desc) => {
                    let rdf = scan.descs[*desc]
                        .rdf
                        .clone()
                        .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                    let indent = line_indent(content, occ.elem.whole.start).unwrap_or_default();
                    let index = darktable_index(color).unwrap_or(0).to_string();
                    editor.replace(
                        occ.elem.whole.clone(),
                        render_list(&occ.elem.qname, &rdf, "Seq", &[index], &indent, nl),
                    );
                }
                (None, _) => editor.remove(occ.elem.whole.clone()),
            }
        }
        for occ in &scan.dk_labels {
            let code = rr_color.as_deref().map_or("0", digikam_code);
            if occ.decoded != code {
                editor.set_value(&occ.loc, code);
            }
        }
        for occ in &scan.labels {
            match (&rr_color, &values.label) {
                (Some(_), Some(label)) => {
                    if occ.decoded.to_lowercase() != label.to_lowercase() {
                        editor.set_value(&occ.loc, &escape(label.as_str()));
                    }
                }
                _ => {
                    if normalize_label(&occ.decoded).is_some() {
                        match &occ.loc {
                            Loc::Attr(a) => editor.remove(a.whole.clone()),
                            Loc::Elem(el) => editor.remove(el.whole.clone()),
                        }
                    }
                }
            }
        }
        let no_field =
            scan.dt_labels.is_empty() && scan.dk_labels.is_empty() && scan.labels.is_empty();
        if let (true, Some(color), Some(label)) = (no_field, &rr_color, &values.label) {
            if dt_target.is_some() {
                dt_insert = Some(darktable_index(color).unwrap_or(0).to_string());
            } else if dk_target.is_some() {
                dk_insert = Some(digikam_code(color).to_string());
            } else {
                needs_xmp = true;
                label_insert = Some(attr(
                    &format!("{}:Label", xmp_prefix),
                    &escape(label.as_str()),
                ));
            }
        }
    }

    if needs_xmp && xmp_clash {
        return Err(XmpWriteError::PrefixClash(
            "prefix xmp is bound to another namespace".to_string(),
        ));
    }

    let props = keyword_properties(&values.keywords, &old_keywords(&scan));
    let mut subject_items: Option<Vec<String>> = None;
    if props.subject.is_empty() {
        for occ in &scan.subjects {
            editor.remove(occ.elem.whole.clone());
        }
    } else if scan.subjects.len() == 1 && same_set(&scan.subjects[0].values, &props.subject) {
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
            render_list(&first.elem.qname, &rdf, "Bag", &props.subject, &indent, nl),
        );
        for occ in &scan.subjects[1..] {
            editor.remove(occ.elem.whole.clone());
        }
    } else {
        subject_insert = true;
        needs_dc = true;
        subject_items = Some(props.subject.clone());
    }

    let mut hierarchical_insert: Option<Vec<String>> = None;
    if let Some(items) = &props.hierarchical
        && !rewrite_list(&mut editor, &scan, &scan.hierarchical, items, "Bag", nl)?
    {
        hierarchical_insert = Some(items.clone());
    }
    if let Some(items) = &props.tags_list {
        rewrite_list(&mut editor, &scan, &scan.tags_lists, items, "Seq", nl)?;
    }
    let needs_lr = hierarchical_insert.is_some();
    let after_subject = needs_lr
        && !props.subject.is_empty()
        && scan.subjects.first().is_some_and(|first| first.desc == 0);

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
            let lr_prefix = match &t.lr {
                Some(p) => p.clone(),
                None if needs_lr && t.lr_clash => {
                    return Err(XmpWriteError::PrefixClash(
                        "prefix lr is bound to another namespace".to_string(),
                    ));
                }
                None => "lr".to_string(),
            };
            let lr_decl = t.lr.is_none() && needs_lr;
            let mut attrs: Vec<String> = Vec::new();
            if needs_xmp && xmp_decl {
                attrs.push(attr("xmlns:xmp", NS_XMP_STR));
            }
            if dc_decl {
                attrs.push(attr("xmlns:dc", NS_DC_STR));
            }
            if lr_decl {
                attrs.push(attr("xmlns:lr", NS_LR_STR));
            }
            attrs.extend(rating_insert);
            attrs.extend(label_insert);
            if let (Some(code), Some(dk)) = (&dk_insert, &t.dk) {
                attrs.push(attr(&format!("{}:ColorLabel", dk), code));
            }
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
            let mut blocks: Vec<String> = Vec::new();
            if let (Some(index), Some(dt)) = (&dt_insert, &t.dt) {
                let rdf = t
                    .rdf
                    .clone()
                    .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                blocks.push(format!(
                    "{}{}",
                    child_indent,
                    render_list(
                        &format!("{}:colorlabels", dt),
                        &rdf,
                        "Seq",
                        std::slice::from_ref(index),
                        &child_indent,
                        nl
                    )
                ));
            }
            if subject_insert {
                let rdf = t
                    .rdf
                    .clone()
                    .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                let items = subject_items.take().unwrap_or_default();
                blocks.push(format!(
                    "{}{}",
                    child_indent,
                    render_list(
                        &format!("{}:subject", dc_prefix),
                        &rdf,
                        "Bag",
                        &items,
                        &child_indent,
                        nl
                    )
                ));
            }
            if let (Some(items), Some(subject)) = (&hierarchical_insert, scan.subjects.first()) {
                let rdf = t
                    .rdf
                    .clone()
                    .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                let indent = line_indent(content, subject.elem.whole.start)
                    .unwrap_or_else(|| child_indent.clone());
                let list = render_list(
                    &format!("{}:hierarchicalSubject", lr_prefix),
                    &rdf,
                    "Bag",
                    items,
                    &indent,
                    nl,
                );
                if after_subject {
                    let at = subject.elem.whole.end;
                    editor.replace(at..at, format!("{nl}{indent}{list}"));
                }
            }
            if let (Some(items), false) = (&hierarchical_insert, after_subject) {
                let rdf = t
                    .rdf
                    .clone()
                    .ok_or_else(|| unsupported("rdf namespace has no prefix"))?;
                blocks.push(format!(
                    "{}{}",
                    child_indent,
                    render_list(
                        &format!("{}:hierarchicalSubject", lr_prefix),
                        &rdf,
                        "Bag",
                        items,
                        &child_indent,
                        nl
                    )
                ));
            }
            let block = (!blocks.is_empty()).then(|| blocks.join(nl));

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
            if rating_insert.is_some()
                || label_insert.is_some()
                || subject_insert
                || hierarchical_insert.is_some()
            {
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
                if hierarchical_insert.is_some() {
                    open.push_str(&format!(" xmlns:lr=\"{}\"", NS_LR_STR));
                }
                if let Some(a) = &rating_insert {
                    open.push(' ');
                    open.push_str(a);
                }
                if let Some(a) = &label_insert {
                    open.push(' ');
                    open.push_str(a);
                }
                let mut lists = Vec::new();
                if subject_insert {
                    let items = subject_items.take().unwrap_or_default();
                    lists.push(render_list(
                        "dc:subject",
                        &prefix,
                        "Bag",
                        &items,
                        &child_indent,
                        nl,
                    ));
                }
                if let Some(items) = &hierarchical_insert {
                    lists.push(render_list(
                        "lr:hierarchicalSubject",
                        &prefix,
                        "Bag",
                        items,
                        &child_indent,
                        nl,
                    ));
                }
                let body = if !lists.is_empty() {
                    format!(
                        "{open}>{nl}{child_indent}{}{nl}{desc_indent}</{prefix}:Description>",
                        lists.join(&format!("{nl}{child_indent}"))
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
    let label_ok = match (&values.label, legacy_label) {
        (Some(v), true) => after
            .label
            .as_ref()
            .is_some_and(|a| a.to_lowercase() == v.to_lowercase()),
        _ => xmp_color(&after) == rr_color.as_deref(),
    };
    if !label_ok {
        return Err(XmpWriteError::Verify("label differs".to_string()));
    }
    if !same_set(&after.subject, &props.subject) {
        return Err(XmpWriteError::Verify("keywords differ".to_string()));
    }
    if let Some(items) = &props.hierarchical
        && !same_set(&after.hierarchical_subject, items)
    {
        return Err(XmpWriteError::Verify(
            "hierarchical keywords differ".to_string(),
        ));
    }
    if let Some(items) = &props.tags_list {
        let rescan = scan_xmp(&out).map_err(|e| XmpWriteError::Verify(e.to_string()))?;
        if !same_set(&old_keywords(&rescan).tags_list.unwrap_or_default(), items) {
            return Err(XmpWriteError::Verify("digiKam tags differ".to_string()));
        }
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

fn bare_name(value: &str) -> &str {
    let value = value.trim();
    value.strip_prefix(USER_TAG_PREFIX).unwrap_or(value).trim()
}

pub(crate) fn normalize_keywords<S: AsRef<str>>(keywords: &[S]) -> Vec<String> {
    let set: BTreeSet<String> = keywords
        .iter()
        .filter_map(|k| normalize_keyword(bare_name(k.as_ref())))
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
    XmpSyncState::new(
        rating,
        xmp_color(fields).map(str::to_string),
        merge_keywords(&fields.subject, &fields.hierarchical_subject),
    )
}

pub(crate) fn classify(
    base: Option<&XmpSyncState>,
    rr: &XmpSyncState,
    xmp: &XmpSyncState,
    rr_wins_rating_tie: bool,
) -> SyncAction {
    if rr == xmp {
        return SyncAction::InSync;
    }
    let mut adjusted;
    let mut xmp = xmp;
    if base.is_none() && rr_wins_rating_tie && rr.rating != 0 {
        adjusted = xmp.clone();
        adjusted.rating = rr.rating;
        if *rr == adjusted {
            return SyncAction::Write;
        }
        xmp = &adjusted;
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
    tags.iter().any(|t| {
        !t.starts_with(COLOR_TAG_PREFIX)
            && normalize_keyword(bare_name(t)).as_deref() == Some(keyword)
    })
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
        let name = normalize_keyword(bare_name(t)).unwrap_or_default();
        !is_synced_keyword(&name) || wanted.contains(name.as_str())
    });
    if let Some(legacy) = legacy_names {
        for tag in tags.iter_mut() {
            if tag.starts_with(COLOR_TAG_PREFIX) || tag.starts_with(USER_TAG_PREFIX) {
                continue;
            }
            let name = normalize_keyword(bare_name(tag)).unwrap_or_default();
            if legacy.contains(&name) {
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
        let name = bare_name(tag);
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
    rating_tie: bool,
}

impl ReadSidecar {
    fn new(path: PathBuf, content: String, fields: &XmpFields, metadata: &ImageMetadata) -> Self {
        let file = sidecar_file_name(&path);
        let base = matching_base(metadata, &file);
        let xmp = xmp_state(fields, &base.clone().unwrap_or_default());
        let rating_tie = base.is_none() && fields.darktable_ns;
        ReadSidecar {
            path,
            file,
            content,
            base,
            xmp,
            rating_tie,
        }
    }
}

fn upgrade_hierarchy(metadata: &mut ImageMetadata, fields: &XmpFields) {
    let xmp = normalize_keywords(&merge_keywords(
        &fields.subject,
        &fields.hierarchical_subject,
    ));
    upgrade_tags(metadata, &xmp);
    if let Some(base) = metadata.xmp_sync.as_mut()
        && let Some(upgraded) = upgrade_keywords(&base.state.keywords, &xmp)
    {
        base.state.keywords = normalize_keywords(&upgraded);
    }
}

fn read_sidecar(path: PathBuf, metadata: &mut ImageMetadata) -> Result<ReadSidecar, String> {
    let content = fs::read_to_string(&path)
        .map_err(|e| format!("failed to read {}: {}", path.display(), e))?;
    let fields =
        parse_xmp(&content).map_err(|e| format!("failed to parse {}: {}", path.display(), e))?;
    decode_legacy_tags(metadata, &fields.subject);
    upgrade_hierarchy(metadata, &fields);
    Ok(ReadSidecar::new(path, content, &fields, metadata))
}

fn sync_action(metadata: &ImageMetadata, side: &ReadSidecar, rr: &XmpSyncState) -> SyncAction {
    match classify(side.base.as_ref(), rr, &side.xmp, side.rating_tie) {
        SyncAction::Import | SyncAction::Write if metadata.xmp_conflict => SyncAction::Conflict,
        action => action,
    }
}

pub(crate) fn sync_on_load(
    source_path: &Path,
    metadata: &mut ImageMetadata,
    siblings: &StemSiblings,
) -> bool {
    let before = snapshot(metadata);
    let Some(located) = locate_sidecar(source_path, siblings) else {
        return false;
    };
    let Some(path) = located.existing else {
        metadata.xmp_conflict = false;
        return snapshot(metadata) != before;
    };
    let side = match read_xmp_file(&path) {
        Ok(fields) => {
            decode_legacy_tags(metadata, &fields.subject);
            upgrade_hierarchy(metadata, &fields);
            ReadSidecar::new(path, String::new(), &fields, metadata)
        }
        Err(e) => {
            log::warn!("Skipping XMP sync: {}", e);
            return false;
        }
    };

    let rr = rr_state(metadata);
    let first_sync = side.base.is_none();
    match sync_action(metadata, &side, &rr) {
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
    siblings: &StemSiblings,
) -> bool {
    let Some(located) = locate_sidecar(source_path, siblings) else {
        return false;
    };
    let Some(path) = located.existing else {
        if !create_if_missing {
            return false;
        }
        let rr = rr_state(metadata);
        let target = located.create_at;
        let out = match update_xmp(NEW_XMP, &rr.to_values()) {
            Ok(out) => out.unwrap_or_else(|| NEW_XMP.to_string()),
            Err(e) => {
                log::warn!("Not writing XMP for {}: {}", source_path.display(), e);
                return false;
            }
        };
        match create_atomic(&target, out.as_bytes()) {
            Ok(true) => {}
            Ok(false) => {
                log::debug!(
                    "XMP sidecar appeared for {}, leaving it alone",
                    source_path.display()
                );
                return false;
            }
            Err(e) => {
                log::warn!("Not writing XMP for {}: {}", source_path.display(), e);
                return false;
            }
        }
        set_base(metadata, &sidecar_file_name(&target), &rr);
        metadata.xmp_conflict = false;
        return false;
    };
    let tags_before = metadata.tags.clone();
    let base_before = metadata.xmp_sync.clone();
    let side = match read_sidecar(path, metadata) {
        Ok(side) => side,
        Err(e) => {
            log::warn!("Not writing XMP for {}: {}", source_path.display(), e);
            return false;
        }
    };
    let decoded = metadata.tags != tags_before || metadata.xmp_sync != base_before;

    let rr = rr_state(metadata);
    let first_sync = side.base.is_none();
    let imported = match sync_action(metadata, &side, &rr) {
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
    siblings: &StemSiblings,
) -> Result<(), String> {
    let path = locate_sidecar(source_path, siblings)
        .and_then(|located| located.existing)
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
    siblings: &StemSiblings,
) -> Result<Option<XmpConflictInfo>, String> {
    if !metadata.xmp_conflict {
        return Ok(None);
    }
    let Some(path) = locate_sidecar(source_path, siblings).and_then(|located| located.existing)
    else {
        return Ok(None);
    };
    let mut metadata = metadata.clone();
    let side = read_sidecar(path, &mut metadata)?;
    Ok(Some(XmpConflictInfo {
        rr: rr_state(&metadata),
        xmp: side.xmp,
    }))
}

pub(crate) fn needs_backfill(
    image_path: &Path,
    metadata: &ImageMetadata,
    siblings: &StemSiblings,
    settings: &AppSettings,
) -> bool {
    settings.enable_xmp_sync.unwrap_or(false)
        && settings.create_xmp_if_missing.unwrap_or(false)
        && !metadata.xmp_conflict
        && !rr_state(metadata).is_empty()
        && locate_sidecar(image_path, siblings).is_some_and(|located| {
            located.existing.is_none()
                && matching_base(metadata, &sidecar_file_name(&located.create_at)).is_none()
        })
}

fn is_dead_folder_error(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::PermissionDenied
            | std::io::ErrorKind::ReadOnlyFilesystem
            | std::io::ErrorKind::NotConnected
            | std::io::ErrorKind::TimedOut
    )
}

pub(crate) fn write_rrdata_if_unchanged(
    rrdata_path: &Path,
    original: &[u8],
    metadata: &ImageMetadata,
) -> std::io::Result<bool> {
    let json = serde_json::to_string_pretty(metadata).map_err(std::io::Error::other)?;
    replace_if_unchanged(rrdata_path, original, json.as_bytes())
}

pub(crate) struct BackfillBatch {
    cache: SiblingCache,
    skip_dirs: HashSet<PathBuf>,
    warned: HashSet<PathBuf>,
    created: usize,
}

impl BackfillBatch {
    pub(crate) fn new() -> Self {
        BackfillBatch {
            cache: SiblingCache::new(),
            skip_dirs: HashSet::new(),
            warned: HashSet::new(),
            created: 0,
        }
    }

    fn fail_dir(&mut self, dir: &Path, skip: bool, message: &str) {
        if skip {
            self.skip_dirs.insert(dir.to_path_buf());
        }
        if self.warned.insert(dir.to_path_buf()) {
            log::warn!("XMP backfill in {}: {}", dir.display(), message);
        }
    }

    pub(crate) fn run(
        &mut self,
        image_path: &Path,
        rrdata_path: &Path,
        settings: &AppSettings,
    ) -> bool {
        if !settings.enable_xmp_sync.unwrap_or(false)
            || !settings.create_xmp_if_missing.unwrap_or(false)
        {
            return false;
        }
        let Some(dir) = image_path.parent() else {
            return false;
        };
        if self.skip_dirs.contains(dir)
            || is_cloud_placeholder(rrdata_path)
            || is_cloud_placeholder(image_path)
        {
            return false;
        }
        let Ok(bytes) = fs::read(rrdata_path) else {
            return false;
        };
        let Ok(mut metadata) = serde_json::from_slice::<ImageMetadata>(&bytes) else {
            return false;
        };
        let Some(siblings) = self.cache.get(dir) else {
            self.fail_dir(dir, true, "cannot list the folder");
            return false;
        };
        if !needs_backfill(image_path, &metadata, &siblings, settings) {
            return false;
        }
        let Some(located) = locate_sidecar(image_path, &siblings) else {
            return false;
        };
        let target = located.create_at;

        let rr = rr_state(&metadata);
        let content = match update_xmp(NEW_XMP, &rr.to_values()) {
            Ok(out) => out.unwrap_or_else(|| NEW_XMP.to_string()),
            Err(e) => {
                self.fail_dir(dir, false, &e.to_string());
                return false;
            }
        };
        match create_atomic(&target, content.as_bytes()) {
            Ok(true) => {}
            Ok(false) => {
                log::debug!("XMP sidecar {} already exists", target.display());
                return false;
            }
            Err(e) => {
                self.fail_dir(dir, is_dead_folder_error(e.kind()), &e.to_string());
                return false;
            }
        }
        self.created += 1;

        set_base(&mut metadata, &sidecar_file_name(&target), &rr);
        match write_rrdata_if_unchanged(rrdata_path, &bytes, &metadata) {
            Ok(true) => {}
            Ok(false) => log::debug!(
                "Skipping XMP base for {}, the rrdata changed",
                image_path.display()
            ),
            Err(e) => self.fail_dir(dir, is_dead_folder_error(e.kind()), &e.to_string()),
        }
        true
    }

    pub(crate) fn finish(self) {
        if self.created > 0 {
            log::info!("Created {} XMP sidecars from rrdata", self.created);
        }
    }
}
