use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use serde::Deserialize;

pub const PROTO_CONFIG_PHASE: i32 = 764;
pub const PROTO_NBT_TITLES: i32 = 765;
pub const PROTO_SHOULD_AUTH: i32 = 766;
pub const PROTO_VARINT_CONTAINER: i32 = 768;
pub const PROTO_CHUNK_ZX: i32 = 764;
pub const PROTO_VERIFY_TOKEN_FLAG_MAX: i32 = 760;

#[derive(Debug, Clone, Deserialize)]
pub struct Ids {
    pub versions: Vec<String>,
    pub play_cb: HashMap<String, i32>,
    pub play_sb: HashMap<String, i32>,
    #[serde(default)]
    pub config_cb: HashMap<String, i32>,
    #[serde(default)]
    pub config_sb: HashMap<String, i32>,
}

// missing entries are -1 so they never match a real packet id
#[derive(Debug, Clone)]
pub struct Protocol {
    pub version: i32,
    pub label: String,
    pub cb_open_screen: i32,
    pub cb_container_close: i32,
    pub cb_set_content: i32,
    pub cb_set_slot: i32,
    pub cb_set_data: i32,
    pub cb_start_configuration: i32,
    pub cb_disconnect: i32,
    pub cb_level_chunk: i32,
    pub cb_forget_chunk: i32,
    pub sb_container_click: i32,
    pub sb_container_close: i32,
    pub sb_button_click: i32,
    pub sb_configuration_ack: i32,
    pub cfg_cb_finish: i32,
    pub cfg_sb_finish: i32,
}

fn table() -> &'static HashMap<i32, Ids> {
    static TABLE: OnceLock<HashMap<i32, Ids>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let raw: HashMap<String, Ids> =
            serde_json::from_str(include_str!("../data/protocols.json")).expect("protocols.json");
        raw.into_iter()
            .map(|(k, v)| (k.parse().expect("protocol number"), v))
            .collect()
    })
}

pub fn supported_range() -> String {
    let mut keys: Vec<_> = table().keys().copied().collect();
    keys.sort();
    let first = &table()[&keys[0]].versions;
    let last = &table()[keys.last().unwrap()].versions;
    format!("{} .. {}", first.first().unwrap(), last.last().unwrap())
}

impl Protocol {
    pub fn get(version: i32) -> Option<Protocol> {
        let ids = table().get(&version)?;
        let g = |m: &HashMap<String, i32>, k: &str| m.get(k).copied().unwrap_or(-1);
        let mut versions = ids.versions.clone();
        versions.sort();
        Some(Protocol {
            version,
            label: versions.join("/"),
            cb_open_screen: g(&ids.play_cb, "open_screen"),
            cb_container_close: g(&ids.play_cb, "container_close"),
            cb_set_content: g(&ids.play_cb, "container_set_content"),
            cb_set_slot: g(&ids.play_cb, "container_set_slot"),
            cb_set_data: g(&ids.play_cb, "container_set_data"),
            cb_start_configuration: g(&ids.play_cb, "start_configuration"),
            cb_disconnect: g(&ids.play_cb, "disconnect"),
            cb_level_chunk: g(&ids.play_cb, "level_chunk_with_light"),
            cb_forget_chunk: g(&ids.play_cb, "forget_level_chunk"),
            sb_container_click: g(&ids.play_sb, "container_click"),
            sb_container_close: g(&ids.play_sb, "container_close"),
            sb_button_click: g(&ids.play_sb, "container_button_click"),
            sb_configuration_ack: g(&ids.play_sb, "configuration_acknowledged"),
            cfg_cb_finish: g(&ids.config_cb, "finish_configuration"),
            cfg_sb_finish: g(&ids.config_sb, "finish_configuration"),
        })
    }

    pub fn has_config_phase(&self) -> bool {
        self.version >= PROTO_CONFIG_PHASE
    }

    pub fn varint_container(&self) -> bool {
        self.version >= PROTO_VARINT_CONTAINER
    }

    pub fn read_container_id(&self, r: &mut Reader) -> Result<i32> {
        if self.varint_container() {
            r.varint()
        } else {
            Ok(r.u8()? as i32)
        }
    }

    pub fn write_container_id(&self, w: &mut Vec<u8>, id: i32) {
        if self.varint_container() {
            write_varint(w, id);
        } else {
            w.push(id as u8);
        }
    }
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).context("packet truncated")?;
        self.pos += 1;
        Ok(b)
    }

    pub fn i16(&mut self) -> Result<i16> {
        let hi = self.u8()?;
        let lo = self.u8()?;
        Ok(i16::from_be_bytes([hi, lo]))
    }

    pub fn i32(&mut self) -> Result<i32> {
        let b = self.bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u16(&mut self) -> Result<u16> {
        Ok(self.i16()? as u16)
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn varint(&mut self) -> Result<i32> {
        let mut value: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            value |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(value as i32);
            }
        }
        bail!("varint too long")
    }

    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.pos + n > self.buf.len() {
            bail!("packet truncated");
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn byte_array(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()?;
        if n < 0 {
            bail!("negative length");
        }
        self.bytes(n as usize)
    }

    pub fn string(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.byte_array()?).into_owned())
    }
}

