//! Plays on their way to the servers: against a lab server in the library
//! and a network that answers what it was loaded with, and against a wire
//! the test answers for. Nothing here leaves the machine.

use super::super::ratings::tests::{ok, plex, subsonic, Answers};
use super::*;
use std::sync::Mutex;

fn query(url: &str, name: &str) -> Option<String> {
    url.split('?').nth(1)?.split('&').find_map(|pair| pair.strip_prefix(&format!("{name}=")).map(str::to_string))
}

#[test]
fn a_play_goes_to_its_subsonic_server_with_when_it_was_heard() {
    let lab = subsonic();
    let net = Answers::default().status(200, ok());
    assert_eq!(deliver(&lab.lib, &net, &lab.keys, &lab.instance, "song-1", Report::Played { at: 1_700_000_000 }), Delivery::Sent(1.0));
    let asked = net.asked();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].1.starts_with("http://lab.lan/rest/scrobble.view?"));
    assert_eq!(query(&asked[0].1, "id").as_deref(), Some("song-1"));
    assert_eq!(query(&asked[0].1, "submission").as_deref(), Some("true"));
    assert_eq!(query(&asked[0].1, "time").as_deref(), Some("1700000000000"));
}

#[test]
fn a_server_that_is_gone_or_not_told_plays_is_never_asked() {
    let lab = subsonic();
    let net = Answers::default();
    assert!(matches!(deliver(&lab.lib, &net, &lab.keys, "no-such-instance", "x", Report::Playing), Delivery::Never(_)));
    let lab = plex();
    assert!(matches!(deliver(&lab.lib, &net, &lab.keys, &lab.instance, "x", Report::Playing), Delivery::Never(_)));
    assert!(net.asked().is_empty());
}

