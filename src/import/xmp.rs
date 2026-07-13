//! XMP sidecar read/write for photo metadata.
//!
//! Implements a practical subset of ISO 16684-1 (RDF/XML serialization)
//! focused on photo library workflows: rating, color label, keywords,
//! title/description, creator, copyright, capture dates, and Camera Raw
//! develop settings (`crs:`).
//!
//! Sidecar discovery follows Adobe conventions:
//! 1. `photo.ext.xmp` (e.g. `IMG_001.CR2.xmp`)
//! 2. `photo.xmp` (stem only)

use std::io::Write;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};

use crate::catalog::PhotoInsert;
use crate::develop::DevelopSettings;

/// Dublin Core namespace.
const NS_DC: &str = "http://purl.org/dc/elements/1.1/";
/// Basic XMP namespace.
const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
/// Photoshop namespace (DateCreated).
const NS_PHOTOSHOP: &str = "http://ns.adobe.com/photoshop/1.0/";
/// Adobe Camera Raw develop settings.
const NS_CRS: &str = "http://ns.adobe.com/camera-raw-settings/1.0/";
/// RDF namespace.
const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
/// xml:lang attribute namespace.
const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";

/// Neutral Kelvin used when mapping relative temp ↔ absolute Temperature.
const TEMP_BASE_K: f32 = 5500.0;
/// UI temp unit → Kelvin offset scale.
const TEMP_SCALE: f32 = 50.0;

/// Metadata extracted from (or written to) an XMP packet / sidecar.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct XmpData {
    /// Star rating, typically 0–5. `-1` means rejected (Lightroom).
    pub rating: Option<i64>,
    /// Color label name (`Red`, `Yellow`, `Green`, `Blue`, `Purple`, …).
    pub label: Option<String>,
    /// Keywords from `dc:subject` (unordered Bag).
    pub keywords: Vec<String>,
    /// Preferred title (`dc:title` LangAlt or simple).
    pub title: Option<String>,
    /// Preferred description / caption (`dc:description`).
    pub description: Option<String>,
    /// First creator (`dc:creator` Seq/Bag or simple).
    pub creator: Option<String>,
    /// Rights / copyright (`dc:rights` or simple).
    pub copyright: Option<String>,
    /// ISO-8601-ish create date (`xmp:CreateDate` / `photoshop:DateCreated`).
    pub create_date: Option<String>,
    /// ISO-8601-ish modify date (`xmp:ModifyDate`).
    pub modify_date: Option<String>,
    /// Camera Raw / develop adjustments (`crs:`), when present.
    pub develop: Option<DevelopSettings>,
}

impl XmpData {
    /// Map color label name to catalog integer (0 = none).
    pub fn color_label_id(&self) -> Option<i64> {
        self.label.as_deref().map(label_name_to_id)
    }

    /// Apply XMP fields onto a `PhotoInsert` without overwriting existing
    /// `Some` capture metadata (same precedence style as EXIF).
    ///
    /// Rating / color label always apply when present in XMP (they are
    /// library annotations, not capture data).
    pub fn apply_to(&self, p: &mut PhotoInsert) {
        if let Some(r) = self.rating {
            p.rating = Some(r.clamp(-1, 5));
        }
        if let Some(id) = self.color_label_id() {
            p.color_label = Some(id);
        }
        if !self.keywords.is_empty() {
            p.keywords = self.keywords.clone();
        }
        p.copyright = p.copyright.clone().or_else(|| self.copyright.clone());
        if p.date_taken.is_none()
            && let Some(ref d) = self.create_date
        {
            p.date_taken = parse_xmp_datetime(d);
        }
    }
}

/// Locate an XMP sidecar next to `image_path`, if any.
///
/// Checks, in order:
/// 1. `{filename}.xmp` — Adobe style (`IMG_001.CR2.xmp`)
/// 2. `{stem}.xmp` — stem only (`IMG_001.xmp`)
///
/// Case-insensitive match on the extension (`.xmp` / `.XMP`).
pub fn find_sidecar(image_path: &Path) -> Option<PathBuf> {
    let parent = image_path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = image_path.file_name()?.to_string_lossy();

    // 1. full filename + .xmp
    let adobe = parent.join(format!("{file_name}.xmp"));
    if adobe.is_file() {
        return Some(adobe);
    }
    let adobe_upper = parent.join(format!("{file_name}.XMP"));
    if adobe_upper.is_file() {
        return Some(adobe_upper);
    }

    // 2. stem + .xmp
    let stem = image_path.file_stem()?.to_string_lossy();
    let stem_path = parent.join(format!("{stem}.xmp"));
    if stem_path.is_file() {
        return Some(stem_path);
    }
    let stem_upper = parent.join(format!("{stem}.XMP"));
    if stem_upper.is_file() {
        return Some(stem_upper);
    }

    None
}

