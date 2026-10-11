//! Decoder for Audacity's serialized project document.
//!
//! Audacity stores its project XML in a compact binary form (`ProjectSerializer`): a
//! stream of typed fields, with element and attribute names replaced by 16-bit ids
//! that a separate dictionary stream defines. This turns the two streams back into a
//! tree. Pure — no I/O.

use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum XmlError {
    #[error("project document is truncated")]
    Truncated,
    #[error("project document has an unknown field type {0}")]
    UnknownField(u8),
    #[error("project document uses an undefined name id {0}")]
    UnknownName(u16),
    #[error("project document has an unsupported character size {0}")]
    CharSize(u8),
    #[error("project document has text that is not valid Unicode")]
    BadText,
    #[error("project document is malformed: {0}")]
    Malformed(&'static str),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Text(String),
    Int(i64),
    Bool(bool),
    Float(f64),
    /// Binary payload (e.g. the project thumbnail); the bytes are not kept.
    Blob,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, Value)>,
    pub children: Vec<Element>,
}

impl Element {
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |c| c.name == name)
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// An element can carry the same attribute name twice (a wave track has a
    /// spectrogram `gain` and a volume `gain`). Audacity applies them in order, so
    /// the last one is the one in effect.
    fn attr(&self, name: &str) -> Option<&Value> {
        self.attrs
            .iter()
            .rev()
            .find_map(|(n, v)| (n == name).then_some(v))
    }

    /// Numeric value of an attribute, whatever type it was stored as.
    pub fn number(&self, name: &str) -> Option<f64> {
        match self.attr(name)? {
            Value::Float(v) => Some(*v),
            Value::Int(v) => Some(*v as f64),
            Value::Bool(v) => Some(f64::from(u8::from(*v))),
            Value::Text(s) => s.trim().parse().ok(),
            Value::Blob => None,
        }
    }

    pub fn integer(&self, name: &str) -> Option<i64> {
        match self.attr(name)? {
            Value::Int(v) => Some(*v),
            Value::Bool(v) => Some(i64::from(*v)),
            Value::Float(v) if v.fract() == 0.0 => Some(*v as i64),
            Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn flag(&self, name: &str) -> Option<bool> {
        self.integer(name).map(|v| v != 0)
    }

    /// The last attribute of this name that was stored as a floating-point number.
    pub fn float(&self, name: &str) -> Option<f64> {
        self.attrs.iter().rev().find_map(|(n, v)| match v {
            Value::Float(f) if n == name => Some(*f),
            _ => None,
        })
    }
}

// Field types, in the order of Audacity's `FieldTypes` enum.
const FT_CHAR_SIZE: u8 = 0;
const FT_START_TAG: u8 = 1;
const FT_END_TAG: u8 = 2;
const FT_STRING: u8 = 3;
const FT_INT: u8 = 4;
const FT_BOOL: u8 = 5;
const FT_LONG: u8 = 6;
const FT_LONG_LONG: u8 = 7;
const FT_SIZE_T: u8 = 8;
const FT_FLOAT: u8 = 9;
const FT_DOUBLE: u8 = 10;
const FT_DATA: u8 = 11;
const FT_RAW: u8 = 12;
const FT_PUSH: u8 = 13;
const FT_POP: u8 = 14;
const FT_NAME: u8 = 15;
const FT_BLOB: u8 = 16;

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn done(&self) -> bool {
        self.pos >= self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], XmlError> {
        let end = self.pos.checked_add(n).ok_or(XmlError::Truncated)?;
        let bytes = self.buf.get(self.pos..end).ok_or(XmlError::Truncated)?;
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], XmlError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, XmlError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, XmlError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, XmlError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// A byte length, stored as a signed 32-bit integer.
    fn length(&mut self) -> Result<usize, XmlError> {
        usize::try_from(self.i32()?).map_err(|_| XmlError::Malformed("negative length"))
    }
}

/// Decoder state that carries over from the dictionary stream to the document.
struct Decoder {
    char_size: u8,
    names: HashMap<u16, String>,
    name_stack: Vec<HashMap<u16, String>>,
    open: Vec<Element>,
    root: Option<Element>,
}

