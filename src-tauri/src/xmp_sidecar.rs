use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::XmlVersion;
use quick_xml::escape::{escape, partial_escape, unescape};
use quick_xml::events::{BytesStart, Event};
use quick_xml::name::{NamespaceResolver, ResolveResult};
use quick_xml::reader::NsReader;
use regex::{NoExpand, regex};

use crate::file_management::{is_virtual_copy, write_atomic};
use crate::image_processing::{ImageFlag, ImageMetadata, default_adjustments};
use crate::tagging::COLOR_TAG_PREFIX;

const NS_RDF: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";
const NS_XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/";
const NS_DC: &[u8] = b"http://purl.org/dc/elements/1.1/";
const NS_X: &[u8] = b"adobe:ns:meta/";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct XmpFields {
    pub rating: Option<i8>,
    pub label: Option<String>,
    pub subject: Vec<String>,
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
}

fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

impl Collector {
    fn set_rating(&mut self, value: &str) {
        if self.fields.rating.is_none()
            && let Ok(rating) = value.trim().parse::<i8>()
        {
            self.fields.rating = Some(rating);
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

pub fn sync_metadata_from_xmp(source_path: &Path, metadata: &mut ImageMetadata) -> bool {
    let Some(xmp_file) = resolve_xmp_path(source_path) else {
        return false;
    };
    let fields = match read_xmp_file(&xmp_file) {
        Ok(fields) => fields,
        Err(e) => {
            log::warn!("Skipping XMP sync: {}", e);
            return false;
        }
    };

    let mut changed = false;
    let xmp_rating = fields.rating;

    if xmp_rating == Some(XMP_REJECTED_RATING) && metadata.flag.is_none() {
        metadata.flag = Some(ImageFlag::Reject);
        changed = true;
    }

    if metadata.rating == 0
        && let Some(rating) = xmp_rating.and_then(|r| u8::try_from(r).ok())
        && rating != 0
    {
        metadata.rating = rating;
        if !metadata.adjustments.is_object() {
            metadata.adjustments = default_adjustments();
        }
        if let Some(obj) = metadata.adjustments.as_object_mut() {
            obj.insert("rating".to_string(), serde_json::json!(rating));
        }
        changed = true;
    }

    let mut current_tags = metadata.tags.clone().unwrap_or_default();
    let original_len = current_tags.len();
    let had_no_tags = metadata.tags.is_none();

    let mut replaced = false;
    for tag in &fields.subject {
        for existing in current_tags.iter_mut() {
            if existing != tag && unescape(existing).is_ok_and(|u| u == tag.as_str()) {
                *existing = tag.clone();
                replaced = true;
            }
        }
    }
    if replaced {
        let mut seen = HashSet::new();
        current_tags.retain(|t| seen.insert(t.clone()));
    }

    for tag in fields.subject {
        if !current_tags.contains(&tag) {
            current_tags.push(tag);
        }
    }

    if let Some(label) = fields.label {
        let label_tag = format!("{}{}", COLOR_TAG_PREFIX, label.to_lowercase());
        if !current_tags.contains(&label_tag) {
            current_tags.retain(|t| !t.starts_with(COLOR_TAG_PREFIX));
            current_tags.push(label_tag);
        }
    }

    if replaced || current_tags.len() != original_len || (had_no_tags && !current_tags.is_empty()) {
        metadata.tags = Some(current_tags);
        changed = true;
    }
    changed
}

pub fn sync_metadata_to_xmp(
    sidecar_path: &Path,
    source_path: &Path,
    metadata: &ImageMetadata,
    create_if_missing: bool,
) {
    if is_virtual_copy(sidecar_path, source_path) {
        return;
    }

    let xmp_path = source_path.with_extension("xmp");
    let xmp_path_upper = source_path.with_extension("XMP");

    let mut actual_xmp = if xmp_path.exists() {
        Some(xmp_path.clone())
    } else if xmp_path_upper.exists() {
        Some(xmp_path_upper.clone())
    } else {
        None
    };

    let mut new_content: Option<String> = None;
    if actual_xmp.is_none() {
        if !create_if_missing {
            return;
        }
        let skeleton = r#"<?xml version="1.0" encoding="UTF-8"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="RapidRAW">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        new_content = Some(skeleton.to_string());
        actual_xmp = Some(xmp_path);
    }

    if let Some(xmp_file) = actual_xmp {
        let file_existed = new_content.is_none();
        let original = match new_content {
            Some(skeleton) => skeleton,
            None => match fs::read_to_string(&xmp_file) {
                Ok(c) => c,
                Err(e) => {
                    log::warn!("Failed to read XMP {}: {}", xmp_file.display(), e);
                    return;
                }
            },
        };
        if file_existed && let Err(e) = parse_xmp(&original) {
            log::warn!("Not touching XMP {}: {}", xmp_file.display(), e);
            return;
        }
        let mut content = original.clone();
        let rating_str = xmp_rating(metadata);
        let re_rating_attr = regex!(r#"xmp:Rating\s*=\s*"[^"]*""#);
        let re_rating_tag = regex!(r#"<xmp:Rating\s*>[^<]*</xmp:Rating>"#);

        if re_rating_attr.is_match(&content) {
            let replacement = format!("xmp:Rating=\"{}\"", rating_str);
            content = re_rating_attr
                .replace(&content, NoExpand(&replacement))
                .to_string();
        } else if re_rating_tag.is_match(&content) {
            let replacement = format!("<xmp:Rating>{}</xmp:Rating>", rating_str);
            content = re_rating_tag
                .replace(&content, NoExpand(&replacement))
                .to_string();
        } else if let Some(last_index) = content.rfind("</rdf:Description>") {
            let (start, end) = content.split_at(last_index);
            content = format!("{} <xmp:Rating>{}</xmp:Rating>\n{}", start, rating_str, end);
        }

        let current_tags = metadata.tags.clone().unwrap_or_default();
        let label = xmp_label(&current_tags);
        let normal_tags: Vec<String> = current_tags
            .into_iter()
            .filter(|t| !t.starts_with(COLOR_TAG_PREFIX))
            .collect();

        if let Some(lbl) = label {
            let re_label_attr = regex!(r#"xmp:Label\s*=\s*"[^"]*""#);
            let re_label_tag = regex!(r#"<xmp:Label\s*>[^<]*</xmp:Label>"#);

            if re_label_attr.is_match(&content) {
                let replacement = format!("xmp:Label=\"{}\"", escape(lbl.as_str()));
                content = re_label_attr
                    .replace(&content, NoExpand(&replacement))
                    .to_string();
            } else if re_label_tag.is_match(&content) {
                let replacement =
                    format!("<xmp:Label>{}</xmp:Label>", partial_escape(lbl.as_str()));
                content = re_label_tag
                    .replace(&content, NoExpand(&replacement))
                    .to_string();
            } else if let Some(last_index) = content.rfind("</rdf:Description>") {
                let (start, end) = content.split_at(last_index);
                content = format!(
                    "{} <xmp:Label>{}</xmp:Label>\n{}",
                    start,
                    partial_escape(lbl.as_str()),
                    end
                );
            }
        } else {
            let re_label_attr = regex!(r#"\s*xmp:Label\s*=\s*"[^"]*""#);
            let re_label_tag = regex!(r#"\s*<xmp:Label\s*>[^<]*</xmp:Label>"#);
            content = re_label_attr.replace_all(&content, "").to_string();
            content = re_label_tag.replace_all(&content, "").to_string();
        }

        let re_subject = regex!(r#"(?s)<dc:subject>\s*<rdf:Bag>.*?</rdf:Bag>\s*</dc:subject>"#);
        if normal_tags.is_empty() {
            content = re_subject.replace_all(&content, "").to_string();
        } else {
            let escaped: Vec<_> = normal_tags
                .iter()
                .map(|t| partial_escape(t.as_str()))
                .collect();
            let bag = xmp_subject_bag(&escaped);

            if re_subject.is_match(&content) {
                content = re_subject.replace(&content, NoExpand(&bag)).to_string();
            } else if let Some(last_index) = content.rfind("</rdf:Description>") {
                let (start, end) = content.split_at(last_index);
                content = format!("{} {}\n  {}", start, bag, end);
            }
        }

        if !file_existed || content != original {
            if let Err(e) = parse_xmp(&content) {
                log::warn!("Not writing XMP {}: {}", xmp_file.display(), e);
                return;
            }
            if let Err(e) = write_atomic(&xmp_file, content.as_bytes()) {
                log::error!("Failed to write XMP {}: {}", xmp_file.display(), e);
            }
        }
    }
}