/// Destination sidecar path for a photo that will live at `dest_image`.
/// Mirrors the naming style of `source_sidecar` relative to `source_image`.
pub fn sidecar_dest_for(
    source_image: &Path,
    source_sidecar: &Path,
    dest_image: &Path,
) -> PathBuf {
    let src_name = source_image
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let side_name = source_sidecar
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let dest_parent = dest_image.parent().unwrap_or_else(|| Path::new("."));
    let dest_file = dest_image
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());

    // Adobe style: sidecar = "{image_filename}.xmp"
    if side_name.eq_ignore_ascii_case(&format!("{src_name}.xmp")) {
        return dest_parent.join(format!("{dest_file}.xmp"));
    }

    // Stem style: sidecar = "{stem}.xmp"
    let dest_stem = dest_image
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| dest_file.clone());
    dest_parent.join(format!("{dest_stem}.xmp"))
}

/// Parse an XMP sidecar file.
pub fn parse_xmp_file(path: &Path) -> Result<XmpData, XmpError> {
    let bytes = std::fs::read(path)?;
    parse_xmp_bytes(&bytes)
}

/// Parse XMP from raw bytes (handles UTF-8 BOM and `xpacket` wrappers).
pub fn parse_xmp_bytes(bytes: &[u8]) -> Result<XmpData, XmpError> {
    let text = decode_xmp_text(bytes);
    parse_xmp_str(&text)
}

