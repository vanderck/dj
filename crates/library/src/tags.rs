//! Tag reading focused on what Mixed In Key writes into files:
//!
//! * BPM (`TBPM`, `BPM`, `tmpo`),
//! * key (`TKEY`, `INITIALKEY`, `----:com.apple.iTunes:initialkey`), Camelot or musical,
//! * energy (`TXXX:EnergyLevel` or "8A - Energy 6" style comments),
//! * cue points and loops via Serato Markers2 (`GEOB` in ID3; base64 comment
//!   in FLAC/Ogg; freeform atom in MP4), and the Serato BeatGrid if present,
//! * ReplayGain track gain.
//!
//! Supports ID3v2.3/2.4 (MP3, AIFF, WAV), FLAC and Ogg Vorbis/Opus comments,
//! and MP4/M4A atoms. No detection is performed here; tags only.

use std::io::Read;
use std::path::Path;

use dj_engine::track::{BeatGrid, Cue, CueKind, TempoMap};
use serde::{Deserialize, Serialize};

use crate::key::Key;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub label: Option<String>,
    pub comment: Option<String>,
    pub bpm: Option<f64>,
    pub key_text: Option<String>,
    pub key: Option<Key>,
    pub energy: Option<u8>,
    pub cues: Vec<Cue>,
    /// Serato hot-cue index for each cue (0-based), aligned with `cues`.
    pub cue_slots: Vec<u8>,
    pub grid: Option<BeatGrid>,
    /// A Serato grid with several markers: the tempo changes through the track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tempo_map: Option<TempoMap>,
    pub replaygain_db: Option<f32>,
}

impl Tags {
    fn finish(&mut self) {
        if self.key.is_none() {
            if let Some(k) = self.key_text.as_deref().and_then(Key::parse) {
                self.key = Some(k);
            } else if let Some(k) = self.comment.as_deref().and_then(Key::parse) {
                // MIK can write "8A - Energy 6" into the comment instead.
                self.key = Some(k);
            }
        }
        if self.energy.is_none() {
            if let Some(c) = &self.comment {
                if let Some(i) = c.to_ascii_lowercase().find("energy") {
                    let n: String = c[i + 6..].trim().chars().take_while(|c| c.is_ascii_digit()).collect();
                    self.energy = n.parse().ok().filter(|e| (1..=10).contains(e));
                }
            }
        }
        if let Some(b) = self.bpm {
            if !(20.0..=999.0).contains(&b) {
                self.bpm = None;
            }
        }
    }

    fn set_text(&mut self, field: &str, v: String) {
        let v = v.trim().trim_end_matches('\0').to_string();
        if v.is_empty() {
            return;
        }
        match field.to_ascii_uppercase().as_str() {
            "TITLE" => self.title = Some(v),
            "ARTIST" => self.artist = Some(v),
            "ALBUM" => self.album = Some(v),
            "GENRE" => self.genre = Some(v),
            "LABEL" | "ORGANIZATION" | "PUBLISHER" => self.label = Some(v),
            "COMMENT" | "DESCRIPTION" => self.comment.get_or_insert(v).truncate(4096),
            "BPM" | "TEMPO" => self.bpm = self.bpm.or_else(|| v.replace(',', ".").parse().ok()),
            "INITIALKEY" | "KEY" | "INITIAL KEY" => self.key_text = Some(v),
            "ENERGYLEVEL" | "ENERGY" => self.energy = v.trim().parse().ok(),
            "REPLAYGAIN_TRACK_GAIN" => {
                self.replaygain_db = v.trim_end_matches("dB").trim_end_matches("db").trim().parse().ok()
            }
            _ => {}
        }
    }
}

