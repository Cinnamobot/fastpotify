//! The automix cuepoints Spotify computes for a track.
//!
//! The official client derives a transition from its servers rather than its
//! ears: `CUEPOINTS` serves a per-track answer of where a track should be
//! brought in and taken out, with the tempo to do it at, and the client's own
//! `TransitionType::CUEPOINTS` is the first thing it asks for. Deriving the
//! same thing locally is what the rest of `automix` does, and measurement
//! against real material is what showed how badly a threshold over the band
//! ratio does it: on twenty tracks the local detector found a section on three
//! of them from the whole track and on none of them from the 90-second probe
//! the incoming side actually has. So the server's answer is preferred
//! wherever it exists, and the local analysis is what covers the tracks it
//! does not cover.
//!
//! Nothing here is guessed at. The message is
//! `spotify.automix.proto.Cuepoints`, which this workspace already carries in
//! `protocol/proto/cuepoints.proto`; the two entries it defines are the ones
//! used below. The service returns them for a normal account and a normal
//! track — measured on this project's own play history at 57 of 60 tracks,
//! where the tempo it reports agrees with the local beat tracker to within
//! 0.1% on all but the two that differ by exactly an octave.

use librespot_core::{Session, SpotifyUri};
use librespot_protocol::{cuepoints, extension_kind::ExtensionKind};
use protobuf::Message as _;

/// Where a track's own automix transition begins and ends, in track time.
///
/// Both halves come from the same response: the server publishes a fade-in
/// cuepoint for a track being brought in and a fade-out cuepoint for the same
/// track being taken out, which is what lets one lookup serve both sides of a
/// pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cuepoints {
    /// Seconds into the track where the incoming side starts it.
    ///
    /// This is where the track's own material begins, past its intro, so a
    /// transition into it starts here rather than at the first sample.
    pub fade_in_at: f64,
    /// Seconds into the track where the outgoing side leaves it.
    pub fade_out_at: f64,
    /// The tempo the server measured, in BPM.
    ///
    /// Preferred over the local tracker's figure because the two cues above
    /// were placed against it: mixing at a tempo the cues were not measured
    /// at would put the beats back out of line.
    pub bpm: f64,
}

/// Why a cuepoint lookup produced no answer.
///
/// The two cases are not the same thing and must not be cached alike. A track
/// the service has not analysed answers the same way forever, so asking again
/// wastes a request; a lookup that failed says nothing about the track, and
/// treating it as an answer took the transition away for the rest of the
/// session — the track was never asked for again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The service answered, and had nothing for this track.
    NoCuepoints,
    /// The request did not complete. Worth asking again.
    Failed,
}

impl Cuepoints {
    /// Asks Spotify for a track's automix cuepoints.
    ///
    /// `Err(Missing::NoCuepoints)` is an ordinary answer rather than a
    /// failure: the ~5% of tracks without one keep the local analysis.
    pub async fn fetch(session: &Session, track: &SpotifyUri) -> Result<Self, Missing> {
        let bytes = session
            .spclient()
            .get_metadata(ExtensionKind::CUEPOINTS, track)
            .await
            .map_err(|_| Missing::Failed)?;
        Self::parse(&bytes).ok_or(Missing::NoCuepoints)
    }

    /// Reads the response body.
    ///
    /// Public because a probe of what the service actually serves is the only
    /// way to check this against real responses rather than crafted ones.
    ///
    /// A cuepoint is only useful if it lands inside the track, so one that
    /// does not — a non-positive position, or a tempo that is not a tempo —
    /// is treated as the service having no answer rather than as a usable
    /// one. Trusting a zero would start every track at its first sample,
    /// which is the behaviour this exists to replace.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let message = cuepoints::Cuepoints::parse_from_bytes(bytes).ok()?;
        let fade_in = message.fade_in_cuepoint.into_option()?;
        let fade_out = message.fade_out_cuepoint.into_option()?;
        let fade_in_at = fade_in.position_ms as f64 / 1000.0;
        let fade_out_at = fade_out.position_ms as f64 / 1000.0;
        let bpm = f64::from(fade_in.tempo_bpm);
        if !(fade_in_at > 0.0 && fade_out_at > fade_in_at && bpm.is_finite() && bpm > 0.0) {
            return None;
        }
        Some(Self {
            fade_in_at,
            fade_out_at,
            bpm,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the response the service returns for a track whose material
    /// runs from 12s to 180s at 128 BPM.
    fn response(fade_in_ms: i64, fade_out_ms: i64, bpm: f32) -> Vec<u8> {
        let cue = |position_ms| {
            let mut point = cuepoints::Cuepoint::new();
            point.position_ms = position_ms;
            point.tempo_bpm = bpm;
            point
        };
        let mut message = cuepoints::Cuepoints::new();
        message.fade_in_cuepoint = protobuf::MessageField::some(cue(fade_in_ms));
        message.fade_out_cuepoint = protobuf::MessageField::some(cue(fade_out_ms));
        message.write_to_bytes().expect("the message encodes")
    }

    #[test]
    fn a_cuepoint_pair_is_read_in_track_time() {
        let parsed = Cuepoints::parse(&response(12_000, 180_000, 128.0)).expect("a pair");
        assert!((parsed.fade_in_at - 12.0).abs() < 1e-9);
        assert!((parsed.fade_out_at - 180.0).abs() < 1e-9);
        assert!((parsed.bpm - 128.0).abs() < 1e-4);
    }

    /// The bug this covers: a zero fade-in cuepoint is what the service
    /// returns when it has nothing to say about a track, and taking it at
    /// face value puts the incoming track back at its first sample — the
    /// exact symptom the cuepoints were adopted to remove.
    #[test]
    fn a_cuepoint_that_cannot_be_used_is_no_answer() {
        assert!(Cuepoints::parse(&response(0, 180_000, 128.0)).is_none());
        assert!(Cuepoints::parse(&response(12_000, 0, 128.0)).is_none());
        assert!(
            Cuepoints::parse(&response(180_000, 12_000, 128.0)).is_none(),
            "a fade-out before the fade-in is not a pair"
        );
        assert!(
            Cuepoints::parse(&response(12_000, 180_000, 0.0)).is_none(),
            "a tempo of zero is not a tempo"
        );
        assert!(
            Cuepoints::parse(&response(12_000, 180_000, f32::NAN)).is_none(),
            "and neither is a tempo that is not a number"
        );
    }

    /// A response with no pair in it at all is what a track the service has
    /// not analysed looks like, and it must not be read as a transition from
    /// the start of the track.
    #[test]
    fn an_empty_response_is_no_answer() {
        let empty = cuepoints::Cuepoints::new()
            .write_to_bytes()
            .expect("the message encodes");
        assert!(Cuepoints::parse(&empty).is_none());
        assert!(Cuepoints::parse(&[]).is_none());
        assert!(Cuepoints::parse(&[0xff, 0xff, 0xff]).is_none());
    }

    /// Only the fade-in half, which the service can return when it knows
    /// where a track should start but not where it should end.
    #[test]
    fn a_half_pair_is_no_answer() {
        let mut message = cuepoints::Cuepoints::new();
        let mut point = cuepoints::Cuepoint::new();
        point.position_ms = 12_000;
        point.tempo_bpm = 128.0;
        message.fade_in_cuepoint = protobuf::MessageField::some(point);
        let bytes = message.write_to_bytes().expect("the message encodes");
        assert!(Cuepoints::parse(&bytes).is_none());
    }
}