/// Parse XMP RDF/XML text into [`XmpData`].
pub fn parse_xmp_str(xml: &str) -> Result<XmpData, XmpError> {
    let xml = strip_xpacket(xml);
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut data = XmpData::default();
    let mut buf = Vec::new();
    let mut ns_map = default_ns_map();

    // Open element stack (expanded names), excluding RDF containers.
    let mut stack: Vec<ExpandedName> = Vec::new();
    // When inside Bag/Seq/Alt of a property, that property name.
    let mut array_of: Option<ExpandedName> = None;
    let mut in_li = false;
    let mut li_lang: Option<String> = None;
    let mut text_buf = String::new();
    let mut langalt_values: Vec<(Option<String>, String)> = Vec::new();

    loop {
        let event = reader.read_event_into(&mut buf);
        match event {
            Ok(Event::Start(e)) => {
                ingest_xmlns(&e, &mut ns_map);
                let name = expand_name(e.name().as_ref(), &ns_map);

                // Attribute-form properties on Description and elsewhere.
                apply_element_attrs(&e, &ns_map, &mut data);

                if is_rdf_array(&name) {
                    array_of = stack.last().cloned();
                    langalt_values.clear();
                } else if name.ns == NS_RDF && name.local == "li" {
                    in_li = true;
                    li_lang = attr_lang(&e, &ns_map);
                    text_buf.clear();
                } else if !is_rdf_skeleton(&name) {
                    stack.push(name);
                    text_buf.clear();
                }
            }
            Ok(Event::Empty(e)) => {
                ingest_xmlns(&e, &mut ns_map);
                let name = expand_name(e.name().as_ref(), &ns_map);
                apply_element_attrs(&e, &ns_map, &mut data);

                if name.ns == NS_RDF && name.local == "li" {
                    // Empty li — nothing to add.
                } else if !is_rdf_skeleton(&name) && !is_rdf_array(&name) {
                    // Empty property element: value only via attributes (done)
                    // or empty simple value — ignore.
                }
            }
            Ok(Event::End(e)) => {
                let name = expand_name(e.name().as_ref(), &ns_map);

                if name.ns == NS_RDF && name.local == "li" {
                    let text = text_buf.trim().to_string();
                    if let Some(ref prop) = array_of {
                        if is_langalt_prop(prop) {
                            langalt_values.push((li_lang.take(), text));
                        } else {
                            apply_array_item(&mut data, prop, &text);
                        }
                    }
                    in_li = false;
                    text_buf.clear();
                } else if is_rdf_array(&name) {
                    if let Some(ref prop) = array_of
                        && is_langalt_prop(prop)
                        && let Some(v) = pick_langalt(&langalt_values)
                    {
                        apply_simple_prop(&mut data, prop, &v);
                    }
                    array_of = None;
                    langalt_values.clear();
                } else if !is_rdf_skeleton(&name) {
                    // Closing a property element.
                    if !in_li && array_of.is_none() {
                        let text = text_buf.trim().to_string();
                        if !text.is_empty() {
                            apply_simple_prop(&mut data, &name, &text);
                        }
                    }
                    if let Some(pos) = stack.iter().rposition(|s| s == &name) {
                        stack.truncate(pos);
                    }
                    text_buf.clear();
                }
            }
            Ok(Event::Text(t)) => {
                if let Ok(s) = t.decode() {
                    text_buf.push_str(&s);
                }
            }
            Ok(Event::CData(t)) => {
                if let Ok(s) = std::str::from_utf8(t.as_ref()) {
                    text_buf.push_str(s);
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(XmpError::Xml(e.to_string())),
            _ => {}
        }
        buf.clear();
    }

    Ok(data)
}

/// Write a canonical XMP sidecar next to (or at) `path`.
pub fn write_xmp_file(path: &Path, data: &XmpData) -> Result<(), XmpError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let xml = serialize_xmp(data);
    let mut f = std::fs::File::create(path)?;
    f.write_all(xml.as_bytes())?;
    Ok(())
}

/// Serialize [`XmpData`] to a complete XMP packet (with `xpacket` wrapper).
pub fn serialize_xmp(data: &XmpData) -> String {
    let mut body = String::new();
    body.push_str(
        r#"<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="realraw">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/""#,
    );

    if let Some(r) = data.rating {
        body.push_str(&format!("\n   xmp:Rating=\"{r}\""));
    }
    if let Some(ref label) = data.label {
        body.push_str(&format!("\n   xmp:Label=\"{}\"", escape_xml_attr(label)));
    }
    if let Some(ref d) = data.create_date {
        body.push_str(&format!("\n   xmp:CreateDate=\"{}\"", escape_xml_attr(d)));
        body.push_str(&format!(
            "\n   photoshop:DateCreated=\"{}\"",
            escape_xml_attr(d)
        ));
    }
    if let Some(ref d) = data.modify_date {
        body.push_str(&format!("\n   xmp:ModifyDate=\"{}\"", escape_xml_attr(d)));
    }

    if let Some(ref dev) = data.develop {
        let has = !dev.is_identity();
        body.push_str(&format!(
            "\n   crs:Version=\"15.0\"\n   crs:ProcessVersion=\"11.0\"\n   crs:HasSettings=\"{}\"",
            if has { "True" } else { "False" }
        ));
        body.push_str(&format!(
            "\n   crs:Exposure2012=\"{}\"",
            fmt_f32(dev.exposure)
        ));
        body.push_str(&format!(
            "\n   crs:Contrast2012=\"{}\"",
            fmt_f32(dev.contrast)
        ));
        body.push_str(&format!(
            "\n   crs:Highlights2012=\"{}\"",
            fmt_f32(dev.highlights)
        ));
        body.push_str(&format!(
            "\n   crs:Shadows2012=\"{}\"",
            fmt_f32(dev.shadows)
        ));
        body.push_str(&format!("\n   crs:Whites2012=\"{}\"", fmt_f32(dev.whites)));
        body.push_str(&format!("\n   crs:Blacks2012=\"{}\"", fmt_f32(dev.blacks)));
        body.push_str(&format!(
            "\n   crs:Clarity2012=\"{}\"",
            fmt_f32(dev.clarity)
        ));
        body.push_str(&format!("\n   crs:Vibrance=\"{}\"", fmt_f32(dev.vibrance)));
        body.push_str(&format!(
            "\n   crs:Saturation=\"{}\"",
            fmt_f32(dev.saturation)
        ));
        body.push_str(&format!(
            "\n   crs:Temperature=\"{}\"",
            fmt_f32(temp_to_kelvin(dev.temp))
        ));
        body.push_str(&format!("\n   crs:Tint=\"{}\"", fmt_f32(dev.tint)));
    }

    body.push_str(">\n");

    if let Some(ref title) = data.title {
        body.push_str("   <dc:title>\n    <rdf:Alt>\n");
        body.push_str(&format!(
            "     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n",
            escape_xml_text(title)
        ));
        body.push_str("    </rdf:Alt>\n   </dc:title>\n");
    }
    if let Some(ref desc) = data.description {
        body.push_str("   <dc:description>\n    <rdf:Alt>\n");
        body.push_str(&format!(
            "     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n",
            escape_xml_text(desc)
        ));
        body.push_str("    </rdf:Alt>\n   </dc:description>\n");
    }
    if let Some(ref creator) = data.creator {
        body.push_str("   <dc:creator>\n    <rdf:Seq>\n");
        body.push_str(&format!(
            "     <rdf:li>{}</rdf:li>\n",
            escape_xml_text(creator)
        ));
        body.push_str("    </rdf:Seq>\n   </dc:creator>\n");
    }
    if let Some(ref rights) = data.copyright {
        body.push_str("   <dc:rights>\n    <rdf:Alt>\n");
        body.push_str(&format!(
            "     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n",
            escape_xml_text(rights)
        ));
        body.push_str("    </rdf:Alt>\n   </dc:rights>\n");
    }
    if !data.keywords.is_empty() {
        body.push_str("   <dc:subject>\n    <rdf:Bag>\n");
        for kw in &data.keywords {
            body.push_str(&format!(
                "     <rdf:li>{}</rdf:li>\n",
                escape_xml_text(kw)
            ));
        }
        body.push_str("    </rdf:Bag>\n   </dc:subject>\n");
    }

    body.push_str("  </rdf:Description>\n </rdf:RDF>\n</x:xmpmeta>\n");

    let mut out = String::new();
    out.push_str("<?xpacket begin=\"\u{FEFF}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n");
    out.push_str(&body);
    // Padding so other tools can rewrite the packet in place.
    for _ in 0..32 {
        out.push_str(&" ".repeat(80));
        out.push('\n');
    }
    out.push_str("<?xpacket end=\"w\"?>\n");
    out
}

/// Build [`XmpData`] from catalog fields for write-back.
#[allow(clippy::too_many_arguments)]
pub fn xmp_from_photo_fields(
    rating: i64,
    color_label: i64,
    keywords: &[String],
    copyright: Option<&str>,
    date_taken_unix: Option<i64>,
    title: Option<&str>,
    description: Option<&str>,
    develop: Option<&DevelopSettings>,
) -> XmpData {
    XmpData {
        rating: Some(rating),
        label: label_id_to_name(color_label).map(str::to_string),
        keywords: keywords.to_vec(),
        title: title.map(str::to_string),
        description: description.map(str::to_string),
        creator: None,
        copyright: copyright.map(str::to_string),
        create_date: date_taken_unix.and_then(format_xmp_datetime),
        modify_date: format_xmp_datetime(time::OffsetDateTime::now_utc().unix_timestamp()),
        develop: develop.cloned(),
    }
}

/// Write an Adobe-style XMP sidecar next to `image_path`
/// (`{filename}.xmp`).
pub fn write_sidecar_for_image(image_path: &Path, data: &XmpData) -> Result<PathBuf, XmpError> {
    let file_name = image_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "photo".into());
    let sidecar = image_path.with_file_name(format!("{file_name}.xmp"));
    write_xmp_file(&sidecar, data)?;
    Ok(sidecar)
}