pub fn read(path: &Path) -> std::io::Result<Tags> {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let file_len = f.metadata()?.len();
    let mut head = [0u8; 12];
    let n = f.read(&mut head)?;
    f.seek(SeekFrom::Start(0))?;
    let read_n = |f: &mut std::fs::File, n: u64| -> std::io::Result<Vec<u8>> {
        let mut v = Vec::new();
        f.take(n).read_to_end(&mut v)?;
        Ok(v)
    };
    let mut tags = Tags::default();
    if n >= 10 && &head[..3] == b"ID3" {
        // Read only the tag itself, not the audio.
        let size = syncsafe(&head[6..10]) as u64 + 10;
        let data = read_n(&mut f, size)?;
        parse_id3(&data, &mut tags);
    } else if n >= 4 && &head[..4] == b"fLaC" {
        // Metadata blocks precede the audio; 16 MB covers even huge artwork.
        let data = read_n(&mut f, 16 << 20)?;
        parse_flac(&data, &mut tags);
    } else if n >= 4 && &head[..4] == b"OggS" {
        let data = read_n(&mut f, 1 << 20)?;
        parse_ogg(&data, &mut tags);
    } else if n >= 12 && (&head[..4] == b"RIFF" || &head[..4] == b"FORM") {
        // Walk chunks with seeks; only the ID3 chunk is read.
        let riff = &head[..4] == b"RIFF";
        let mut pos = 12u64;
        let mut hdr = [0u8; 8];
        while pos + 8 <= file_len {
            f.seek(SeekFrom::Start(pos))?;
            f.read_exact(&mut hdr)?;
            let len = if riff { le32(&hdr[4..]) } else { be32(&hdr[4..]) } as u64;
            if hdr[..4].eq_ignore_ascii_case(b"id3 ") {
                let data = read_n(&mut f, len)?;
                parse_id3(&data, &mut tags);
                break;
            }
            pos += 8 + len + (len & 1);
        }
    } else if n >= 8 && &head[4..8] == b"ftyp" {
        // Top-level atoms; read only `moov`.
        let mut pos = 0u64;
        let mut hdr = [0u8; 16];
        while pos + 8 <= file_len {
            f.seek(SeekFrom::Start(pos))?;
            f.read_exact(&mut hdr[..8])?;
            let mut size = be32(&hdr[..4]) as u64;
            if size == 1 {
                f.read_exact(&mut hdr[8..16])?;
                size = u64::from_be_bytes(hdr[8..16].try_into().unwrap());
            } else if size == 0 {
                size = file_len - pos;
            }
            if size < 8 {
                break;
            }
            if &hdr[4..8] == b"moov" {
                f.seek(SeekFrom::Start(pos))?;
                let data = read_n(&mut f, size)?;
                parse_mp4(&data, &mut tags);
                break;
            }
            pos += size;
        }
    }
    tags.finish();
    Ok(tags)
}

// ------------------------------------------------------------ helpers

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// A Serato position (milliseconds) in seconds. Taggers write a cue just before the first audio
/// frame as a small negative number (−55 ms is 0xFFFFFFC9): read it signed, from the start.
fn serato_ms(b: &[u8]) -> f64 {
    (be32(b) as i32).max(0) as f64 / 1000.0
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn syncsafe(b: &[u8]) -> u32 {
    ((b[0] as u32 & 0x7f) << 21) | ((b[1] as u32 & 0x7f) << 14) | ((b[2] as u32 & 0x7f) << 7) | (b[3] as u32 & 0x7f)
}

fn unsync(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        out.push(data[i]);
        if data[i] == 0xff && i + 1 < data.len() && data[i + 1] == 0x00 {
            i += 1;
        }
        i += 1;
    }
    out
}

/// Lenient base64 (Serato omits padding and inserts newlines).
pub fn base64_decode(s: &[u8]) -> Vec<u8> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for &c in s {
        if c == b'=' {
            break;
        }
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

fn latin1(b: &[u8]) -> String {
    b.iter().map(|&c| c as char).collect()
}

/// Decode an ID3 text payload with its encoding byte.
fn id3_text(enc: u8, b: &[u8]) -> String {
    match enc {
        0 => latin1(b.split(|&c| c == 0).next().unwrap_or(b)),
        3 => String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(b)).into_owned(),
        1 | 2 => {
            let mut be = enc == 2;
            let mut b = b;
            if b.len() >= 2 && (b[..2] == [0xff, 0xfe] || b[..2] == [0xfe, 0xff]) {
                be = b[0] == 0xfe;
                b = &b[2..];
            }
            let units: Vec<u16> = b
                .chunks_exact(2)
                .map(|c| if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })
                .take_while(|&u| u != 0)
                .collect();
            String::from_utf16_lossy(&units)
        }
        _ => String::new(),
    }
}

/// Split off a null-terminated string in the given encoding; returns (string, rest).
fn id3_split(enc: u8, b: &[u8]) -> (String, &[u8]) {
    if enc == 1 || enc == 2 {
        let mut i = 0;
        while i + 1 < b.len() {
            if b[i] == 0 && b[i + 1] == 0 {
                return (id3_text(enc, &b[..i]), &b[i + 2..]);
            }
            i += 2;
        }
        (id3_text(enc, b), &[])
    } else {
        match b.iter().position(|&c| c == 0) {
            Some(i) => (id3_text(enc, &b[..i]), &b[i + 1..]),
            None => (id3_text(enc, b), &[]),
        }
    }
}

