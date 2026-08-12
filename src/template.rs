// Schema template engine + filename sanitizer.
//
// Templates use `{placeholder}` and `{placeholder:format}` tokens, e.g.
// `{track:02} - {title}`. Format is an integer zero-pad width (only meaningful
// for numeric fields). Unknown placeholders render as empty strings.
//
// All path/filename components must be passed through `sanitize` before use so
// a metadata value can never break out of its component or produce an invalid
// file name on any OS.

/// Values available to templates. Any field may be empty/None; empty renders as
/// an empty string in the output.
#[derive(Debug, Clone, Default)]
pub struct TemplateFields {
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub title: String,
    pub artist: String,
    pub album_artist: String,
    pub album: String,
    pub year: Option<u32>,
    pub genre: String,
    /// "Live"/"Bootleg"/"" (recording source, so live tracks are never
    /// confused with studio releases).
    pub recording: String,
    pub mbid: String,
}

impl TemplateFields {
    fn resolve(&self, name: &str) -> Option<String> {
        let value: Option<String> = match name {
            "track" => self.track.map(|v| v.to_string()),
            "disc" => self.disc.map(|v| v.to_string()),
            "year" => self.year.map(|v| v.to_string()),
            "title" => Some(self.title.clone()),
            "artist" => Some(self.artist.clone()),
            "albumArtist" => Some(self.album_artist.clone()),
            "album" => Some(self.album.clone()),
            "genre" => Some(self.genre.clone()),
            "recording" => Some(self.recording.clone()),
            "mbid" => Some(self.mbid.clone()),
            _ => None,
        };
        value.filter(|v| !v.is_empty())
    }
}

/// Render a single template string against the given fields.
pub fn render(schema: &str, fields: &TemplateFields) -> String {
    let mut out = String::with_capacity(schema.len());
    let bytes = schema.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'{' {
            if let Some(end) = schema[i..].find('}') {
                let token = &schema[i + 1..i + end];
                if !token.is_empty() {
                    let (name, fmt) = match token.split_once(':') {
                        Some((n, f)) => (n, Some(f)),
                        None => (token, None),
                    };
                    if let Some(v) = fields.resolve(name) {
                        let rendered = match (fmt, name) {
                            (Some(f), "track") => pad_field(&v, f),
                            (Some(f), "disc") => pad_field(&v, f),
                            (Some(f), "year") => pad_field(&v, f),
                            _ => v,
                        };
                        out.push_str(&rendered);
                    }
                    i += end + 1;
                    continue;
                }
            }
        }
        out.push(b as char);
        i += 1;
    }
    out
}

fn pad_field(value: &str, fmt: &str) -> String {
    let Ok(width) = fmt.parse::<usize>() else {
        return value.to_string();
    };
    let value = value.trim_start_matches('-');
    if value.len() >= width {
        value.to_string()
    } else {
        format!("{}{}", "0".repeat(width - value.len()), value)
    }
}

/// Options controlling sanitization. Users can tweak these in the plugin config.
#[derive(Debug, Clone, Copy)]
pub struct SanitizeOptions {
    pub illegal_char_replacement: char,
    pub max_name_length: usize,
}

impl Default for SanitizeOptions {
    fn default() -> Self {
        SanitizeOptions {
            illegal_char_replacement: '_',
            max_name_length: 180,
        }
    }
}

/// Characters that are illegal in a file/folder name component on common OSes.
fn is_forbidden(c: char) -> bool {
    matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
}

fn is_reserved_windows(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (base.starts_with("COM") && base[3..].parse::<u8>().is_ok())
        || (base.starts_with("LPT") && base[3..].parse::<u8>().is_ok())
}

/// Sanitize a single path component (file or folder name, no separators).
/// Replaces illegal chars, strips control chars and trailing dots/spaces,
/// guards Windows reserved names, and caps length.
pub fn sanitize_with(s: &str, opts: &SanitizeOptions) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if is_forbidden(c) {
                opts.illegal_char_replacement
            } else if (c as u32) < 32 {
                ' '
            } else {
                c
            }
        })
        .collect();

    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    if out.is_empty() {
        return out;
    }
    if is_reserved_windows(&out) {
        out.insert(0, '_');
    }
    if out.chars().count() > opts.max_name_length {
        let truncated: String = out.chars().take(opts.max_name_length).collect();
        out = truncated.trim_end_matches(['.', ' ']).to_string();
        if out.is_empty() {
            out = "trimmed".into();
        }
    }
    out
}

/// Sanitize with default options (convenience for callers without a config).
#[cfg(test)]
pub fn sanitize(s: &str) -> String {
    sanitize_with(s, &SanitizeOptions::default())
}

/// Split a folder schema into sanitized components and join with `/`.
/// Returns an empty string if the result would be empty.
pub fn render_folder_path(schema: &str, fields: &TemplateFields, opts: &SanitizeOptions) -> String {
    let rendered = render(schema, fields);
    let mut components: Vec<String> = Vec::new();
    for part in rendered.split('/') {
        let comp = sanitize_with(part, opts);
        if !comp.is_empty() {
            components.push(comp);
        }
    }
    components.join("/")
}

