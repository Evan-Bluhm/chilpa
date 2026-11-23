//! Reports against answers in the shape Navidrome 0.64.2 gives `scrobble`:
//! an empty `ok`, or error 70 for an id it does not have.

use super::*;
use crate::model::Client;
use crate::net::testing::Canned;

fn client() -> Client {
    Client { version: "0.0.0".into(), platform: "Linux".into(), device: "desk".into(), id: "c0ffee".into() }
}

fn subsonic() -> (Server, Credential) {
    (subsonic::server("http://music.lan:4533", client(), Vec::new(), Vec::new()), Credential::Password { user: "lab".into(), password: "lab-password".into() })
}

fn query(url: &str, name: &str) -> Option<String> {
    url.split('?').nth(1)?.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}=")).map(str::to_string))
}

const OK: &[u8] = br#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome"}}"#;

#[test]
fn only_subsonic_is_told_what_plays() {
    assert!(Kind::Subsonic.reports_plays());
    assert!(!Kind::Jellyfin.reports_plays());
    assert!(!Kind::Plex.reports_plays());
    assert!(!Kind::Dlna.reports_plays());
}

#[test]
fn what_plays_now_is_not_a_submission() {
    let (server, key) = subsonic();
    let net = Canned::new().answer(200, OK);
    report(&net, &server, &key, "47gLTJvR49ADB1Pa0vPhB5", Report::Playing).unwrap();
    let asked = net.asked();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].url.starts_with("http://music.lan:4533/rest/scrobble.view?"));
    assert_eq!(query(&asked[0].url, "id").as_deref(), Some("47gLTJvR49ADB1Pa0vPhB5"));
    assert_eq!(query(&asked[0].url, "submission").as_deref(), Some("false"));
    assert_eq!(query(&asked[0].url, "time"), None, "what plays now is now");
    assert!(query(&asked[0].url, "t").is_some(), "signed like every other request");
}

#[test]
fn a_play_is_a_submission_filed_when_it_was_heard_in_milliseconds() {
    let (server, key) = subsonic();
    let net = Canned::new().answer(200, OK);
    report(&net, &server, &key, "a b/c", Report::Played { at: 1_700_000_000 }).unwrap();
    let url = &net.asked()[0].url;
    assert_eq!(query(url, "submission").as_deref(), Some("true"));
    assert_eq!(query(url, "time").as_deref(), Some("1700000000000"));
    assert_eq!(query(url, "id").as_deref(), Some("a%20b%2Fc"), "the id is encoded");
}

#[test]
fn an_id_the_server_no_longer_has_is_gone_and_no_network_is_unreachable() {
    let (server, key) = subsonic();
    let net = Canned::new().answer(200, br#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"Song not found"}}}"#);
    assert!(matches!(report(&net, &server, &key, "gone", Report::Played { at: 1 }), Err(Failure::Gone(_))));
    let net = Canned::new().answer(200, br#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#);
    assert!(matches!(report(&net, &server, &key, "x", Report::Played { at: 1 }), Err(Failure::Refused(_))));
    let net = Canned::new().silence();
    assert!(matches!(report(&net, &server, &key, "x", Report::Played { at: 1 }), Err(Failure::Unreachable(_))));
}

#[test]
fn the_other_kinds_are_sent_nothing() {
    let (mut server, key) = subsonic();
    for kind in [Kind::Jellyfin, Kind::Plex, Kind::Dlna] {
        server.kind = kind;
        let net = Canned::new();
        assert!(report(&net, &server, &key, "x", Report::Playing).is_err());
        assert!(net.asked().is_empty());
    }
}