// ------------------------------------------------------------ ID3v2

pub fn parse_id3(data: &[u8], tags: &mut Tags) {
    if data.len() < 10 || &data[..3] != b"ID3" {
        return;
    }
    let major = data[3];
    let flags = data[5];
    let size = syncsafe(&data[6..10]) as usize;
    let end = (10 + size).min(data.len());
    let mut body = data[10..end].to_vec();
    if major < 3 {
        return; // ID3v2.2 is long obsolete; MIK writes 2.3/2.4.
    }
    if major == 3 && flags & 0x80 != 0 {
        body = unsync(&body);
    }
    let mut pos = 0;
    if flags & 0x40 != 0 && body.len() >= 4 {
        let ext = if major == 4 { syncsafe(&body[..4]) as usize } else { be32(&body[..4]) as usize + 4 };
        pos = ext.min(body.len());
    }
    while pos + 10 <= body.len() {
        let id = &body[pos..pos + 4];
        if id[0] == 0 {
            break;
        }
        let fsize = if major == 4 { syncsafe(&body[pos + 4..pos + 8]) } else { be32(&body[pos + 4..pos + 8]) } as usize;
        let fflags = u16::from_be_bytes([body[pos + 8], body[pos + 9]]);
        pos += 10;
        if pos + fsize > body.len() {
            break;
        }
        let mut f = body[pos..pos + fsize].to_vec();
        pos += fsize;
        if major == 4 {
            if fflags & 0x000c != 0 {
                continue; // compressed / encrypted
            }
            if fflags & 0x0001 != 0 && f.len() >= 4 {
                f.drain(..4);
            }
            if fflags & 0x0002 != 0 {
                f = unsync(&f);
            }
        } else {
            if fflags & 0x00c0 != 0 {
                continue;
            }
            if fflags & 0x0020 != 0 && !f.is_empty() {
                f.remove(0);
            }
        }
        id3_frame(id, &f, tags);
    }
}

fn id3_frame(id: &[u8], f: &[u8], tags: &mut Tags) {
    if f.is_empty() {
        return;
    }
    let enc = f[0];
    match id {
        b"TIT2" => tags.set_text("TITLE", id3_text(enc, &f[1..])),
        b"TPE1" => tags.set_text("ARTIST", id3_text(enc, &f[1..])),
        b"TALB" => tags.set_text("ALBUM", id3_text(enc, &f[1..])),
        b"TCON" => tags.set_text("GENRE", id3_text(enc, &f[1..])),
        b"TPUB" => tags.set_text("LABEL", id3_text(enc, &f[1..])),
        b"TBPM" => tags.set_text("BPM", id3_text(enc, &f[1..])),
        b"TKEY" => tags.set_text("INITIALKEY", id3_text(enc, &f[1..])),
        b"TXXX" => {
            let (desc, rest) = id3_split(enc, &f[1..]);
            tags.set_text(&desc, id3_text(enc, rest));
        }
        b"COMM" if f.len() > 4 => {
            let (desc, rest) = id3_split(enc, &f[4..]);
            if desc.is_empty() || tags.comment.is_none() {
                tags.comment = None;
                tags.set_text("COMMENT", id3_text(enc, rest));
            }
        }
        b"GEOB" => {
            let rest = &f[1..];
            let mime_end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
            let rest = &rest[(mime_end + 1).min(rest.len())..];
            let (_file, rest) = id3_split(enc, rest);
            let (desc, payload) = id3_split(enc, rest);
            serato_object(&desc, payload, tags);
        }
        _ => {}
    }
}

// ------------------------------------------------------------ Serato

fn serato_object(desc: &str, payload: &[u8], tags: &mut Tags) {
    match desc {
        "Serato Markers2" => serato_markers2(payload, tags),
        "Serato BeatGrid" => {
            if let Some(g) = serato_beatgrid(payload) {
                tags.tempo_map = serato_tempo_map(payload);
                // With several markers, the steady grid is the overall one.
                tags.grid = Some(tags.tempo_map.as_ref().map_or(g, |m| m.average()));
            }
        }
        _ => {}
    }
}

