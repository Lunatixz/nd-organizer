// Identity confidence: how trustworthy is a file's album pairing?
//
// Text tags alone (album artist / album name) can be wrong, so a file is only
// fully trusted when it carries an unambiguous identifier: a MusicBrainz ID or
// an ISRC. AcoustID fingerprint matches provide partial confidence for files
// with no identifier at all. The organizer pairs files into albums only above
// the configured `minConfidence` threshold, and `skipUnverified` decides what
// happens to files below it (drop them, or organize by tag/folder heuristics).

use crate::tags::TrackTags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Verified,
    Unverified,
}

/// Weighted identity score (0.0 - 1.0):
///   - album MBID + (recording or artist MBID): 1.0
///   - any single MBID:                         0.9
///   - ISRC only:                               0.85
///   - AcoustID fingerprint match:              0.5 + 0.5 * confidence
///   - nothing:                                 0.2
pub fn score(t: &TrackTags, acoustid_conf: Option<f64>) -> f64 {
    let has_album = !t.mbid_album.trim().is_empty();
    let has_rec = !t.mbid_recording.trim().is_empty();
    let has_artist = !t.mbid_artist.trim().is_empty();
    let has_isrc = !t.isrc.trim().is_empty();
    if has_album && (has_rec || has_artist) {
        return 1.0;
    }
    if has_album || has_rec || has_artist {
        return 0.9;
    }
    if has_isrc {
        return 0.85;
    }
    if let Some(c) = acoustid_conf {
        return 0.5 + 0.5 * c.clamp(0.0, 1.0);
    }
    0.2
}

/// True when a file's score meets the configured minimum confidence.
pub fn is_verified(t: &TrackTags, min_confidence: f64, acoustid_conf: Option<f64>) -> bool {
    score(t, acoustid_conf) >= min_confidence.clamp(0.0, 1.0)
}

/// Backward-compatible binary verdict: verified when the score is at/above 0.6.
pub fn confidence(t: &TrackTags) -> Confidence {
    if score(t, None) >= 0.6 {
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
        assert!(score(&t, None) < 0.6);
    }

    #[test]
    fn any_id_is_verified() {
        let mut t = tags();
        t.mbid_album = "abc".into();
        assert_eq!(confidence(&t), Confidence::Verified);
        assert!(is_verified(&t, 0.6, None));

        let mut t = tags();
        t.isrc = "USRC17607839".into();
        assert_eq!(confidence(&t), Confidence::Verified);
        assert!(is_verified(&t, 0.6, None));

        let mut t = tags();
        t.mbid_recording = "def".into();
        assert_eq!(confidence(&t), Confidence::Verified);
    }

    #[test]
    fn score_thresholds_and_acoustid() {
        let mut t = tags();
        t.mbid_album = "a".into();
        t.mbid_recording = "r".into();
        assert_eq!(score(&t, None), 1.0);
        assert!(is_verified(&t, 0.9, None));

        // No ID + strong acoustid match clears a strict threshold.
        let bare = tags();
        assert!(is_verified(&bare, 0.7, Some(0.8)));
        // No ID + weak acoustid match does not.
        assert!(!is_verified(&bare, 0.7, Some(0.2)));

        // Strict user threshold rejects a lone ISRC.
        assert!(!is_verified(&bare, 0.95, None));
    }
}