impl Decoder {
    fn text(&self, bytes: &[u8]) -> Result<String, XmlError> {
        match self.char_size {
            1 => String::from_utf8(bytes.to_vec()).map_err(|_| XmlError::BadText),
            2 => {
                let units = bytes
                    .chunks_exact(2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]));
                char::decode_utf16(units)
                    .collect::<Result<String, _>>()
                    .map_err(|_| XmlError::BadText)
            }
            4 => bytes
                .chunks_exact(4)
                .map(|b| char::from_u32(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
                .collect::<Option<String>>()
                .ok_or(XmlError::BadText),
            other => Err(XmlError::CharSize(other)),
        }
    }

    fn name(&self, id: u16) -> Result<String, XmlError> {
        self.names
            .get(&id)
            .cloned()
            .ok_or(XmlError::UnknownName(id))
    }

    fn attr(&mut self, id: u16, value: Value) -> Result<(), XmlError> {
        let name = self.name(id)?;
        let element = self
            .open
            .last_mut()
            .ok_or(XmlError::Malformed("attribute outside an element"))?;
        element.attrs.push((name, value));
        Ok(())
    }

    fn feed(&mut self, buf: &[u8]) -> Result<(), XmlError> {
        let mut r = Reader { buf, pos: 0 };
        while !r.done() {
            match r.u8()? {
                FT_CHAR_SIZE => self.char_size = r.u8()?,
                FT_NAME => {
                    let id = r.u16()?;
                    let len = usize::from(r.u16()?);
                    let name = self.text(r.take(len)?)?;
                    self.names.insert(id, name);
                }
                FT_PUSH => self.name_stack.push(std::mem::take(&mut self.names)),
                FT_POP => {
                    self.names = self
                        .name_stack
                        .pop()
                        .ok_or(XmlError::Malformed("unbalanced name scope"))?;
                }
                FT_START_TAG => {
                    let name = self.name(r.u16()?)?;
                    self.open.push(Element {
                        name,
                        ..Element::default()
                    });
                }
                FT_END_TAG => {
                    let name = self.name(r.u16()?)?;
                    let element = self
                        .open
                        .pop()
                        .filter(|e| e.name == name)
                        .ok_or(XmlError::Malformed("mismatched end tag"))?;
                    match self.open.last_mut() {
                        Some(parent) => parent.children.push(element),
                        None if self.root.is_none() => self.root = Some(element),
                        None => return Err(XmlError::Malformed("more than one root element")),
                    }
                }
                FT_STRING => {
                    let id = r.u16()?;
                    let len = r.length()?;
                    let text = self.text(r.take(len)?)?;
                    self.attr(id, Value::Text(text))?;
                }
                FT_INT | FT_LONG => {
                    let id = r.u16()?;
                    let value = r.i32()?;
                    self.attr(id, Value::Int(i64::from(value)))?;
                }
                FT_SIZE_T => {
                    let id = r.u16()?;
                    let value = u32::from_le_bytes(r.array()?);
                    self.attr(id, Value::Int(i64::from(value)))?;
                }
                FT_LONG_LONG => {
                    let id = r.u16()?;
                    let value = i64::from_le_bytes(r.array()?);
                    self.attr(id, Value::Int(value))?;
                }
                FT_BOOL => {
                    let id = r.u16()?;
                    let value = r.u8()? != 0;
                    self.attr(id, Value::Bool(value))?;
                }
                FT_FLOAT => {
                    let id = r.u16()?;
                    let value = f32::from_le_bytes(r.array()?);
                    r.i32()?; // display digits
                    self.attr(id, Value::Float(f64::from(value)))?;
                }
                FT_DOUBLE => {
                    let id = r.u16()?;
                    let value = f64::from_le_bytes(r.array()?);
                    r.i32()?; // display digits
                    self.attr(id, Value::Float(value))?;
                }
                // Character data and raw pass-through text: nothing we read lives there.
                FT_DATA | FT_RAW => {
                    let len = r.length()?;
                    r.take(len)?;
                }
                FT_BLOB => {
                    let id = r.u16()?;
                    let len = r.length()?;
                    r.take(len)?;
                    self.attr(id, Value::Blob)?;
                }
                other => return Err(XmlError::UnknownField(other)),
            }
        }
        Ok(())
    }
}