/// FLAC/Ogg/MP4 store Serato objects base64-encoded with a
/// "application/octet-stream\0\0<description>\0" prefix.
fn serato_wrapped(b64: &[u8], tags: &mut Tags) {
    let raw = base64_decode(b64);
    let prefix = b"application/octet-stream\0";
    if raw.starts_with(prefix) {
        let rest = &raw[prefix.len()..];
        let rest = &rest[rest.iter().position(|&c| c == 0).map(|i| i + 1).unwrap_or(0)..]; // filename
        let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
        let desc = latin1(&rest[..end]);
        serato_object(&desc, &rest[(end + 1).min(rest.len())..], tags);
    }
}

fn parse_serato_rgb(b: &[u8]) -> [u8; 3] {
    [b[0], b[1], b[2]]
}

pub fn serato_markers2(payload: &[u8], tags: &mut Tags) {
    // Payload: 0x01 0x01, then base64 text.
    if payload.len() < 2 {
        return;
    }
    let text = &payload[2..];
    let text_end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
    let raw = base64_decode(&text[..text_end]);
    if raw.len() < 2 || raw[0] != 1 || raw[1] != 1 {
        return;
    }
    let mut p = 2;
    let mut cues: Vec<(u8, Cue)> = Vec::new();
    while p < raw.len() {
        let Some(nl) = raw[p..].iter().position(|&c| c == 0) else { break };
        let name = latin1(&raw[p..p + nl]);
        p += nl + 1;
        if name.is_empty() || p + 4 > raw.len() {
            break;
        }
        let len = be32(&raw[p..p + 4]) as usize;
        p += 4;
        if p + len > raw.len() {
            break;
        }
        let d = &raw[p..p + len];
        p += len;
        match name.as_str() {
            "CUE" if d.len() >= 13 => {
                let idx = d[1];
                let ms = serato_ms(&d[2..6]);
                let color = parse_serato_rgb(&d[7..10]);
                let label_end = d[12..].iter().position(|&c| c == 0).unwrap_or(d.len() - 12);
                let label = String::from_utf8_lossy(&d[12..12 + label_end]).into_owned();
                cues.push((idx, Cue { kind: CueKind::Hot, secs: ms, end_secs: None, name: label, color }));
            }
            "LOOP" if d.len() >= 20 => {
                let idx = d[1];
                let s = serato_ms(&d[2..6]);
                let e = serato_ms(&d[6..10]);
                let color = parse_serato_rgb(&d[15..18]);
                // Layout: 0, idx, start, end, 0xFFFFFFFF, 0x00 RGB, locked, name\0
                let label_end = d[19..].iter().position(|&c| c == 0).unwrap_or(d.len() - 19);
                let label = String::from_utf8_lossy(&d[19..19 + label_end]).into_owned();
                // Loops occupy their own bank in Serato; place after hot cues.
                cues.push((
                    100 + idx,
                    Cue { kind: CueKind::Loop, secs: s, end_secs: Some(e), name: label, color },
                ));
            }
            _ => {}
        }
    }
    cues.sort_by_key(|(i, _)| *i);
    for (i, c) in cues {
        tags.cue_slots.push(i);
        tags.cues.push(c);
    }
}

/// A Serato BeatGrid with several markers (each: its position and the beats to the next one; the
/// last: its BPM) as a tempo map through them; `None` for a single marker (a steady grid).
pub fn serato_tempo_map(payload: &[u8]) -> Option<TempoMap> {
    if payload.len() < 6 || payload[0] != 1 || payload[1] != 0 {
        return None;
    }
    let n = be32(&payload[2..6]) as usize;
    if n < 2 || payload.len() < 6 + n * 8 {
        return None;
    }
    let f = |at: usize| f32::from_be_bytes(payload[at..at + 4].try_into().unwrap()) as f64;
    let mut points = Vec::with_capacity(n + 1);
    let mut beat = 0.0;
    for i in 0..n {
        let at = 6 + i * 8;
        points.push((beat, f(at)));
        if i + 1 < n {
            beat += be32(&payload[at + 4..at + 8]) as f64;
        } else {
            // The last marker's tempo carries on.
            let bpm = f(at + 4);
            if !(bpm > 20.0 && bpm < 999.0) {
                return None;
            }
            points.push((beat + 64.0, f(at) + 64.0 * 60.0 / bpm));
        }
    }
    TempoMap::new(points)
}

