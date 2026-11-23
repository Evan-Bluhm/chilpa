//! What the listener plays, told to the server the track came from: what is
//! playing now, and each play that counted, so the server's own play count
//! and history keep up with Chilpa.
//!
//! What each protocol says, and this follows:
//!
//! - **OpenSubsonic** takes both through `scrobble`. `submission=false` is
//!   what is playing now: Navidrome shows it under its "now playing" and
//!   counts nothing. `submission=true` is a play: Navidrome adds one to the
//!   song's, its record's and its artist's play counts, sets when they were
//!   last played, and passes it on to the Last.fm and ListenBrainz accounts
//!   the user linked there. `time` is when it was listened to, in
//!   milliseconds since the epoch, so a play sent late is filed when it
//!   happened. An id the server does not have is error 70.
//! - **Jellyfin** and **Plex** have their own ways of hearing what plays
//!   (playback sessions, `/:/scrobble`). Nothing is sent to them yet.
//! - **A media server** keeps no plays.

use std::time::Duration;

use crate::model::{Credential, Failure, Kind, Server};
use crate::net::{Net, Request};
use crate::subsonic;
use crate::text::encode;

/// How long a server has to take one report.
const WRITE_TIME: Duration = Duration::from_secs(15);

impl Kind {
    /// Whether Chilpa tells it what is played.
    pub fn reports_plays(self) -> bool {
        self == Kind::Subsonic
    }
}

/// What is told of a track.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
    /// It started playing: shown on the server, counted nowhere.
    Playing,
    /// It counted as a play, listened to at this moment, in unix seconds.
    Played { at: i64 },
}

/// Tells the server one thing about its track `id`.
pub fn report(net: &dyn Net, server: &Server, key: &Credential, id: &str, report: Report) -> Result<(), Failure> {
    match server.kind {
        Kind::Subsonic => {
            let extra = match report {
                Report::Playing => format!("&id={}&submission=false", encode(id)),
                Report::Played { at } => format!("&id={}&time={}&submission=true", encode(id), at.max(0).saturating_mul(1000)),
            };
            let mut request = Request::get(subsonic::url(&server.address, "scrobble", Some(key), &extra), Vec::new());
            request.timeout = WRITE_TIME;
            subsonic::send(net, request, refused).map(|_| ())
        }
        Kind::Jellyfin | Kind::Plex | Kind::Dlna => Err(Failure::Unexpected(chilpa_i18n::t("this server keeps no plays Chilpa can report"))),
    }
}

/// A Subsonic error to a report: an id the server no longer has is gone.
fn refused(error: &serde_json::Value) -> Failure {
    match error.get("code").and_then(serde_json::Value::as_i64) {
        Some(70) => Failure::Gone(chilpa_i18n::t("the server no longer has it")),
        _ => subsonic::error_of(error),
    }
}

#[cfg(test)]
mod tests;
