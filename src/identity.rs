// Identity confidence: is a file's album pairing trustworthy?
//
// Text tags alone (album artist / album name) can be wrong, so a file is only
// "verified" when it carries an unambiguous identifier: a MusicBrainz ID or an
// ISRC. When AcoustID fingerprinting is unavailable, files without any such ID
// are treated as UNVERIFIED and are not confidently paired into an album.

use crate::tags::TrackTags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Verified,
    Unverified,
}

/// A file is verified if it carries any of: album MBID, recording MBID, artist
/// MBID, or an ISRC.
pub fn confidence(t: &TrackTags) -> Confidence {
    if !t.mbid_album.trim().is_empty()
        || !t.mbid_recording.trim().is_empty()
        || !t.mbid_artist.trim().is_empty()
        || !t.isrc.trim().is_empty()
    {
        Confidence::Verified
    } else {
        Confidence::Unverified
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tags::TrackTags;

    fn tags() -> TrackTags {
        TrackTags::default()
    }

    #[test]
    fn text_only_is_unverified() {
        let mut t = tags();
        t.album = "Some Album".into();
        t.album_artist = "Some Artist".into();
        assert_eq!(confidence(&t), Confidence::Unverified);
    }

    #[test]
    fn any_id_is_verified() {
        let mut t = tags();
        t.mbid_album = "abc".into();
        assert_eq!(confidence(&t), Confidence::Verified);

        let mut t = tags();
        t.isrc = "USRC17607839".into();
        assert_eq!(confidence(&t), Confidence::Verified);

        let mut t = tags();
        t.mbid_recording = "def".into();
        assert_eq!(confidence(&t), Confidence::Verified);
    }
}