/// Decode a project's dictionary and document streams into its root element.
pub fn decode(dict: &[u8], doc: &[u8]) -> Result<Element, XmlError> {
    let mut decoder = Decoder {
        // Overridden by the dictionary's first field; wide characters are the norm.
        char_size: 4,
        names: HashMap::new(),
        name_stack: Vec::new(),
        open: Vec::new(),
        root: None,
    };
    decoder.feed(dict)?;
    decoder.feed(doc)?;
    if !decoder.open.is_empty() {
        return Err(XmlError::Malformed("unclosed element"));
    }
    decoder.root.ok_or(XmlError::Malformed("no root element"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aup::fixture::XmlWriter;

    fn sample(char_size: u8) -> (Vec<u8>, Vec<u8>) {
        let mut w = XmlWriter::with_char_size(char_size);
        w.start("project");
        w.text("audacityversion", "4.0.1");
        w.double("rate", 96000.0);
        w.start("thumbnail");
        w.blob("data", &[1, 2, 3, 4, 5]);
        w.end("thumbnail");
        w.start("wavetrack");
        w.text("name", "Audio 1 – é");
        w.int("gain", 20);
        w.int("linked", 3);
        w.boolean("mute", false);
        w.double("gain", 0.5);
        w.long("sampleformat", 262_145);
        w.start("waveblock");
        w.long_long("start", 5_000_000_000);
        w.size_t("maxsamples", 262_144);
        w.float("pan", -0.25);
        w.end("waveblock");
        w.end("wavetrack");
        w.end("project");
        w.finish()
    }

    #[test]
    fn decodes_elements_attributes_and_nesting() {
        let (dict, doc) = sample(4);
        let root = decode(&dict, &doc).unwrap();
        assert_eq!(root.name, "project");
        assert_eq!(root.number("rate"), Some(96000.0));
        assert_eq!(root.children.len(), 2);

        let track = root.child("wavetrack").unwrap();
        assert_eq!(
            track.attrs[0],
            ("name".to_owned(), Value::Text("Audio 1 – é".to_owned()))
        );
        assert_eq!(track.integer("linked"), Some(3));
        assert_eq!(track.flag("mute"), Some(false));
        assert_eq!(track.integer("sampleformat"), Some(262_145));

        let block = track.child("waveblock").unwrap();
        assert_eq!(block.integer("start"), Some(5_000_000_000));
        assert_eq!(block.integer("maxsamples"), Some(262_144));
        assert_eq!(block.number("pan"), Some(-0.25));
    }

    #[test]
    fn duplicate_attribute_names_resolve_like_audacity() {
        let (dict, doc) = sample(4);
        let root = decode(&dict, &doc).unwrap();
        let track = root.child("wavetrack").unwrap();
        // The later, floating-point `gain` is the track volume.
        assert_eq!(track.number("gain"), Some(0.5));
        assert_eq!(track.float("gain"), Some(0.5));
    }

    #[test]
    fn blobs_are_skipped_without_losing_position() {
        let (dict, doc) = sample(4);
        let root = decode(&dict, &doc).unwrap();
        let thumbnail = root.child("thumbnail").unwrap();
        assert_eq!(thumbnail.attrs, [("data".to_owned(), Value::Blob)]);
        assert_eq!(thumbnail.number("data"), None);
    }

    #[test]
    fn every_character_width_decodes_the_same_tree() {
        let wide = decode(&sample(4).0, &sample(4).1).unwrap();
        for char_size in [1, 2] {
            let (dict, doc) = sample(char_size);
            assert_eq!(decode(&dict, &doc).unwrap(), wide, "char size {char_size}");
        }
    }

    #[test]
    fn truncated_documents_are_rejected() {
        let (dict, doc) = sample(4);
        for cut in [1, doc.len() / 2, doc.len() - 1] {
            assert!(decode(&dict, &doc[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn unknown_fields_and_names_are_rejected() {
        let (dict, _) = sample(4);
        assert_eq!(decode(&dict, &[99]), Err(XmlError::UnknownField(99)));
        // Start tag with an id the dictionary never defined.
        let doc = [FT_START_TAG, 0xFF, 0xFF];
        assert_eq!(decode(&dict, &doc), Err(XmlError::UnknownName(0xFFFF)));
    }

    #[test]
    fn structural_errors_are_rejected() {
        let mut w = XmlWriter::with_char_size(4);
        w.start("project");
        w.start("wavetrack");
        w.end("project"); // closes the wrong element
        let (dict, doc) = w.finish();
        assert_eq!(
            decode(&dict, &doc),
            Err(XmlError::Malformed("mismatched end tag"))
        );
        assert_eq!(
            decode(&[], &[]),
            Err(XmlError::Malformed("no root element"))
        );
    }
}
