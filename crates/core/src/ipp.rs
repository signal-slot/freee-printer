//! Minimal IPP message codec (RFC 8010).

use anyhow::{Result, bail};

pub mod tag {
    pub const OPERATION: u8 = 0x01;
    pub const JOB: u8 = 0x02;
    pub const END: u8 = 0x03;
    pub const PRINTER: u8 = 0x04;

    pub const NO_VALUE: u8 = 0x13;
    pub const INTEGER: u8 = 0x21;
    pub const BOOLEAN: u8 = 0x22;
    pub const ENUM: u8 = 0x23;
    pub const OCTET_STRING: u8 = 0x30;
    pub const DATE_TIME: u8 = 0x31;
    pub const RESOLUTION: u8 = 0x32;
    pub const RANGE: u8 = 0x33;
    pub const BEG_COLLECTION: u8 = 0x34;
    pub const TEXT_LANG: u8 = 0x35;
    pub const NAME_LANG: u8 = 0x36;
    pub const END_COLLECTION: u8 = 0x37;
    pub const TEXT: u8 = 0x41;
    pub const NAME: u8 = 0x42;
    pub const KEYWORD: u8 = 0x44;
    pub const URI: u8 = 0x45;
    pub const CHARSET: u8 = 0x47;
    pub const LANGUAGE: u8 = 0x48;
    pub const MIME_TYPE: u8 = 0x49;
    pub const MEMBER_NAME: u8 = 0x4a;
}

pub mod op {
    pub const PRINT_JOB: u16 = 0x0002;
    pub const VALIDATE_JOB: u16 = 0x0004;
    pub const CREATE_JOB: u16 = 0x0005;
    pub const SEND_DOCUMENT: u16 = 0x0006;
    pub const CANCEL_JOB: u16 = 0x0008;
    pub const GET_JOB_ATTRIBUTES: u16 = 0x0009;
    pub const GET_JOBS: u16 = 0x000a;
    pub const GET_PRINTER_ATTRIBUTES: u16 = 0x000b;
    pub const CANCEL_MY_JOBS: u16 = 0x0039;
    pub const CLOSE_JOB: u16 = 0x003b;
    pub const IDENTIFY_PRINTER: u16 = 0x003c;
}

