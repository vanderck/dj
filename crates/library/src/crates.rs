//! Crates (named track lists), and reading them from Serato (`_Serato_/Subcrates/*.crate`) and
//! rekordbox (a collection exported as XML).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Crate {
    /// Nested crates are named with their parents: "House / Deep".
    pub name: String,
    pub tracks: Vec<PathBuf>,
    /// Where it was imported from ("Serato", "rekordbox"); empty for dj's own.
    pub source: String,
}

// ---------------------------------------------------------------- Serato

/// Serato's `.crate` file: a list of tags (4 ASCII bytes, a big-endian u32 length, the data);
/// each track is an `otrk` whose `ptrk` holds its path (UTF-16BE, relative to the volume the
/// `_Serato_` folder is on). Returns the paths, made absolute under `volume_root`.
pub fn read_serato_crate(bytes: &[u8], volume_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    for (tag, data) in tlv(bytes)? {
        if tag == *b"otrk" {
            for (t, d) in tlv(data)? {
                if t == *b"ptrk" {
                    let p = utf16be(d);
                    let p = p.trim_start_matches('/');
                    if !p.is_empty() {
                        out.push(volume_root.join(p));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn tlv(mut b: &[u8]) -> Result<Vec<([u8; 4], &[u8])>, String> {
    let mut out = Vec::new();
    while !b.is_empty() {
        if b.len() < 8 {
            return Err("not a Serato crate (truncated)".into());
        }
        let tag = [b[0], b[1], b[2], b[3]];
        let len = u32::from_be_bytes([b[4], b[5], b[6], b[7]]) as usize;
        let data = b.get(8..8 + len).ok_or("not a Serato crate (bad length)")?;
        out.push((tag, data));
        b = &b[8 + len..];
    }
    Ok(out)
}

fn utf16be(d: &[u8]) -> String {
    let units: Vec<u16> = d.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
    String::from_utf16_lossy(&units).trim_end_matches('\0').to_string()
}

/// A crate's name from its file name: Serato nests crates with "%%" ("House%%Deep.crate").
pub fn serato_crate_name(file: &Path) -> String {
    file.file_stem().map(|s| s.to_string_lossy().replace("%%", " / ")).unwrap_or_default()
}

/// The volume a `_Serato_` folder's paths are relative to: the external drive it's on
/// (`/Volumes/<name>`), else the startup disk (`/`).
pub fn serato_volume_root(serato_dir: &Path) -> PathBuf {
    let mut parts = serato_dir.components();
    if let (Some(std::path::Component::RootDir), Some(v), Some(name)) = (parts.next(), parts.next(), parts.next()) {
        if v.as_os_str() == "Volumes" {
            return Path::new("/Volumes").join(name.as_os_str());
        }
    }
    PathBuf::from("/")
}

/// The `_Serato_` folders on this computer: in the Music folder, and on each external drive.
pub fn serato_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join("Music").join("_Serato_"));
    }
    if let Ok(vols) = std::fs::read_dir("/Volumes") {
        dirs.extend(vols.flatten().map(|v| v.path().join("_Serato_")));
    }
    dirs.retain(|d| d.join("Subcrates").is_dir());
    dirs
}

/// Every crate in a `_Serato_` folder's `Subcrates`, by name.
pub fn import_serato(serato_dir: &Path) -> Result<Vec<Crate>, String> {
    let root = serato_volume_root(serato_dir);
    let dir = serato_dir.join("Subcrates");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("crate")))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for f in files {
        let bytes = std::fs::read(&f).map_err(|e| format!("{}: {e}", f.display()))?;
        match read_serato_crate(&bytes, &root) {
            Ok(tracks) => out.push(Crate { name: serato_crate_name(&f), tracks, source: "Serato".into() }),
            Err(e) => return Err(format!("{}: {e}", f.display())),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- rekordbox

/// rekordbox's XML export (File → Export Collection in xml format): its playlists, named with
/// their folders ("Sets / Friday"), the tracks as files.
pub fn read_rekordbox_xml(xml: &str) -> Result<Vec<Crate>, String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| format!("not an XML file: {e}"))?;
    let root = doc.root_element();
    if !root.has_tag_name("DJ_PLAYLISTS") {
        return Err("not a rekordbox collection (no DJ_PLAYLISTS)".into());
    }
    let mut by_id = std::collections::HashMap::new();
    if let Some(col) = root.children().find(|n| n.has_tag_name("COLLECTION")) {
        for t in col.children().filter(|n| n.has_tag_name("TRACK")) {
            if let (Some(id), Some(loc)) = (t.attribute("TrackID"), t.attribute("Location")) {
                if let Some(p) = file_url_path(loc) {
                    by_id.insert(id.to_string(), p);
                }
            }
        }
    }
    let mut out = Vec::new();
    if let Some(pl) = root.children().find(|n| n.has_tag_name("PLAYLISTS")) {
        for n in pl.children().filter(|n| n.has_tag_name("NODE")) {
            walk(n, &mut Vec::new(), &by_id, &mut out);
        }
    }
    Ok(out)
}

fn walk(node: roxmltree::Node, path: &mut Vec<String>, by_id: &std::collections::HashMap<String, PathBuf>, out: &mut Vec<Crate>) {
    let name = node.attribute("Name").unwrap_or_default().to_string();
    // (The top folder is "ROOT": not part of the names.)
    let named = !(path.is_empty() && name == "ROOT");
    if named {
        path.push(name);
    }
    match node.attribute("Type") {
        Some("1") => {
            // KeyType 0: tracks by their TrackID in the collection; 1: by location.
            let by_location = node.attribute("KeyType") == Some("1");
            let tracks = node
                .children()
                .filter(|n| n.has_tag_name("TRACK"))
                .filter_map(|t| t.attribute("Key"))
                .filter_map(|k| if by_location { file_url_path(k) } else { by_id.get(k).cloned() })
                .collect();
            out.push(Crate { name: path.join(" / "), tracks, source: "rekordbox".into() });
        }
        _ => {
            for c in node.children().filter(|n| n.has_tag_name("NODE")) {
                walk(c, path, by_id, out);
            }
        }
    }
    if named {
        path.pop();
    }
}

/// `file://localhost/Users/me/My%20Track.mp3` → `/Users/me/My Track.mp3` (Windows:
/// `file://localhost/C:/Music/…` → `C:/Music/…`).
pub fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://localhost").or_else(|| url.strip_prefix("file://"))?;
    let decoded = percent_decode(rest);
    let windows = decoded.len() > 2 && decoded.as_bytes()[0] == b'/' && decoded.as_bytes()[2] == b':';
    Some(PathBuf::from(if windows { &decoded[1..] } else { &decoded }))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(t: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = t.to_vec();
        v.extend((data.len() as u32).to_be_bytes());
        v.extend(data);
        v
    }

    fn u16be(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()
    }

    #[test]
    fn serato_crates_are_read_with_their_paths_and_names() {
        let mut b = tag(b"vrsn", &u16be("1.0/Serato ScratchLive Crate"));
        b.extend(tag(b"osrt", &tag(b"tvcn", &u16be("song"))));
        for p in ["Users/kris/Music/House/A Track.mp3", "Users/kris/Music/Ünïcode – B.flac"] {
            b.extend(tag(b"otrk", &tag(b"ptrk", &u16be(p))));
        }
        let tracks = read_serato_crate(&b, Path::new("/")).unwrap();
        assert_eq!(tracks, vec![PathBuf::from("/Users/kris/Music/House/A Track.mp3"), PathBuf::from("/Users/kris/Music/Ünïcode – B.flac")]);
        // On an external drive: relative to it.
        let ext = read_serato_crate(&tag(b"otrk", &tag(b"ptrk", &u16be("DJ/x.mp3"))), &serato_volume_root(Path::new("/Volumes/USB/_Serato_"))).unwrap();
        assert_eq!(ext, vec![PathBuf::from("/Volumes/USB/DJ/x.mp3")]);
        assert_eq!(serato_volume_root(Path::new("/Users/kris/Music/_Serato_")), PathBuf::from("/"));
        assert_eq!(serato_crate_name(Path::new("/x/Subcrates/House%%Deep.crate")), "House / Deep");
        assert!(read_serato_crate(&[0, 1, 2], Path::new("/")).is_err());
        assert!(read_serato_crate(&tag(b"otrk", &[0; 3])[..10], Path::new("/")).is_err());
    }

    #[test]
    fn serato_folder_import() {
        let dir = std::env::temp_dir().join(format!("dj-serato-{}", std::process::id()));
        let sub = dir.join("_Serato_").join("Subcrates");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("Sets%%Friday.crate"), tag(b"otrk", &tag(b"ptrk", &u16be("Music/a.mp3")))).unwrap();
        std::fs::write(sub.join("notes.txt"), b"x").unwrap();
        let crates = import_serato(&dir.join("_Serato_")).unwrap();
        assert_eq!(crates, vec![Crate { name: "Sets / Friday".into(), tracks: vec![PathBuf::from("/Music/a.mp3")], source: "Serato".into() }]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rekordbox_playlists_are_read_with_their_folders() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<DJ_PLAYLISTS Version="1.0.0">
  <PRODUCT Name="rekordbox" Version="6.8.5" Company="AlphaTheta"/>
  <COLLECTION Entries="3">
    <TRACK TrackID="1" Name="One" Location="file://localhost/Users/kris/Music/One%20Track.mp3"/>
    <TRACK TrackID="2" Name="Two" Location="file://localhost/Users/kris/Music/Caf%C3%A9.mp3"/>
    <TRACK TrackID="3" Name="Win" Location="file://localhost/C:/Music/Three.wav"/>
  </COLLECTION>
  <PLAYLISTS>
    <NODE Type="0" Name="ROOT" Count="2">
      <NODE Name="Warm up" Type="1" KeyType="0" Entries="2"><TRACK Key="2"/><TRACK Key="1"/></NODE>
      <NODE Type="0" Name="Sets" Count="1">
        <NODE Name="Friday" Type="1" KeyType="1" Entries="1"><TRACK Key="file://localhost/C:/Music/Three.wav"/></NODE>
      </NODE>
    </NODE>
  </PLAYLISTS>
</DJ_PLAYLISTS>"#;
        let crates = read_rekordbox_xml(xml).unwrap();
        assert_eq!(crates.len(), 2);
        assert_eq!(crates[0].name, "Warm up");
        assert_eq!(crates[0].tracks, vec![PathBuf::from("/Users/kris/Music/Café.mp3"), PathBuf::from("/Users/kris/Music/One Track.mp3")]);
        assert_eq!((crates[1].name.as_str(), crates[1].source.as_str()), ("Sets / Friday", "rekordbox"));
        assert_eq!(crates[1].tracks, vec![PathBuf::from("C:/Music/Three.wav")]);
        assert!(read_rekordbox_xml("<plist/>").is_err());
        assert!(read_rekordbox_xml("not xml").is_err());
    }
}