/// Merge `develop` into an existing sidecar (if any) and rewrite it.
/// Preserves library metadata already in the file when present.
pub fn update_sidecar_develop(
    image_path: &Path,
    develop: &DevelopSettings,
) -> Result<PathBuf, XmpError> {
    let mut data = find_sidecar(image_path)
        .and_then(|p| parse_xmp_file(&p).ok())
        .unwrap_or_default();
    data.develop = Some(develop.clone());
    data.modify_date = format_xmp_datetime(time::OffsetDateTime::now_utc().unix_timestamp());
    write_sidecar_for_image(image_path, &data)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExpandedName {
    ns: String,
    local: String,
}

fn default_ns_map() -> Vec<(String, String)> {
    vec![
        ("rdf".into(), NS_RDF.into()),
        ("dc".into(), NS_DC.into()),
        ("xmp".into(), NS_XMP.into()),
        ("xap".into(), NS_XMP.into()), // legacy Adobe prefix
        ("photoshop".into(), NS_PHOTOSHOP.into()),
        ("crs".into(), NS_CRS.into()),
        ("xml".into(), NS_XML.into()),
        ("x".into(), "adobe:ns:meta/".into()),
    ]
}

fn develop_mut(data: &mut XmpData) -> &mut DevelopSettings {
    if data.develop.is_none() {
        data.develop = Some(DevelopSettings::default());
    }
    data.develop.as_mut().unwrap()
}

fn parse_f32(v: &str) -> Option<f32> {
    v.parse::<f32>().ok().filter(|f| f.is_finite())
}

fn fmt_f32(v: f32) -> String {
    // Compact float for XMP attributes.
    let s = format!("{v:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
    if s.is_empty() || s == "-" {
        "0".into()
    } else {
        s
    }
}

/// Map absolute Kelvin (crs:Temperature) to relative UI temp.
fn kelvin_to_temp(k: f32) -> f32 {
    ((k - TEMP_BASE_K) / TEMP_SCALE).clamp(-100.0, 100.0)
}

/// Map relative UI temp to absolute Kelvin.
fn temp_to_kelvin(temp: f32) -> f32 {
    TEMP_BASE_K + temp * TEMP_SCALE
}

fn expand_name(raw: &[u8], ns_map: &[(String, String)]) -> ExpandedName {
    let s = String::from_utf8_lossy(raw);
    if let Some((prefix, local)) = s.split_once(':') {
        let ns = ns_map
            .iter()
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.clone())
            .unwrap_or_else(|| prefix.to_string());
        ExpandedName {
            ns,
            local: local.to_string(),
        }
    } else {
        ExpandedName {
            ns: String::new(),
            local: s.into_owned(),
        }
    }
}

fn attr_value(a: &quick_xml::events::attributes::Attribute<'_>) -> String {
    a.normalized_value(XmlVersion::Implicit1_0)
        .unwrap_or_default()
        .into_owned()
}

fn ingest_xmlns(e: &quick_xml::events::BytesStart<'_>, ns_map: &mut Vec<(String, String)>) {
    for a in e.attributes().flatten() {
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        if key == "xmlns" || key.starts_with("xmlns:") {
            let prefix = key.strip_prefix("xmlns:").unwrap_or("").to_string();
            let uri = attr_value(&a);
            ns_map.retain(|(p, _)| p != &prefix);
            ns_map.push((prefix, uri));
        }
    }
}

fn apply_element_attrs(
    e: &quick_xml::events::BytesStart<'_>,
    ns_map: &[(String, String)],
    data: &mut XmpData,
) {
    for a in e.attributes().flatten() {
        let key = a.key.as_ref();
        if key.starts_with(b"xmlns") {
            continue;
        }
        let aname = expand_name(key, ns_map);
        if aname.ns == NS_XML {
            continue;
        }
        if aname.local == "about" && (aname.ns == NS_RDF || aname.ns.is_empty()) {
            continue;
        }
        // Only known metadata attributes.
        if matches!(
            (aname.ns.as_str(), aname.local.as_str()),
            (NS_XMP, _)
                | (NS_DC, _)
                | (NS_PHOTOSHOP, _)
                | (NS_CRS, _)
                | (
                    "",
                    "Rating"
                        | "Label"
                        | "CreateDate"
                        | "ModifyDate"
                        | "DateCreated"
                        | "Exposure2012"
                        | "Contrast2012"
                        | "Highlights2012"
                        | "Shadows2012"
                        | "Whites2012"
                        | "Blacks2012"
                        | "Clarity2012"
                        | "Vibrance"
                        | "Saturation"
                        | "Temperature"
                        | "Tint"
                )
        ) {
            apply_simple_prop(data, &aname, &attr_value(&a));
        }
    }
}

fn attr_lang(
    e: &quick_xml::events::BytesStart<'_>,
    ns_map: &[(String, String)],
) -> Option<String> {
    for a in e.attributes().flatten() {
        let aname = expand_name(a.key.as_ref(), ns_map);
        if aname.ns == NS_XML && aname.local == "lang" {
            return Some(attr_value(&a));
        }
        let raw = String::from_utf8_lossy(a.key.as_ref());
        if raw == "xml:lang" {
            return Some(attr_value(&a));
        }
    }
    None
}

fn is_rdf_array(name: &ExpandedName) -> bool {
    name.ns == NS_RDF && matches!(name.local.as_str(), "Bag" | "Seq" | "Alt")
}

fn is_rdf_skeleton(name: &ExpandedName) -> bool {
    if name.ns == NS_RDF {
        return matches!(
            name.local.as_str(),
            "RDF" | "Description" | "Bag" | "Seq" | "Alt" | "li"
        );
    }
    name.local == "xmpmeta"
}

fn is_langalt_prop(prop: &ExpandedName) -> bool {
    prop.ns == NS_DC && matches!(prop.local.as_str(), "title" | "description" | "rights")
}

fn apply_simple_prop(data: &mut XmpData, name: &ExpandedName, val: &str) {
    let v = val.trim();
    if v.is_empty() {
        return;
    }
    match (name.ns.as_str(), name.local.as_str()) {
        (NS_XMP, "Rating") | ("", "Rating") => {
            if let Ok(n) = v.parse::<i64>() {
                data.rating = Some(n);
            } else if let Ok(f) = v.parse::<f64>() {
                data.rating = Some(f as i64);
            }
        }
        (NS_XMP, "Label") | ("", "Label") => {
            data.label = Some(v.to_string());
        }
        (NS_XMP, "CreateDate") | (NS_PHOTOSHOP, "DateCreated") | ("", "CreateDate")
        | ("", "DateCreated") => {
            if data.create_date.is_none() {
                data.create_date = Some(v.to_string());
            }
        }
        (NS_XMP, "ModifyDate") | ("", "ModifyDate") => {
            data.modify_date = Some(v.to_string());
        }
        (NS_DC, "title") | ("", "title") => {
            data.title = Some(v.to_string());
        }
        (NS_DC, "description") | ("", "description") => {
            data.description = Some(v.to_string());
        }
        (NS_DC, "creator") | ("", "creator") => {
            if data.creator.is_none() {
                data.creator = Some(v.to_string());
            }
        }
        (NS_DC, "rights") | ("", "rights") => {
            data.copyright = Some(v.to_string());
        }
        (NS_DC, "subject") | ("", "subject") => {
            if !data.keywords.iter().any(|k| k == v) {
                data.keywords.push(v.to_string());
            }
        }
        // Camera Raw / develop (process 2012).
        (NS_CRS, "Exposure2012") | ("", "Exposure2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).exposure = f;
            }
        }
        (NS_CRS, "Contrast2012") | ("", "Contrast2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).contrast = f;
            }
        }
        (NS_CRS, "Highlights2012") | ("", "Highlights2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).highlights = f;
            }
        }
        (NS_CRS, "Shadows2012") | ("", "Shadows2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).shadows = f;
            }
        }
        (NS_CRS, "Whites2012") | ("", "Whites2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).whites = f;
            }
        }
        (NS_CRS, "Blacks2012") | ("", "Blacks2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).blacks = f;
            }
        }
        (NS_CRS, "Clarity2012") | ("", "Clarity2012") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).clarity = f;
            }
        }
        (NS_CRS, "Vibrance") | ("", "Vibrance") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).vibrance = f;
            }
        }
        (NS_CRS, "Saturation") | ("", "Saturation") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).saturation = f;
            }
        }
        (NS_CRS, "Temperature") | ("", "Temperature") => {
            if let Some(k) = parse_f32(v) {
                // Absolute Kelvin from LR, or relative if already small.
                develop_mut(data).temp = if k.abs() > 500.0 {
                    kelvin_to_temp(k)
                } else {
                    k
                };
            }
        }
        (NS_CRS, "Tint") | ("", "Tint") => {
            if let Some(f) = parse_f32(v) {
                develop_mut(data).tint = f;
            }
        }
        _ => {}
    }
}