pub mod status {
    pub const OK: u16 = 0x0000;
    pub const BAD_REQUEST: u16 = 0x0400;
    pub const NOT_FOUND: u16 = 0x0406;
    pub const NOT_POSSIBLE: u16 = 0x0404;
    pub const DOCUMENT_FORMAT_NOT_SUPPORTED: u16 = 0x040a;
    pub const COMPRESSION_NOT_SUPPORTED: u16 = 0x040f;
    pub const OPERATION_NOT_SUPPORTED: u16 = 0x0501;
    pub const BUSY: u16 = 0x0507;
    pub const VERSION_NOT_SUPPORTED: u16 = 0x0503;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Integer(i32),
    Boolean(bool),
    Enum(i32),
    /// text, name, keyword, uri, charset, naturalLanguage, mimeMediaType, ...
    Str(u8, String),
    Resolution(i32, i32, u8),
    Range(i32, i32),
    DateTime([u8; 11]),
    Collection(Vec<Attr>),
    /// Out-of-band values and anything we do not interpret.
    Other(u8, Vec<u8>),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(_, s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i32> {
        match self {
            Value::Integer(i) | Value::Enum(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Boolean(b) => Some(*b),
            _ => None,
        }
    }
}

pub fn text(s: impl Into<String>) -> Value {
    Value::Str(tag::TEXT, s.into())
}
pub fn name(s: impl Into<String>) -> Value {
    Value::Str(tag::NAME, s.into())
}
pub fn keyword(s: impl Into<String>) -> Value {
    Value::Str(tag::KEYWORD, s.into())
}
pub fn uri(s: impl Into<String>) -> Value {
    Value::Str(tag::URI, s.into())
}
pub fn mime(s: impl Into<String>) -> Value {
    Value::Str(tag::MIME_TYPE, s.into())
}
pub fn keywords(list: &[&str]) -> Vec<Value> {
    list.iter().map(|s| keyword(*s)).collect()
}
pub fn no_value() -> Value {
    Value::Other(tag::NO_VALUE, Vec::new())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Attr {
    pub name: String,
    pub values: Vec<Value>,
}

impl Attr {
    pub fn new(name: &str, value: Value) -> Self {
        Attr {
            name: name.to_string(),
            values: vec![value],
        }
    }

    pub fn set(name: &str, values: Vec<Value>) -> Self {
        Attr {
            name: name.to_string(),
            values,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Group {
    pub tag: u8,
    pub attrs: Vec<Attr>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub version: (u8, u8),
    /// operation-id for requests, status-code for responses
    pub code: u16,
    pub request_id: u32,
    pub groups: Vec<Group>,
}

impl Message {
    pub fn response(request: &Message, status: u16) -> Self {
        Message {
            version: request.version,
            code: status,
            request_id: request.request_id,
            groups: vec![Group {
                tag: tag::OPERATION,
                attrs: vec![
                    Attr::new(
                        "attributes-charset",
                        Value::Str(tag::CHARSET, "utf-8".into()),
                    ),
                    Attr::new(
                        "attributes-natural-language",
                        Value::Str(tag::LANGUAGE, "ja".into()),
                    ),
                ],
            }],
        }
    }

    pub fn with_status_message(mut self, message: &str) -> Self {
        self.groups[0]
            .attrs
            .push(Attr::new("status-message", text(message)));
        self
    }

    pub fn push_group(&mut self, tag: u8, attrs: Vec<Attr>) {
        self.groups.push(Group { tag, attrs });
    }

    pub fn find(&self, group: u8, name: &str) -> Option<&Attr> {
        self.groups
            .iter()
            .filter(|g| g.tag == group)
            .flat_map(|g| g.attrs.iter())
            .find(|a| a.name == name)
    }

    pub fn op_str(&self, name: &str) -> Option<&str> {
        self.find(tag::OPERATION, name)?.values.first()?.as_str()
    }

    pub fn op_int(&self, name: &str) -> Option<i32> {
        self.find(tag::OPERATION, name)?.values.first()?.as_int()
    }

    /// Parses a message and returns it with the offset of the document data.
    pub fn parse(buf: &[u8]) -> Result<(Message, usize)> {
        let mut r = Reader { buf, pos: 0 };
        let version = (r.u8()?, r.u8()?);
        let code = r.u16()?;
        let request_id = r.u32()?;
        let mut groups: Vec<Group> = Vec::new();
        loop {
            let t = r.u8()?;
            if t == tag::END {
                break;
            }
            if t < 0x10 {
                groups.push(Group {
                    tag: t,
                    attrs: Vec::new(),
                });
                continue;
            }
            let Some(group) = groups.last_mut() else {
                bail!("attribute before any group tag");
            };
            let attr_name = r.string()?;
            let value = r.value(t)?;
            if attr_name.is_empty() {
                match group.attrs.last_mut() {
                    Some(attr) => attr.values.push(value),
                    None => bail!("additional value without attribute"),
                }
            } else {
                group.attrs.push(Attr {
                    name: attr_name,
                    values: vec![value],
                });
            }
        }
        Ok((
            Message {
                version,
                code,
                request_id,
                groups,
            },
            r.pos,
        ))
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1024);
        out.push(self.version.0);
        out.push(self.version.1);
        out.extend_from_slice(&self.code.to_be_bytes());
        out.extend_from_slice(&self.request_id.to_be_bytes());
        for group in &self.groups {
            out.push(group.tag);
            for attr in &group.attrs {
                encode_attr(&mut out, attr);
            }
        }
        out.push(tag::END);
        out
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.buf.len() - self.pos < n {
            bail!("truncated IPP message");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into()?))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.u16()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.bytes()?).into_owned())
    }

    /// Reads the value that follows an attribute name.
    fn value(&mut self, t: u8) -> Result<Value> {
        let data = self.bytes()?;
        if t == tag::BEG_COLLECTION {
            return self.collection();
        }
        decode_value(t, data)
    }

    fn collection(&mut self) -> Result<Value> {
        let mut members: Vec<Attr> = Vec::new();
        loop {
            let t = self.u8()?;
            let name_len = self.u16()?;
            if name_len != 0 {
                bail!("unexpected name inside collection");
            }
            match t {
                tag::END_COLLECTION => {
                    self.bytes()?;
                    break;
                }
                tag::MEMBER_NAME => {
                    let member = self.string()?;
                    members.push(Attr {
                        name: member,
                        values: Vec::new(),
                    });
                }
                _ => {
                    let value = self.value(t)?;
                    match members.last_mut() {
                        Some(attr) => attr.values.push(value),
                        None => bail!("collection value without member name"),
                    }
                }
            }
        }
        Ok(Value::Collection(members))
    }
}

fn be_i32(data: &[u8]) -> i32 {
    i32::from_be_bytes([data[0], data[1], data[2], data[3]])
}

fn decode_value(t: u8, data: &[u8]) -> Result<Value> {
    Ok(match t {
        tag::INTEGER if data.len() == 4 => Value::Integer(be_i32(data)),
        tag::ENUM if data.len() == 4 => Value::Enum(be_i32(data)),
        tag::BOOLEAN if data.len() == 1 => Value::Boolean(data[0] != 0),
        tag::RESOLUTION if data.len() == 9 => {
            Value::Resolution(be_i32(data), be_i32(&data[4..]), data[8])
        }
        tag::RANGE if data.len() == 8 => Value::Range(be_i32(data), be_i32(&data[4..])),
        tag::DATE_TIME if data.len() == 11 => Value::DateTime(data.try_into()?),
        tag::TEXT_LANG | tag::NAME_LANG => {
            // language length, language, text length, text
            let mut r = Reader { buf: data, pos: 0 };
            r.bytes()?;
            let s = r.string()?;
            Value::Str(
                if t == tag::TEXT_LANG {
                    tag::TEXT
                } else {
                    tag::NAME
                },
                s,
            )
        }
        tag::OCTET_STRING => Value::Other(t, data.to_vec()),
        0x40..=0x5f => Value::Str(t, String::from_utf8_lossy(data).into_owned()),
        _ => Value::Other(t, data.to_vec()),
    })
}

fn put_bytes(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    out.extend_from_slice(data);
}

fn encode_attr(out: &mut Vec<u8>, attr: &Attr) {
    for (i, value) in attr.values.iter().enumerate() {
        encode_value(out, if i == 0 { &attr.name } else { "" }, value);
    }
}

fn encode_value(out: &mut Vec<u8>, attr_name: &str, value: &Value) {
    let t = match value {
        Value::Integer(_) => tag::INTEGER,
        Value::Boolean(_) => tag::BOOLEAN,
        Value::Enum(_) => tag::ENUM,
        Value::Str(t, _) | Value::Other(t, _) => *t,
        Value::Resolution(..) => tag::RESOLUTION,
        Value::Range(..) => tag::RANGE,
        Value::DateTime(_) => tag::DATE_TIME,
        Value::Collection(_) => tag::BEG_COLLECTION,
    };
    out.push(t);
    put_bytes(out, attr_name.as_bytes());
    match value {
        Value::Integer(i) | Value::Enum(i) => put_bytes(out, &i.to_be_bytes()),
        Value::Boolean(b) => put_bytes(out, &[*b as u8]),
        Value::Str(_, s) => {
            // IPP values are limited to 1023 octets for text; never split a UTF-8 sequence.
            let mut end = s.len().min(1023);
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            put_bytes(out, &s.as_bytes()[..end]);
        }
        Value::Other(_, data) => put_bytes(out, data),
        Value::Resolution(x, y, units) => {
            let mut data = Vec::with_capacity(9);
            data.extend_from_slice(&x.to_be_bytes());
            data.extend_from_slice(&y.to_be_bytes());
            data.push(*units);
            put_bytes(out, &data);
        }
        Value::Range(lo, hi) => {
            let mut data = Vec::with_capacity(8);
            data.extend_from_slice(&lo.to_be_bytes());
            data.extend_from_slice(&hi.to_be_bytes());
            put_bytes(out, &data);
        }
        Value::DateTime(data) => put_bytes(out, data),
        Value::Collection(members) => {
            put_bytes(out, &[]);
            for member in members {
                out.push(tag::MEMBER_NAME);
                put_bytes(out, &[]);
                put_bytes(out, member.name.as_bytes());
                for v in &member.values {
                    encode_value(out, "", v);
                }
            }
            out.push(tag::END_COLLECTION);
            put_bytes(out, &[]);
            put_bytes(out, &[]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut msg = Message {
            version: (2, 0),
            code: op::PRINT_JOB,
            request_id: 42,
            groups: Vec::new(),
        };
        msg.push_group(
            tag::OPERATION,
            vec![
                Attr::new(
                    "attributes-charset",
                    Value::Str(tag::CHARSET, "utf-8".into()),
                ),
                Attr::set("requested-attributes", keywords(&["job-id", "job-state"])),
                Attr::new("job-name", name("請求書")),
            ],
        );
        msg.push_group(
            tag::JOB,
            vec![
                Attr::new("copies", Value::Integer(2)),
                Attr::new(
                    "media-col",
                    Value::Collection(vec![
                        Attr::new(
                            "media-size",
                            Value::Collection(vec![
                                Attr::new("x-dimension", Value::Integer(21000)),
                                Attr::new("y-dimension", Value::Integer(29700)),
                            ]),
                        ),
                        Attr::new("media-type", keyword("stationery")),
                    ]),
                ),
                Attr::new("printer-resolution", Value::Resolution(300, 300, 3)),
            ],
        );
        let mut bytes = msg.encode();
        let header_len = bytes.len();
        bytes.extend_from_slice(b"%PDF-1.7");
        let (parsed, offset) = Message::parse(&bytes).unwrap();
        assert_eq!(offset, header_len);
        assert_eq!(parsed.code, op::PRINT_JOB);
        assert_eq!(parsed.request_id, 42);
        assert_eq!(parsed.op_str("job-name"), Some("請求書"));
        assert_eq!(
            parsed
                .find(tag::OPERATION, "requested-attributes")
                .unwrap()
                .values
                .len(),
            2
        );
        assert_eq!(parsed.groups[1].attrs, msg.groups[1].attrs);
    }

    #[test]
    fn truncated_message_is_an_error() {
        assert!(Message::parse(&[2, 0, 0, 2, 0, 0]).is_err());
        assert!(Message::parse(&[2, 0, 0, 2, 0, 0, 0, 1, 1, 0x44, 0, 9]).is_err());
    }
}