pub fn serato_beatgrid(payload: &[u8]) -> Option<BeatGrid> {
    if payload.len() < 6 || payload[0] != 1 || payload[1] != 0 {
        return None;
    }
    let n = be32(&payload[2..6]) as usize;
    if n == 0 || payload.len() < 6 + n * 8 {
        return None;
    }
    let first = f32::from_be_bytes(payload[6..10].try_into().ok()?) as f64;
    let last = 6 + (n - 1) * 8;
    let bpm = f32::from_be_bytes(payload[last + 4..last + 8].try_into().ok()?) as f64;
    (bpm > 20.0 && bpm < 999.0 && first.is_finite()).then_some(BeatGrid { bpm, anchor_secs: first })
}

// ------------------------------------------------------------ Vorbis comments (FLAC / Ogg)

fn vorbis_comments(b: &[u8], tags: &mut Tags) {
    if b.len() < 8 {
        return;
    }
    let vlen = le32(&b[..4]) as usize;
    let mut p = 4 + vlen;
    if p + 4 > b.len() {
        return;
    }
    let count = le32(&b[p..p + 4]) as usize;
    p += 4;
    for _ in 0..count {
        if p + 4 > b.len() {
            break;
        }
        let l = le32(&b[p..p + 4]) as usize;
        p += 4;
        if p + l > b.len() {
            break;
        }
        let s = &b[p..p + l];
        p += l;
        let Some(eq) = s.iter().position(|&c| c == b'=') else { continue };
        let k = String::from_utf8_lossy(&s[..eq]).to_ascii_uppercase();
        let v = &s[eq + 1..];
        match k.as_str() {
            "SERATO_MARKERS_V2" | "SERATO_BEATGRID" => serato_wrapped(v, tags),
            _ => tags.set_text(&k, String::from_utf8_lossy(v).into_owned()),
        }
    }
}

pub fn parse_flac(data: &[u8], tags: &mut Tags) {
    let mut p = 4;
    while p + 4 <= data.len() {
        let h = data[p];
        let len = ((data[p + 1] as usize) << 16) | ((data[p + 2] as usize) << 8) | data[p + 3] as usize;
        p += 4;
        if p + len > data.len() {
            break;
        }
        if h & 0x7f == 4 {
            vorbis_comments(&data[p..p + len], tags);
        }
        p += len;
        if h & 0x80 != 0 {
            break;
        }
    }
}

pub fn parse_ogg(data: &[u8], tags: &mut Tags) {
    // Reassemble the first few packets from the first pages.
    let mut packets: Vec<Vec<u8>> = vec![Vec::new()];
    let mut p = 0;
    while p + 27 <= data.len() && packets.len() <= 3 {
        if &data[p..p + 4] != b"OggS" {
            break;
        }
        let nseg = data[p + 26] as usize;
        if p + 27 + nseg > data.len() {
            break;
        }
        let lacing = &data[p + 27..p + 27 + nseg];
        let mut q = p + 27 + nseg;
        for &l in lacing {
            let l = l as usize;
            if q + l > data.len() {
                break;
            }
            packets.last_mut().unwrap().extend_from_slice(&data[q..q + l]);
            q += l;
            if l < 255 {
                packets.push(Vec::new());
            }
        }
        p = q;
    }
    for pk in packets.iter().take(3) {
        if pk.starts_with(b"\x03vorbis") {
            vorbis_comments(&pk[7..], tags);
        } else if pk.starts_with(b"OpusTags") {
            vorbis_comments(&pk[8..], tags);
        }
    }
}

// ------------------------------------------------------------ MP4

fn mp4_atoms(data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    let mut p = 0;
    std::iter::from_fn(move || {
        if p + 8 > data.len() {
            return None;
        }
        let mut size = be32(&data[p..p + 4]) as usize;
        let kind = &data[p + 4..p + 8];
        let mut hdr = 8;
        if size == 1 && p + 16 <= data.len() {
            size = u64::from_be_bytes(data[p + 8..p + 16].try_into().unwrap()) as usize;
            hdr = 16;
        } else if size == 0 {
            size = data.len() - p;
        }
        if size < hdr || p + size > data.len() {
            return None;
        }
        let body = &data[p + hdr..p + size];
        p += size;
        Some((kind, body))
    })
}

fn mp4_find<'a>(data: &'a [u8], path: &[&[u8]]) -> Option<&'a [u8]> {
    let (first, rest) = path.split_first()?;
    let (_, body) = mp4_atoms(data).find(|(k, _)| k == first)?;
    if rest.is_empty() {
        Some(body)
    } else if *first == b"meta" {
        mp4_find(body.get(4..)?, rest)
    } else {
        mp4_find(body, rest)
    }
}