fn apply_array_item(data: &mut XmpData, prop: &ExpandedName, val: &str) {
    let v = val.trim();
    if v.is_empty() {
        return;
    }
    match (prop.ns.as_str(), prop.local.as_str()) {
        (NS_DC, "subject") | ("", "subject") => {
            if !data.keywords.iter().any(|k| k == v) {
                data.keywords.push(v.to_string());
            }
        }
        (NS_DC, "creator") | ("", "creator") if data.creator.is_none() => {
            data.creator = Some(v.to_string());
        }
        _ => {}
    }
}

fn pick_langalt(values: &[(Option<String>, String)]) -> Option<String> {
    for (lang, text) in values {
        if lang
            .as_deref()
            .is_some_and(|l| l.eq_ignore_ascii_case("x-default"))
            && !text.is_empty()
        {
            return Some(text.clone());
        }
    }
    values
        .iter()
        .find(|(_, t)| !t.is_empty())
        .map(|(_, t)| t.clone())
}

fn decode_xmp_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter_map(|c| {
                if c.len() == 2 {
                    Some(u16::from_le_bytes([c[0], c[1]]))
                } else {
                    None
                }
            })
            .collect();
        return String::from_utf16_lossy(&u16s);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let u16s: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter_map(|c| {
                if c.len() == 2 {
                    Some(u16::from_be_bytes([c[0], c[1]]))
                } else {
                    None
                }
            })
            .collect();
        return String::from_utf16_lossy(&u16s);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn strip_xpacket(xml: &str) -> &str {
    let mut s = xml.trim();
    if let Some(rest) = s.strip_prefix("<?xpacket")
        && let Some(end) = rest.find("?>")
    {
        s = rest[end + 2..].trim_start();
    }
    if let Some(idx) = s.rfind("<?xpacket") {
        s = s[..idx].trim_end();
    }
    s
}