#[test]
fn an_unreachable_server_is_later_and_a_track_it_lost_is_never() {
    let lab = subsonic();
    let net = Answers::default().silence();
    assert!(matches!(deliver(&lab.lib, &net, &lab.keys, &lab.instance, "x", Report::Played { at: 1 }), Delivery::Later(_)));
    let net = Answers::default().status(200, r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":70,"message":"not found"}}}"#);
    assert!(matches!(deliver(&lab.lib, &net, &lab.keys, &lab.instance, "x", Report::Played { at: 1 }), Delivery::Never(_)));
}

#[test]
fn only_a_track_on_a_server_that_is_told_plays_is_named() {
    let entry = |provider: &str| RemoteEntry::new(chilpa_models::SourceId::new(provider, "inst-1"), "id/1").to_entry();
    assert_eq!(on_a_server(Path::new(&entry("subsonic"))), Some(("inst-1".to_string(), "id/1".to_string())));
    assert_eq!(on_a_server(Path::new(&entry("plex"))), None);
    assert_eq!(on_a_server(Path::new("/music/song.flac")), None);
}

/// What the test's wire was asked, and how it answers each instance.
#[derive(Clone, Default)]
struct Fake {
    asked: Arc<Mutex<Vec<(String, String, Report)>>>,
    answers: Arc<Mutex<HashMap<String, Delivery>>>,
    offline: Arc<Mutex<bool>>,
}

impl Fake {
    fn answer(&self, instance: &str, delivery: Delivery) {
        self.answers.lock().unwrap().insert(instance.into(), delivery);
    }
    fn asked(&self) -> Vec<(String, String, Report)> {
        std::mem::take(&mut self.asked.lock().unwrap())
    }
}

impl Wire for Fake {
    fn send(&self, instance: &str, id: &str, report: Report) -> Delivery {
        self.asked.lock().unwrap().push((instance.into(), id.into(), report));
        self.answers.lock().unwrap().get(instance).cloned().unwrap_or(Delivery::Sent(1.0))
    }
    fn allowed(&self) -> bool {
        !*self.offline.lock().unwrap()
    }
}

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir() -> Dir {
    Dir(std::env::temp_dir().join(format!("chilpa-plays-{}", super::super::keys::mint())))
}

fn play(instance: &str, id: &str, at: i64) -> Order {
    Order::Played(Played { instance: instance.into(), id: id.into(), at })
}

#[test]
fn a_play_that_goes_is_not_kept_and_what_plays_now_never_is() {
    let dir = dir();
    let fake = Fake::default();
    let mut worker = Worker::open(Box::new(fake.clone()), dir.0.join("plays.json"));
    let now = Instant::now();
    worker.handle(Order::Playing { instance: "a".into(), id: "1".into() }, now);
    worker.handle(play("a", "1", 100), now);
    assert_eq!(fake.asked(), [("a".to_string(), "1".to_string(), Report::Playing), ("a".to_string(), "1".to_string(), Report::Played { at: 100 })]);
    assert!(worker.pending().is_empty());
    fake.answer("a", Delivery::Later("down".into()));
    worker.handle(Order::Playing { instance: "a".into(), id: "2".into() }, now);
    assert!(worker.pending().is_empty(), "what plays now is not kept when it fails");
}

#[test]
fn a_server_that_fails_keeps_its_plays_and_rests_while_the_others_go() {
    let dir = dir();
    let fake = Fake::default();
    let mut worker = Worker::open(Box::new(fake.clone()), dir.0.join("plays.json"));
    let now = Instant::now();
    fake.answer("down", Delivery::Later("unreachable".into()));
    worker.handle(play("down", "1", 100), now);
    worker.handle(play("down", "2", 200), now);
    worker.handle(play("up", "3", 300), now);
    let asked: Vec<String> = fake.asked().into_iter().map(|(i, id, _)| format!("{i}/{id}")).collect();
    assert_eq!(asked, ["down/1", "up/3"], "a resting server is not asked again until its wait is over");
    assert_eq!(worker.pending().iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["1", "2"]);
    // Within its wait nothing is asked; after it, both go, oldest first.
    worker.flush(now + Duration::from_secs(30));
    assert!(fake.asked().is_empty());
    fake.answer("down", Delivery::Sent(1.0));
    worker.flush(now + FIRST_WAIT + Duration::from_secs(1));
    let asked: Vec<(String, Report)> = fake.asked().into_iter().map(|(_, id, r)| (id, r)).collect();
    assert_eq!(asked, [("1".to_string(), Report::Played { at: 100 }), ("2".to_string(), Report::Played { at: 200 })]);
    assert!(worker.pending().is_empty());
}

#[test]
fn each_failure_waits_longer_up_to_half_an_hour() {
    let dir = dir();
    let fake = Fake::default();
    let mut worker = Worker::open(Box::new(fake.clone()), dir.0.join("plays.json"));
    fake.answer("down", Delivery::Later("unreachable".into()));
    let mut now = Instant::now();
    worker.handle(play("down", "1", 100), now);
    let mut waits = Vec::new();
    for _ in 0..8 {
        let wait = worker.resting["down"].wait;
        waits.push(wait.as_secs());
        now += wait;
        worker.flush(now);
    }
    assert_eq!(waits, [60, 120, 240, 480, 960, 1800, 1800, 1800]);
}

#[test]
fn a_play_whose_track_is_gone_is_dropped() {
    let dir = dir();
    let fake = Fake::default();
    let mut worker = Worker::open(Box::new(fake.clone()), dir.0.join("plays.json"));
    fake.answer("a", Delivery::Never("gone".into()));
    worker.handle(play("a", "1", 100), Instant::now());
    assert!(worker.pending().is_empty());
}

#[test]
fn offline_nothing_is_sent_and_what_waits_survives_a_restart() {
    let dir = dir();
    let file = dir.0.join("plays.json");
    let fake = Fake::default();
    *fake.offline.lock().unwrap() = true;
    let mut worker = Worker::open(Box::new(fake.clone()), file.clone());
    let now = Instant::now();
    worker.handle(Order::Playing { instance: "a".into(), id: "1".into() }, now);
    worker.handle(play("a", "1", 100), now);
    worker.handle(play("a", "2", 200), now);
    assert!(fake.asked().is_empty(), "offline, the network is not used");
    drop(worker);
    // A write that died before the move leaves the last good file.
    std::fs::write(file.with_extension("json.new"), b"[{half").unwrap();
    *fake.offline.lock().unwrap() = false;
    let mut worker = Worker::open(Box::new(fake.clone()), file);
    assert_eq!(worker.pending().len(), 2);
    worker.flush(now);
    assert_eq!(fake.asked().len(), 2);
    assert!(worker.pending().is_empty());
}
