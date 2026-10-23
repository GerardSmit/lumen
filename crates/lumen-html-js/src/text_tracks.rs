//! `TextTrack`, `TextTrackList`, `TextTrackCue`, `VTTCue`, `TrackEvent` and the media element
//! text-track members. The script-visible model is implemented in `text_tracks.js` and installed
//! into each window realm.
use lumen::embed::Ctx;

const SOURCE: &str = include_str!("text_tracks.js");

pub(crate) fn install(ctx: &mut Ctx) {
    let global = ctx.global_object();
    let _ = ctx.eval_in_realm(&global, SOURCE);
}