fn escape_xml_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn escape_xml_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn label_name_to_id(name: &str) -> i64 {
    match name.trim().to_ascii_lowercase().as_str() {
        "red" => 1,
        "yellow" => 2,
        "green" => 3,
        "blue" => 4,
        "purple" => 5,
        "" | "none" => 0,
        _ => 0,
    }
}

fn label_id_to_name(id: i64) -> Option<&'static str> {
    match id {
        1 => Some("Red"),
        2 => Some("Yellow"),
        3 => Some("Green"),
        4 => Some("Blue"),
        5 => Some("Purple"),
        _ => None,
    }
}

/// Parse XMP / ISO-8601 dates into a Unix timestamp (UTC).
pub fn parse_xmp_datetime(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.len() < 10 {
        return None;
    }
    let year: i32 = s.get(0..4)?.parse().ok()?;
    let month: u32 = s.get(5..7)?.parse().ok()?;
    let day: u32 = s.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let (mut hour, mut minute, mut second) = (0u32, 0u32, 0u32);
    let mut tz_offset_secs: i64 = 0;

    if s.len() >= 19 && (s.as_bytes().get(10) == Some(&b'T') || s.as_bytes().get(10) == Some(&b' '))
    {
        hour = s.get(11..13)?.parse().ok()?;
        minute = s.get(14..16)?.parse().ok()?;
        second = s.get(17..19)?.parse().ok()?;

        let rest = &s[19..];
        let rest = if rest.starts_with('.') {
            rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit())
        } else {
            rest
        };
        if rest.starts_with('Z') || rest.starts_with('z') {
            tz_offset_secs = 0;
        } else if rest.starts_with('+') || rest.starts_with('-') {
            let sign: i64 = if rest.starts_with('+') { 1 } else { -1 };
            let hh: i64 = rest.get(1..3)?.parse().ok()?;
            let mm: i64 = if rest.len() >= 6 {
                rest.get(4..6)?.parse().ok()?
            } else {
                0
            };
            tz_offset_secs = sign * (hh * 3600 + mm * 60);
        }
    }

    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64 - tz_offset_secs)
}

