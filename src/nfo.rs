// Kodi-style NFO sidecar metadata.
//
// `album.nfo` / `artist.nfo` are XML sidecars users sometimes maintain by hand.
// They can fill metadata that external APIs can't provide (styles, moods,
// biographies, custom fields). We read them as a fallback source and rewrite
// them once we've collected fresh metadata.
//
// Read paths are filesystem-based (`read_album_nfo` / `read_artist_nfo`); the
// XML parse/serialize functions are pure for testability.
//
// Album NFO fields: title, artists, album_artists, year, genres, styles, moods,
// mbid, releasedate, description, bpm, key, chords, structure.
// Artist NFO fields: name, genres, styles, moods, mbid, biography, similar_artists.

use std::path::Path;

use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NfoAlbum {
    pub title: String,
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub year: Option<u32>,
    pub genres: Vec<String>,
    pub styles: Vec<String>,
    pub moods: Vec<String>,
    pub mbid: String,
    pub releasedate: String,
    pub description: String,
    pub bpm: Option<f64>,
    pub key: String,
    pub chords: Vec<String>,
    pub structure: Vec<String>,
    pub credits: Vec<NfoCredit>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NfoCredit {
    pub name: String,
    pub role: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct NfoArtist {
    pub name: String,
    pub genres: Vec<String>,
    pub styles: Vec<String>,
    pub moods: Vec<String>,
    pub mbid: String,
    pub biography: String,
    pub similar_artists: Vec<String>,
}

pub fn read_album_nfo(dir: &Path) -> Option<NfoAlbum> {
    let xml = std::fs::read_to_string(dir.join("album.nfo")).ok()?;
    parse_album_nfo(&xml)
}

/// Look for `artist.nfo` in the album dir, then its parent (Kodi convention:
/// artist.nfo lives in the artist folder).
pub fn read_artist_nfo(dir: &Path) -> Option<NfoArtist> {
    let mut candidates = vec![dir.to_path_buf()];
    if let Some(parent) = dir.parent() {
        candidates.push(parent.to_path_buf());
    }
    for d in candidates {
        if let Ok(xml) = std::fs::read_to_string(d.join("artist.nfo")) {
            if let Some(artist) = parse_artist_nfo(&xml) {
                return Some(artist);
            }
        }
    }
    None
}

fn parse_year(value: &str) -> Option<u32> {
    let digits: String = value
        .chars()
        .filter(|c| c.is_ascii_digit())
        .take(4)
        .collect();
    digits.parse().ok()
}

pub fn parse_album_nfo(xml: &str) -> Option<NfoAlbum> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut nfo = NfoAlbum::default();
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                stack.push(String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase());
            }
            Ok(Event::Empty(e)) => {
                let tag = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                if tag == "credit" {
                    let mut name = String::new();
                    let mut role = String::new();
                    for attr in e.attributes().flatten() {
                        match attr.key.as_ref() {
                            b"name" => name = String::from_utf8_lossy(&attr.value).to_string(),
                            b"role" => role = String::from_utf8_lossy(&attr.value).to_string(),
                            _ => {}
                        }
                    }
                    if !name.is_empty() {
                        nfo.credits.push(crate::nfo::NfoCredit { name, role });
                    }
                }
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = stack.last() {
                    let value = t.unescape().unwrap_or_default().trim().to_string();
                    match field.as_str() {
                        "title" => nfo.title = value,
                        "artist" if !value.is_empty() => nfo.artists.push(value),
                        "albumartist" if !value.is_empty() => nfo.album_artists.push(value),
                        "year" => nfo.year = parse_year(&value),
                        "genre" if !value.is_empty() => nfo.genres.push(value),
                        "styles" if !value.is_empty() => nfo.styles.push(value),
                        "moods" if !value.is_empty() => nfo.moods.push(value),
                        "mbid" if !value.is_empty() => nfo.mbid = value,
                        "releasedate" if !value.is_empty() => nfo.releasedate = value,
                        "description" if !value.is_empty() => nfo.description = value,
                        "bpm" => nfo.bpm = value.parse().ok(),
                        "key" if !value.is_empty() => nfo.key = value,
                        "chord" if !value.is_empty() => nfo.chords.push(value),
                        "structure" if !value.is_empty() => nfo.structure.push(value),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    // A non-empty file with at least one useful field counts as parsed.
    if nfo.title.is_empty() && nfo.artists.is_empty() && nfo.year.is_none() && nfo.mbid.is_empty() {
        None
    } else {
        Some(nfo)
    }
}

pub fn parse_artist_nfo(xml: &str) -> Option<NfoArtist> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut nfo = NfoArtist::default();
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                stack.push(String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase());
            }
            Ok(Event::End(_)) => {
                stack.pop();
            }
            Ok(Event::Text(t)) => {
                if let Some(field) = stack.last() {
                    let value = t.unescape().unwrap_or_default().trim().to_string();
                    match field.as_str() {
                        "name" => nfo.name = value,
                        "genre" if !value.is_empty() => nfo.genres.push(value),
                        "styles" if !value.is_empty() => nfo.styles.push(value),
                        "moods" if !value.is_empty() => nfo.moods.push(value),
                        "mbid" if !value.is_empty() => nfo.mbid = value,
                        "biography" | "biog" => nfo.biography = value,
                        "similarartist" if !value.is_empty() => nfo.similar_artists.push(value),
                        _ => {}
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    if nfo.name.is_empty() && nfo.mbid.is_empty() {
        None
    } else {
        Some(nfo)
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Serialize an album NFO (Kodi-style).
pub fn serialize_album(nfo: &NfoAlbum) -> String {
    let mut out =
        String::from("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<album>\n");
    if !nfo.title.is_empty() {
        out.push_str(&format!("  <title>{}</title>\n", esc(&nfo.title)));
    }
    for a in &nfo.artists {
        out.push_str(&format!("  <artist>{}</artist>\n", esc(a)));
    }
    for a in &nfo.album_artists {
        out.push_str(&format!("  <albumartist>{}</albumartist>\n", esc(a)));
    }
    if let Some(y) = nfo.year {
        out.push_str(&format!("  <year>{y}</year>\n"));
    }
    for g in &nfo.genres {
        out.push_str(&format!("  <genre>{}</genre>\n", esc(g)));
    }
    for s in &nfo.styles {
        out.push_str(&format!("  <styles>{}</styles>\n", esc(s)));
    }
    for m in &nfo.moods {
        out.push_str(&format!("  <moods>{}</moods>\n", esc(m)));
    }
    if !nfo.mbid.is_empty() {
        out.push_str(&format!("  <mbid>{}</mbid>\n", esc(&nfo.mbid)));
    }
    if !nfo.releasedate.is_empty() {
        out.push_str(&format!(
            "  <releasedate>{}</releasedate>\n",
            esc(&nfo.releasedate)
        ));
    }
    if !nfo.description.is_empty() {
        out.push_str(&format!(
            "  <description>{}</description>\n",
            esc(&nfo.description)
        ));
    }
    if let Some(bpm) = nfo.bpm {
        out.push_str(&format!("  <bpm>{:.1}</bpm>\n", bpm));
    }
    if !nfo.key.is_empty() {
        out.push_str(&format!("  <key>{}</key>\n", esc(&nfo.key)));
    }
    for c in &nfo.chords {
        out.push_str(&format!("  <chord>{}</chord>\n", esc(c)));
    }
    for s in &nfo.structure {
        out.push_str(&format!("  <structure>{}</structure>\n", esc(s)));
    }
    for c in &nfo.credits {
        out.push_str(&format!(
            "  <credit name=\"{}\" role=\"{}\"/>\n",
            esc(&c.name),
            esc(&c.role)
        ));
    }
    out.push_str("</album>\n");
    out
}

/// Serialize an artist NFO (Kodi-style).
pub fn serialize_artist(nfo: &NfoArtist) -> String {
    let mut out =
        String::from("<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>\n<artist>\n");
    if !nfo.name.is_empty() {
        out.push_str(&format!("  <name>{}</name>\n", esc(&nfo.name)));
    }
    for g in &nfo.genres {
        out.push_str(&format!("  <genre>{}</genre>\n", esc(g)));
    }
    for s in &nfo.styles {
        out.push_str(&format!("  <styles>{}</styles>\n", esc(s)));
    }
    for m in &nfo.moods {
        out.push_str(&format!("  <moods>{}</moods>\n", esc(m)));
    }
    if !nfo.mbid.is_empty() {
        out.push_str(&format!("  <mbid>{}</mbid>\n", esc(&nfo.mbid)));
    }
    if !nfo.biography.is_empty() {
        out.push_str(&format!(
            "  <biography>{}</biography>\n",
            esc(&nfo.biography)
        ));
    }
    for sa in &nfo.similar_artists {
        out.push_str(&format!("  <similarartist>{}</similarartist>\n", esc(sa)));
    }
    out.push_str("</artist>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_album_nfo() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<album>
  <title>Dark Side of the Moon</title>
  <artist>Pink Floyd</artist>
  <albumartist>Pink Floyd</albumartist>
  <year>1973-03-01</year>
  <genre>Rock</genre>
  <genre>Progressive</genre>
  <styles>Progressive Rock</styles>
  <moods>Brooding</moods>
  <mbid>e1a4f3b4-4f2a-4d5a-8c1a-2a3b4c5d6e7f</mbid>
</album>"#;
        let nfo = parse_album_nfo(xml).unwrap();
        assert_eq!(nfo.title, "Dark Side of the Moon");
        assert_eq!(nfo.artists, vec!["Pink Floyd"]);
        assert_eq!(nfo.album_artists, vec!["Pink Floyd"]);
        assert_eq!(nfo.year, Some(1973));
        assert_eq!(nfo.genres, vec!["Rock", "Progressive"]);
        assert_eq!(nfo.styles, vec!["Progressive Rock"]);
        assert_eq!(nfo.moods, vec!["Brooding"]);
        assert!(!nfo.mbid.is_empty());
    }

    #[test]
    fn parses_artist_nfo() {
        let xml = r#"<artist>
  <name>Pink Floyd</name>
  <genre>Rock</genre>
  <styles>Psychedelic</styles>
  <moods>Melancholy</moods>
  <mbid>83d91898-7763-47d7-b03b-b92132375c47</mbid>
  <biography>English rock band.</biography>
</artist>"#;
        let artist = parse_artist_nfo(xml).unwrap();
        assert_eq!(artist.name, "Pink Floyd");
        assert_eq!(artist.biography, "English rock band.");
        assert_eq!(artist.moods, vec!["Melancholy"]);
    }

    #[test]
    fn rejects_empty_files() {
        assert!(parse_album_nfo("<album></album>").is_none());
        assert!(parse_artist_nfo("<artist><name></name></artist>").is_none());
    }

    #[test]
    fn round_trips_serialize() {
        let nfo = NfoAlbum {
            title: "A & B <C>".into(),
            artists: vec!["Artist \"X\"".into()],
            year: Some(1999),
            ..Default::default()
        };
        let xml = serialize_album(&nfo);
        let reparsed = parse_album_nfo(&xml).unwrap();
        assert_eq!(reparsed.title, "A & B <C>");
        assert_eq!(reparsed.artists, vec!["Artist \"X\""]);
        assert_eq!(reparsed.year, Some(1999));
    }
}