pub fn write_varint(w: &mut Vec<u8>, value: i32) {
    let mut v = value as u32;
    loop {
        if v & !0x7f == 0 {
            w.push(v as u8);
            return;
        }
        w.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
}

pub fn write_string(w: &mut Vec<u8>, s: &str) {
    write_byte_array(w, s.as_bytes());
}

pub fn write_byte_array(w: &mut Vec<u8>, b: &[u8]) {
    write_varint(w, b.len() as i32);
    w.extend_from_slice(b);
}

pub fn decode_frame(body: &[u8], threshold: i32) -> Result<(i32, Vec<u8>)> {
    let data = if threshold >= 0 {
        let mut r = Reader::new(body);
        let data_len = r.varint()?;
        if data_len == 0 {
            r.remaining().to_vec()
        } else {
            if data_len > 8 * 1024 * 1024 {
                bail!("decompressed size too large");
            }
            let mut out = Vec::with_capacity(data_len as usize);
            ZlibDecoder::new(r.remaining()).read_to_end(&mut out)?;
            out
        }
    } else {
        body.to_vec()
    };
    let mut r = Reader::new(&data);
    let id = r.varint()?;
    let payload = r.remaining().to_vec();
    Ok((id, payload))
}

// length prefix included, not encrypted
pub fn encode_frame(id: i32, payload: &[u8], threshold: i32) -> Vec<u8> {
    let mut data = Vec::with_capacity(payload.len() + 5);
    write_varint(&mut data, id);
    data.extend_from_slice(payload);

    let body = if threshold >= 0 {
        let mut body = Vec::new();
        if data.len() as i32 >= threshold {
            write_varint(&mut body, data.len() as i32);
            let mut enc = ZlibEncoder::new(body, Compression::default());
            std::io::Write::write_all(&mut enc, &data).expect("zlib to vec");
            enc.finish().expect("zlib to vec")
        } else {
            write_varint(&mut body, 0);
            body.extend_from_slice(&data);
            body
        }
    } else {
        data
    };
    wrap_body(&body)
}

pub fn wrap_body(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    write_varint(&mut out, body.len() as i32);
    out.extend_from_slice(body);
    out
}

// best effort: a JSON string before 1.20.3, network NBT after
pub fn read_title(r: &mut Reader, proto: &Protocol) -> String {
    if proto.version < PROTO_NBT_TITLES {
        return r.string().unwrap_or_default();
    }
    let rest = r.remaining();
    match rest.first() {
        Some(8) if rest.len() >= 3 => {
            let n = u16::from_be_bytes([rest[1], rest[2]]) as usize;
            rest.get(3..3 + n)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .unwrap_or_default()
        }
        Some(10) => nbt_text_guess(rest),
        _ => String::new(),
    }
}

// pulls every "text" and "translate" string out of a compound without a real
// NBT parser, good enough to recognise a GUI by its title. a real parser is on my list,
// somewhere below "never"
fn nbt_text_guess(nbt: &[u8]) -> String {
    let mut parts = Vec::new();
    for key in [&b"text"[..], &b"translate"[..]] {
        let mut i = 0;
        while i + 3 + key.len() + 2 <= nbt.len() {
            if nbt[i] == 8
                && u16::from_be_bytes([nbt[i + 1], nbt[i + 2]]) as usize == key.len()
                && &nbt[i + 3..i + 3 + key.len()] == key
            {
                let p = i + 3 + key.len();
                let n = u16::from_be_bytes([nbt[p], nbt[p + 1]]) as usize;
                if let Some(s) = nbt.get(p + 2..p + 2 + n) {
                    let s = String::from_utf8_lossy(s);
                    if !s.is_empty() {
                        parts.push(s.into_owned());
                    }
                }
                i = p + 2 + n;
            } else {
                i += 1;
            }
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for v in [0, 1, 127, 128, 255, 25565, 2097151, i32::MAX, -1, i32::MIN] {
            let mut w = Vec::new();
            write_varint(&mut w, v);
            assert_eq!(Reader::new(&w).varint().unwrap(), v);
        }
    }

    #[test]
    fn frame_roundtrip() {
        let payload = vec![7u8; 400];
        for threshold in [-1, 0, 256, 1024] {
            let frame = encode_frame(0x12, &payload, threshold);
            let mut r = Reader::new(&frame);
            let len = r.varint().unwrap() as usize;
            let body = r.bytes(len).unwrap();
            let (id, p) = decode_frame(body, threshold).unwrap();
            assert_eq!(id, 0x12);
            assert_eq!(p, payload);
        }
    }

    #[test]
    fn every_supported_protocol_resolves() {
        for (&v, _) in table() {
            let p = Protocol::get(v).unwrap();
            assert!(p.cb_open_screen >= 0 && p.sb_container_click >= 0, "{v}");
            assert!(p.cb_level_chunk >= 0 && p.cb_forget_chunk >= 0, "{v}");
            if p.has_config_phase() {
                assert!(p.cfg_cb_finish >= 0 && p.sb_configuration_ack >= 0, "{v}");
            }
        }
        assert!(Protocol::get(760).is_some(), "1.19.2");
    }
}