fn format_xmp_datetime(ts: i64) -> Option<String> {
    let dt = time::OffsetDateTime::from_unix_timestamp(ts).ok()?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        dt.year(),
        u8::from(dt.month()),
        dt.day(),
        dt.hour(),
        dt.minute(),
        dt.second()
    ))
}

fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let m = m as i32;
    let d = d as i32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy as u32;
    era as i64 * 146097 + doe as i64 - 719468
}

/// Errors from XMP parse/write.
#[derive(Debug, thiserror::Error)]
pub enum XmpError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml error: {0}")]
    Xml(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::DevelopSettings;
    use tempfile::tempdir;

    const SAMPLE_XMP: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmp:Rating="4"
    xmp:Label="Red"
    xmp:CreateDate="2023-08-15T14:30:00">
   <dc:title>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">Sunset</rdf:li>
     <rdf:li xml:lang="fr">Coucher de soleil</rdf:li>
    </rdf:Alt>
   </dc:title>
   <dc:description>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">A nice photo</rdf:li>
    </rdf:Alt>
   </dc:description>
   <dc:creator>
    <rdf:Seq>
     <rdf:li>Jane Doe</rdf:li>
    </rdf:Seq>
   </dc:creator>
   <dc:rights>
    <rdf:Alt>
     <rdf:li xml:lang="x-default">© Jane</rdf:li>
    </rdf:Alt>
   </dc:rights>
   <dc:subject>
    <rdf:Bag>
     <rdf:li>landscape</rdf:li>
     <rdf:li>sunset</rdf:li>
    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    #[test]
    fn parse_sample_lightroom_style() {
        let d = parse_xmp_str(SAMPLE_XMP).unwrap();
        assert_eq!(d.rating, Some(4));
        assert_eq!(d.label.as_deref(), Some("Red"));
        assert_eq!(d.color_label_id(), Some(1));
        assert_eq!(d.title.as_deref(), Some("Sunset"));
        assert_eq!(d.description.as_deref(), Some("A nice photo"));
        assert_eq!(d.creator.as_deref(), Some("Jane Doe"));
        assert_eq!(d.copyright.as_deref(), Some("© Jane"));
        assert_eq!(d.keywords, vec!["landscape", "sunset"]);
        assert!(d.create_date.as_deref().unwrap().starts_with("2023-08-15"));
    }

    #[test]
    fn round_trip_serialize_parse() {
        let original = XmpData {
            rating: Some(3),
            label: Some("Blue".into()),
            keywords: vec!["cat".into(), "pet".into()],
            title: Some("Fluffy".into()),
            description: Some("A cat".into()),
            creator: Some("Owner".into()),
            copyright: Some("All rights reserved".into()),
            create_date: Some("2020-01-02T03:04:05".into()),
            modify_date: Some("2020-01-03T00:00:00".into()),
            develop: Some(DevelopSettings {
                exposure: 1.5,
                contrast: 20.0,
                highlights: -10.0,
                shadows: 15.0,
                whites: 5.0,
                blacks: -8.0,
                clarity: 12.0,
                vibrance: 7.0,
                saturation: -3.0,
                temp: 10.0,
                tint: -5.0,
            }),
        };
        let xml = serialize_xmp(&original);
        let parsed = parse_xmp_str(&xml).unwrap();
        assert_eq!(parsed.rating, original.rating);
        assert_eq!(parsed.label, original.label);
        assert_eq!(parsed.keywords, original.keywords);
        assert_eq!(parsed.title, original.title);
        assert_eq!(parsed.description, original.description);
        assert_eq!(parsed.creator, original.creator);
        assert_eq!(parsed.copyright, original.copyright);
        assert_eq!(parsed.create_date, original.create_date);
        let dev = parsed.develop.expect("develop settings");
        let orig = original.develop.unwrap();
        assert!((dev.exposure - orig.exposure).abs() < 0.01);
        assert!((dev.contrast - orig.contrast).abs() < 0.01);
        assert!((dev.highlights - orig.highlights).abs() < 0.01);
        assert!((dev.temp - orig.temp).abs() < 0.5);
        assert!((dev.tint - orig.tint).abs() < 0.01);
    }

    #[test]
    fn update_sidecar_writes_develop_next_to_image() {
        let dir = tempdir().unwrap();
        let img = dir.path().join("raw.CR2");
        std::fs::write(&img, b"fake").unwrap();
        let dev = DevelopSettings {
            exposure: 0.75,
            contrast: 10.0,
            ..Default::default()
        };
        let side = update_sidecar_develop(&img, &dev).unwrap();
        assert!(side.is_file());
        assert!(side.file_name().unwrap().to_string_lossy().ends_with(".CR2.xmp"));
        let got = parse_xmp_file(&side).unwrap();
        let d = got.develop.unwrap();
        assert!((d.exposure - 0.75).abs() < 0.01);
        assert!((d.contrast - 10.0).abs() < 0.01);
    }

    #[test]
    fn find_adobe_style_sidecar() {
        let dir = tempdir().unwrap();
        let img = dir.path().join("IMG_001.CR2");
        let xmp = dir.path().join("IMG_001.CR2.xmp");
        std::fs::write(&img, b"raw").unwrap();
        std::fs::write(&xmp, SAMPLE_XMP.as_bytes()).unwrap();
        let found = find_sidecar(&img).unwrap();
        assert_eq!(found, xmp);
    }

    #[test]
    fn find_stem_style_sidecar() {
        let dir = tempdir().unwrap();
        let img = dir.path().join("photo.jpg");
        let xmp = dir.path().join("photo.xmp");
        std::fs::write(&img, b"jpg").unwrap();
        std::fs::write(
            &xmp,
            b"<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"/>",
        )
        .unwrap();
        let found = find_sidecar(&img).unwrap();
        assert_eq!(found, xmp);
    }

    #[test]
    fn sidecar_dest_preserves_adobe_naming() {
        let src = Path::new("/a/IMG.CR2");
        let side = Path::new("/a/IMG.CR2.xmp");
        let dest = Path::new("/b/2024/01/01/IMG.CR2");
        assert_eq!(
            sidecar_dest_for(src, side, dest),
            PathBuf::from("/b/2024/01/01/IMG.CR2.xmp")
        );
    }

    #[test]
    fn parse_xmp_datetime_with_tz() {
        let ts = parse_xmp_datetime("2023-08-15T14:30:00Z").unwrap();
        assert_eq!(ts, 19584 * 86_400 + 14 * 3600 + 30 * 60);
    }

    #[test]
    fn apply_to_sets_rating_and_keywords() {
        let mut p = PhotoInsert::default();
        let d = XmpData {
            rating: Some(5),
            label: Some("Green".into()),
            keywords: vec!["a".into()],
            copyright: Some("c".into()),
            ..Default::default()
        };
        d.apply_to(&mut p);
        assert_eq!(p.rating, Some(5));
        assert_eq!(p.color_label, Some(3));
        assert_eq!(p.keywords, vec!["a"]);
        assert_eq!(p.copyright.as_deref(), Some("c"));
    }

    #[test]
    fn write_and_read_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.xmp");
        let data = XmpData {
            rating: Some(2),
            keywords: vec!["test".into()],
            ..Default::default()
        };
        write_xmp_file(&path, &data).unwrap();
        let got = parse_xmp_file(&path).unwrap();
        assert_eq!(got.rating, Some(2));
        assert_eq!(got.keywords, vec!["test"]);
    }

    #[test]
    fn empty_xmp_is_ok() {
        let d = parse_xmp_str(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
              <rdf:Description rdf:about=""/>
            </rdf:RDF>"#,
        )
        .unwrap();
        assert_eq!(d, XmpData::default());
    }
}