fn mp4_data(item: &[u8]) -> Option<(u32, &[u8])> {
    let (_, d) = mp4_atoms(item).find(|(k, _)| *k == b"data")?;
    if d.len() < 8 {
        return None;
    }
    Some((be32(&d[..4]) & 0xffffff, &d[8..]))
}

pub fn parse_mp4(data: &[u8], tags: &mut Tags) {
    let Some(ilst) = mp4_find(data, &[b"moov", b"udta", b"meta", b"ilst"]) else { return };
    for (kind, item) in mp4_atoms(ilst) {
        let text = || mp4_data(item).map(|(_, v)| String::from_utf8_lossy(v).into_owned());
        match kind {
            b"\xa9nam" => tags.set_text("TITLE", text().unwrap_or_default()),
            b"\xa9ART" => tags.set_text("ARTIST", text().unwrap_or_default()),
            b"\xa9alb" => tags.set_text("ALBUM", text().unwrap_or_default()),
            b"\xa9gen" => tags.set_text("GENRE", text().unwrap_or_default()),
            b"\xa9cmt" => tags.set_text("COMMENT", text().unwrap_or_default()),
            b"tmpo" => {
                if let Some((_, v)) = mp4_data(item) {
                    if v.len() >= 2 {
                        tags.bpm = tags.bpm.or(Some(u16::from_be_bytes([v[0], v[1]]) as f64));
                    }
                }
            }
            b"----" => {
                let name = mp4_atoms(item)
                    .find(|(k, _)| *k == b"name")
                    .map(|(_, b)| String::from_utf8_lossy(b.get(4..).unwrap_or(&[])).into_owned())
                    .unwrap_or_default();
                let Some((_, v)) = mp4_data(item) else { continue };
                match name.to_ascii_lowercase().as_str() {
                    "markersv2" | "beatgrid" => serato_wrapped(v, tags),
                    n => tags.set_text(n, String::from_utf8_lossy(v).into_owned()),
                }
            }
            _ => {}
        }
    }
    // Freeform BPM written as text overrides the integer tmpo atom.
}