/// Render a file name (without extension) and sanitize it.
pub fn render_file_name(schema: &str, fields: &TemplateFields, opts: &SanitizeOptions) -> String {
    let name = sanitize_with(&render(schema, fields), opts);
    if name.is_empty() {
        "_untitled".into()
    } else {
        name
    }
}

/// Translate a Lidarr naming-format string (from /api/v1/config/naming) into
/// our template syntax. Best-effort: unknown Lidarr tokens are left in place
/// (our renderer drops unrecognized placeholders).
pub fn translate_lidarr_format(fmt: &str) -> String {
    let mut s = fmt.to_string();
    for (from, to) in [
        ("{Track Title With Artist}", "{artist} - {title}"),
        ("{track total:00}", "{trackTotal}"),
        ("{track total}", "{trackTotal}"),
        ("{medium total:00}", "{discTotal}"),
        ("{medium total}", "{discTotal}"),
        ("{track:000}", "{track:03}"),
        ("{track:00}", "{track:02}"),
        ("{medium:00}", "{disc:02}"),
        ("{Medium Format}", "{disc}"),
        ("{medium}", "{disc}"),
        ("{Track Title}", "{title}"),
        ("{Recording Title}", "{title}"),
        ("{Album Title}", "{album}"),
        ("{Release Year}", "{year}"),
        ("{Release Date}", "{year}"),
        ("{Album Artist Name}", "{albumArtist}"),
        ("{Artist Name}", "{albumArtist}"),
        ("{Artists}", "{artist}"),
        ("{Artist Disambiguation}", ""),
        ("{Album Disambiguation}", ""),
        ("{Label}", ""),
        ("{Catalog Number}", ""),
        ("{Track Title First Character}", ""),
    ] {
        s = s.replace(from, to);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> TemplateFields {
        TemplateFields {
            track: Some(1),
            disc: Some(2),
            title: "Dream On".into(),
            artist: "Aerosmith".into(),
            album_artist: "Various Artists".into(),
            album: "Rock: 70's".into(),
            year: Some(1973),
            genre: "Rock".into(),
            recording: String::new(),
            mbid: "abc-123".into(),
        }
    }

    fn opts() -> SanitizeOptions {
        SanitizeOptions::default()
    }

    #[test]
    fn pads_numeric_fields() {
        let f = fields();
        assert_eq!(render("{track:02} - {title}", &f), "01 - Dream On");
        assert_eq!(render("{disc:02}-{track:02}", &f), "02-01");
        assert_eq!(render("{year:04}", &f), "1973");
    }

    #[test]
    fn recording_placeholder_renders() {
        let mut f = fields();
        f.recording = "Live".into();
        assert_eq!(render("{title} ({recording})", &f), "Dream On (Live)");
        f.recording = "".into();
        assert_eq!(render("{title} ({recording})", &f), "Dream On ()");
    }

    #[test]
    fn lidarr_format_translation() {
        assert_eq!(
            translate_lidarr_format("{Artist Name}/{Album Title} ({Release Year})"),
            "{albumArtist}/{album} ({year})"
        );
        assert_eq!(
            translate_lidarr_format("{medium:00} - {track:00} - {Track Title}"),
            "{disc:02} - {track:02} - {title}"
        );
        assert_eq!(
            translate_lidarr_format("{Track Title With Artist}"),
            "{artist} - {title}"
        );
        // Unknown tokens survive (they render empty downstream).
        assert_eq!(translate_lidarr_format("{Some Custom Token}"), "{Some Custom Token}");
    }

    #[test]
    fn missing_numeric_renders_empty() {
        let mut f = fields();
        f.track = None;
        assert_eq!(render("{track:02} {title}", &f), " Dream On");
    }

    #[test]
    fn unknown_placeholder_is_dropped() {
        let f = fields();
        assert_eq!(render("a{unknown}b", &f), "ab");
    }

    #[test]
    fn folder_schema_nests_and_sanitizes() {
        let f = fields();
        assert_eq!(
            render_folder_path("{albumArtist}/{album} ({year})", &f, &opts()),
            "Various Artists/Rock_ 70's (1973)"
        );
    }

    #[test]
    fn empty_components_are_dropped() {
        let mut f = fields();
        f.album_artist = "".into();
        f.year = None;
        assert_eq!(render_folder_path("{albumArtist}/{album} ({year})", &f, &opts()), "Rock_ 70's ()");
    }

    #[test]
    fn custom_sanitize_options_apply() {
        let o = SanitizeOptions {
            illegal_char_replacement: '-',
            max_name_length: 10,
        };
        assert_eq!(sanitize_with("a/b:c", &o), "a-b-c");
        let long = "x".repeat(50);
        assert_eq!(sanitize_with(&long, &o).chars().count(), 10);
    }

    #[test]
    fn sanitize_replaces_illegal_and_trims() {
        assert_eq!(sanitize("a/b\\c:d*e?f\"g<h>i|j"), "a_b_c_d_e_f_g_h_i_j");
        assert_eq!(sanitize("trailing.. "), "trailing");
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("lpt1.txt"), "_lpt1.txt");
    }

    #[test]
    fn long_names_capped() {
        let long = "x".repeat(500);
        let out = sanitize(&long);
        assert!(out.chars().count() <= 180);
    }
}