// ------------------------------------------------------------ tests

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn base64_encode(b: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..=c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        s
    }

    /// Build a Serato Markers2 GEOB payload with the given cues (ms).
    pub fn markers2(cues: &[(u8, u32, &str)], loops: &[(u8, u32, u32)]) -> Vec<u8> {
        let mut raw = vec![1u8, 1];
        for &(idx, ms, name) in cues {
            let mut d = vec![0, idx];
            d.extend_from_slice(&ms.to_be_bytes());
            d.extend_from_slice(&[0, 0xcc, 0x00, 0x00, 0, 0]);
            d.extend_from_slice(name.as_bytes());
            d.push(0);
            raw.extend_from_slice(b"CUE\0");
            raw.extend_from_slice(&(d.len() as u32).to_be_bytes());
            raw.extend_from_slice(&d);
        }
        for &(idx, s, e) in loops {
            let mut d = vec![0, idx];
            d.extend_from_slice(&s.to_be_bytes());
            d.extend_from_slice(&e.to_be_bytes());
            d.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0, 0x27, 0xaa, 0xe1, 0]);
            d.push(0);
            raw.extend_from_slice(b"LOOP\0");
            raw.extend_from_slice(&(d.len() as u32).to_be_bytes());
            raw.extend_from_slice(&d);
        }
        raw.push(0);
        let mut payload = vec![1u8, 1];
        let b64 = base64_encode(&raw);
        // Serato wraps lines at 72 chars and strips padding.
        for (i, ch) in b64.trim_end_matches('=').chars().enumerate() {
            if i > 0 && i % 72 == 0 {
                payload.push(b'\n');
            }
            payload.push(ch as u8);
        }
        payload.push(0);
        payload
    }

    fn id3_frame_bytes(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        let n = body.len() as u32;
        f.extend_from_slice(&[(n >> 21 & 0x7f) as u8, (n >> 14 & 0x7f) as u8, (n >> 7 & 0x7f) as u8, (n & 0x7f) as u8]);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(body);
        f
    }

    pub fn id3_tag(frames: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = frames.concat();
        let n = body.len() as u32;
        let mut t = b"ID3\x04\x00\x00".to_vec();
        t.extend_from_slice(&[(n >> 21 & 0x7f) as u8, (n >> 14 & 0x7f) as u8, (n >> 7 & 0x7f) as u8, (n & 0x7f) as u8]);
        t.extend_from_slice(&body);
        t
    }

    pub fn mik_id3() -> Vec<u8> {
        let text = |s: &str| {
            let mut v = vec![3u8];
            v.extend_from_slice(s.as_bytes());
            v
        };
        let mut geob = vec![0u8];
        geob.extend_from_slice(b"application/octet-stream\0\0Serato Markers2\0");
        geob.extend_from_slice(&markers2(&[(0, 15_000, "Intro"), (2, 61_500, "Drop")], &[(0, 30_000, 34_000)]));
        let mut grid = vec![0u8];
        grid.extend_from_slice(b"application/octet-stream\0\0Serato BeatGrid\0");
        grid.extend_from_slice(&[1, 0, 0, 0, 0, 1]);
        grid.extend_from_slice(&0.125f32.to_be_bytes());
        grid.extend_from_slice(&124.0f32.to_be_bytes());
        grid.push(0);
        let mut txxx = vec![3u8];
        txxx.extend_from_slice(b"EnergyLevel\x007");
        id3_tag(&[
            id3_frame_bytes(b"TIT2", &text("Night Drive")),
            id3_frame_bytes(b"TPE1", &text("Somebody")),
            id3_frame_bytes(b"TBPM", &text("124")),
            id3_frame_bytes(b"TKEY", &text("8A")),
            id3_frame_bytes(b"TXXX", &txxx),
            id3_frame_bytes(b"GEOB", &geob),
            id3_frame_bytes(b"GEOB", &grid),
        ])
    }

    #[test]
    fn reads_mik_id3_with_serato_cues() {
        let mut t = Tags::default();
        parse_id3(&mik_id3(), &mut t);
        t.finish();
        assert_eq!(t.title.as_deref(), Some("Night Drive"));
        assert_eq!(t.bpm, Some(124.0));
        assert_eq!(t.key.unwrap().musical(), "Am");
        assert_eq!(t.energy, Some(7));
        assert_eq!(t.cues.len(), 3);
        assert_eq!(t.cues[0].secs, 15.0);
        assert_eq!(t.cues[0].name, "Intro");
        assert_eq!(t.cues[0].color, [0xcc, 0, 0]);
        assert_eq!(t.cue_slots[..2], [0, 2]);
        assert_eq!(t.cues[2].kind, CueKind::Loop);
        assert_eq!(t.cues[2].end_secs, Some(34.0));
        assert_eq!(t.grid, Some(BeatGrid { bpm: 124.0, anchor_secs: 0.125 }));
    }

    #[test]
    fn serato_grids_with_several_markers_are_a_tempo_map() {
        // Markers at 0.5 s (64 beats to the next), 32.5 s (64 beats), 61.1 s at 134 BPM.
        let mut p = vec![1u8, 0];
        p.extend_from_slice(&3u32.to_be_bytes());
        for (pos, tail) in [(0.5f32, 64u32.to_be_bytes()), (32.5, 64u32.to_be_bytes()), (61.1, 134.0f32.to_be_bytes())] {
            p.extend_from_slice(&pos.to_be_bytes());
            p.extend_from_slice(&tail);
        }
        p.push(0);
        let m = serato_tempo_map(&p).expect("a map");
        assert!((m.secs_at_beat(64.0) - 32.5).abs() < 1e-5);
        assert!((m.local(10.0).bpm - 120.0).abs() < 1e-3, "{}", m.local(10.0).bpm);
        assert!((m.local(70.0).bpm - 134.0).abs() < 1e-3);
        // One marker: a steady grid, no map.
        let mut one = vec![1u8, 0];
        one.extend_from_slice(&1u32.to_be_bytes());
        one.extend_from_slice(&0.5f32.to_be_bytes());
        one.extend_from_slice(&124.0f32.to_be_bytes());
        one.push(0);
        assert_eq!(serato_tempo_map(&one), None);
        // A marker that says one beat to the next, two and a half minutes on: no 0.4 BPM
        // stretch, the steady grid at the last marker's tempo.
        let mut bad = vec![1u8, 0];
        bad.extend_from_slice(&2u32.to_be_bytes());
        for (pos, tail) in [(0.5f32, 1u32.to_be_bytes()), (146.0, 128.0f32.to_be_bytes())] {
            bad.extend_from_slice(&pos.to_be_bytes());
            bad.extend_from_slice(&tail);
        }
        bad.push(0);
        assert_eq!(serato_tempo_map(&bad), None);
        let mut tags = Tags::default();
        serato_object("Serato BeatGrid", &bad, &mut tags);
        assert_eq!(tags.grid.map(|g| g.bpm as f32), Some(128.0));
    }

    #[test]
    fn a_cue_just_before_the_first_frame_is_at_the_start() {
        // Mixed In Key writes −55 ms as 0xFFFFFFC9: not 49 days in.
        let mut geob = vec![0u8];
        geob.extend_from_slice(b"application/octet-stream\0\0Serato Markers2\0");
        geob.extend_from_slice(&markers2(&[(0, (-55i32) as u32, "Energy 6"), (1, 16_190, "")], &[]));
        let mut t = Tags::default();
        parse_id3(&id3_tag(&[id3_frame_bytes(b"GEOB", &geob)]), &mut t);
        t.finish();
        assert_eq!(t.cues[0].secs, 0.0);
        assert_eq!(t.cues[1].secs, 16.19);
    }

    #[test]
    fn reads_flac_vorbis_comments_with_serato() {
        let mut wrapped = b"application/octet-stream\0\0Serato Markers2\0".to_vec();
        wrapped.extend_from_slice(&markers2(&[(1, 2_000, "")], &[]));
        let comments = [
            "TITLE=Flac Song".to_string(),
            "BPM=126.5".into(),
            "INITIALKEY=Dbm".into(),
            format!("SERATO_MARKERS_V2={}", base64_encode(&wrapped)),
        ];
        let mut vc = Vec::new();
        vc.extend_from_slice(&3u32.to_le_bytes());
        vc.extend_from_slice(b"mik");
        vc.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in &comments {
            vc.extend_from_slice(&(c.len() as u32).to_le_bytes());
            vc.extend_from_slice(c.as_bytes());
        }
        let mut flac = b"fLaC".to_vec();
        flac.push(0x84);
        let l = vc.len();
        flac.extend_from_slice(&[(l >> 16) as u8, (l >> 8) as u8, l as u8]);
        flac.extend_from_slice(&vc);
        let mut t = Tags::default();
        parse_flac(&flac, &mut t);
        t.finish();
        assert_eq!(t.title.as_deref(), Some("Flac Song"));
        assert_eq!(t.bpm, Some(126.5));
        assert_eq!(t.key.unwrap().camelot(), "12A");
        assert_eq!(t.cues.len(), 1);
        assert_eq!(t.cues[0].secs, 2.0);
        assert_eq!(t.cue_slots, vec![1]);
    }

    #[test]
    fn reads_mp4_freeform_key() {
        fn atom(kind: &[u8], body: &[u8]) -> Vec<u8> {
            let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
            v.extend_from_slice(kind);
            v.extend_from_slice(body);
            v
        }
        let data = |t: u32, v: &[u8]| {
            let mut b = t.to_be_bytes().to_vec();
            b.extend_from_slice(&[0; 4]);
            b.extend_from_slice(v);
            atom(b"data", &b)
        };
        let mut free = atom(b"mean", b"\0\0\0\0com.apple.iTunes");
        free.extend(atom(b"name", b"\0\0\0\0initialkey"));
        free.extend(data(1, b"5A"));
        let mut ilst = atom(b"tmpo", &data(21, &[0, 128]));
        ilst.extend(atom(b"----", &free));
        ilst.extend(atom(b"\xa9nam", &data(1, b"M4A Song")));
        let mut meta = vec![0u8; 4];
        meta.extend(atom(b"ilst", &ilst));
        let file = [atom(b"ftyp", b"M4A \0\0\0\0"), atom(b"moov", &atom(b"udta", &atom(b"meta", &meta)))].concat();
        let mut t = Tags::default();
        parse_mp4(&file, &mut t);
        t.finish();
        assert_eq!(t.bpm, Some(128.0));
        assert_eq!(t.key.unwrap().musical(), "Cm");
        assert_eq!(t.title.as_deref(), Some("M4A Song"));
    }

    #[test]
    fn energy_and_key_from_comment() {
        let mut t = Tags { comment: Some("8A - Energy 6".into()), ..Default::default() };
        t.finish();
        assert_eq!(t.key.unwrap().camelot(), "8A");
        assert_eq!(t.energy, Some(6));
    }

    #[test]
    fn lenient_base64() {
        assert_eq!(base64_decode(b"aGVsbG8"), b"hello");
        assert_eq!(base64_decode(b"aGVs\nbG8="), b"hello");
    }
}
