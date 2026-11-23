//! What is played, told to the server each track came from
//! (`chilpa_servers::plays`), on a thread of its own so the window never
//! waits on a server. Turned off for every server from Settings › Library.
//!
//! What is playing now goes once and is never kept: by the time it could
//! go again it is not playing any more. A play that counted (the rule every
//! scrobbling service has, `crate::scrobble::counts`) is kept on disk until
//! its server takes it, and goes with the moment it was heard, so a server
//! that was off, or a track heard offline from its copy, is still counted
//! when it happened. A server that fails is left alone a while, longer each
//! time, and the others go on. A play whose track or server is gone is
//! dropped.
//!
//! What is kept names the track by its server's instance and id, never by
//! an address or a credential: both are looked up when it goes.

use super::keys::Keys;
use super::net::{self, Network};
use crate::ratings::writer::Delivery;
use crate::Chilpa;
use chilpa_models::RemoteEntry;
use chilpa_servers::plays::Report;
use chilpa_servers::{Failure, Net};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Settings › Library: plays go to the servers they came from. On unless
/// turned off.
pub(crate) const REPORT: &str = "library.reportPlays";

/// The most plays kept waiting. Past it the oldest go.
const KEPT: usize = 20_000;
/// How long a server is left alone after its first failure, and at most.
const FIRST_WAIT: Duration = Duration::from_secs(60);
const MOST_WAIT: Duration = Duration::from_secs(30 * 60);
/// How often the thread wakes on its own to send what waits.
const TICK: Duration = Duration::from_secs(5 * 60);

/// Whether plays go to the servers at all (`REPORT`).
pub(crate) fn on() -> bool {
    chilpa_core::settings::load().window.get(REPORT).is_none_or(|value| value != "false")
}

/// One play that counted, waiting for its server.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Played {
    /// The server's instance, as the library keeps it.
    pub(crate) instance: String,
    /// The track's id on the server.
    pub(crate) id: String,
    /// When it started, in unix seconds.
    pub(crate) at: i64,
}

pub(crate) enum Order {
    Playing { instance: String, id: String },
    Played(Played),
}

/// What the thread is handed to send with, which a test fakes.
pub(crate) trait Wire: Send {
    fn send(&self, instance: &str, id: &str, report: Report) -> Delivery;
    /// Whether the network may be used now (Settings › Privacy).
    fn allowed(&self) -> bool;
}

/// The running app's: the library for where each server is, its sealed
/// credential, the network.
struct Running {
    keys: Arc<Keys>,
}

impl Wire for Running {
    fn send(&self, instance: &str, id: &str, report: Report) -> Delivery {
        let Some(lib) = super::sync::open_library() else {
            return Delivery::Later(chilpa_i18n::t("the library could not be opened"));
        };
        deliver(&lib, &Network, &self.keys, instance, id, report)
    }

    fn allowed(&self) -> bool {
        !chilpa_core::network::current().offline
    }
}

/// Sends one report to the server of that instance.
pub(crate) fn deliver(lib: &chilpa_library::Library, network: &dyn Net, keys: &Keys, instance: &str, id: &str, report: Report) -> Delivery {
    let Some(stored) = lib.servers().ok().and_then(|all| all.into_iter().find(|s| s.instance == instance)) else {
        return Delivery::Never(chilpa_i18n::t("that server is no longer in the library"));
    };
    if !chilpa_servers::Kind::from_provider(&stored.provider).is_some_and(chilpa_servers::Kind::reports_plays) {
        return Delivery::Never(chilpa_i18n::t("this server keeps no plays Chilpa can report"));
    }
    if !stored.enabled {
        return Delivery::Later(chilpa_i18n::t("that server is disabled"));
    }
    let (Some(server), Some(key)) = (net::server(&stored), net::key(keys, &stored)) else {
        return Delivery::Later(chilpa_i18n::t_args("sign in to {} again", &[&stored.label]));
    };
    match chilpa_servers::plays::report(network, &server, &key, id, report) {
        Ok(()) => Delivery::Sent(1.0),
        Err(Failure::Gone(why)) => Delivery::Never(why),
        Err(failure) => Delivery::Later(failure.to_string()),
    }
}

/// A server that failed, and when it is tried again.
struct Resting {
    until: Instant,
    wait: Duration,
}

pub(crate) struct Worker {
    sender: Box<dyn Wire>,
    file: PathBuf,
    pending: Vec<Played>,
    resting: HashMap<String, Resting>,
}

impl Worker {
    pub(crate) fn open(sender: Box<dyn Wire>, file: PathBuf) -> Worker {
        let pending = std::fs::read(&file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        Worker { sender, file, pending, resting: HashMap::new() }
    }

    fn run(mut self, inbox: Receiver<Order>) {
        // Whatever an earlier run left waiting goes first.
        self.flush(Instant::now());
        loop {
            match inbox.recv_timeout(TICK) {
                Ok(order) => self.handle(order, Instant::now()),
                Err(RecvTimeoutError::Timeout) => self.flush(Instant::now()),
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    pub(crate) fn handle(&mut self, order: Order, now: Instant) {
        match order {
            Order::Playing { instance, id } => {
                // Not when its server rests: it would only fail again.
                if self.sender.allowed() && !self.rests(&instance, now) {
                    if let Delivery::Later(why) = self.sender.send(&instance, &id, Report::Playing) {
                        eprintln!("chilpa-servers: what plays now did not reach {instance}: {why}");
                    }
                }
            }
            Order::Played(played) => {
                self.pending.push(played);
                if self.pending.len() > KEPT {
                    let over = self.pending.len() - KEPT;
                    self.pending.drain(..over);
                }
                self.save();
                self.flush(now);
            }
        }
    }

    fn rests(&self, instance: &str, now: Instant) -> bool {
        self.resting.get(instance).is_some_and(|r| now < r.until)
    }

    /// Sends what waits, oldest first. A server that fails is left alone
    /// with the rest of its plays; the other servers' go on.
    pub(crate) fn flush(&mut self, now: Instant) {
        if self.pending.is_empty() || !self.sender.allowed() {
            return;
        }
        let before = self.pending.len();
        let mut kept = Vec::with_capacity(self.pending.len());
        for played in std::mem::take(&mut self.pending) {
            if self.rests(&played.instance, now) {
                kept.push(played);
                continue;
            }
            match self.sender.send(&played.instance, &played.id, Report::Played { at: played.at }) {
                Delivery::Sent(_) => {
                    self.resting.remove(&played.instance);
                }
                Delivery::Never(why) => eprintln!("chilpa-servers: a play for {} was dropped: {why}", played.instance),
                Delivery::Later(why) => {
                    let wait = self.resting.get(&played.instance).map_or(FIRST_WAIT, |r| (r.wait * 2).min(MOST_WAIT));
                    eprintln!("chilpa-servers: a play did not reach {} ({why}); tried again in {}s", played.instance, wait.as_secs());
                    self.resting.insert(played.instance.clone(), Resting { until: now + wait, wait });
                    kept.push(played);
                }
            }
        }
        self.pending = kept;
        if self.pending.len() != before {
            self.save();
        }
    }

    #[cfg(test)]
    fn pending(&self) -> &[Played] {
        &self.pending
    }

    /// Written beside and moved over, so a crash mid write leaves the last
    /// good file.
    fn save(&self) {
        let Ok(bytes) = serde_json::to_vec(&self.pending) else { return };
        if let Some(dir) = self.file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let next = self.file.with_extension("json.new");
        if std::fs::write(&next, bytes).is_ok() {
            let _ = std::fs::rename(&next, &self.file);
        }
    }
}

/// The thread, from the window's side.
pub(crate) struct Reporter {
    orders: Sender<Order>,
}

impl Reporter {
    pub(crate) fn start(keys: Arc<Keys>, dir: &Path) -> Reporter {
        let (orders, inbox) = channel::<Order>();
        let file = dir.join("plays.json");
        let spawned = std::thread::Builder::new()
            .name("chilpa-server-plays".into())
            .spawn(move || Worker::open(Box::new(Running { keys }), file).run(inbox));
        if let Err(error) = spawned {
            eprintln!("chilpa-servers: the plays thread could not start: {error}");
        }
        Reporter { orders }
    }

    fn send(&self, order: Order) {
        let _ = self.orders.send(order);
    }
}

/// The server's instance and the track's id there, for a queue entry of a
/// server whose kind is told what plays.
fn on_a_server(path: &Path) -> Option<(String, String)> {
    let entry = RemoteEntry::parse(&path.to_string_lossy())?;
    chilpa_servers::Kind::from_provider(&entry.source.provider).filter(|kind| kind.reports_plays())?;
    Some((entry.source.instance, entry.external_id))
}

/// Tells the track's server it is playing now. Nothing for a track that is
/// not on a server, or when plays are not reported.
pub(crate) fn playing(reporter: Option<&Reporter>, path: &Path) {
    let (Some(reporter), Some((instance, id))) = (reporter, on_a_server(path)) else { return };
    if on() {
        reporter.send(Order::Playing { instance, id });
    }
}

/// Tells the track's server it was played, from `at`.
pub(crate) fn played(reporter: Option<&Reporter>, path: &Path, at: i64) {
    let (Some(reporter), Some((instance, id))) = (reporter, on_a_server(path)) else { return };
    if on() {
        reporter.send(Order::Played(Played { instance, id, at }));
    }
}

impl Chilpa {
    /// The switch in Settings › Library.
    pub(crate) fn server_plays_setting(&mut self, key: &str, value: &str) -> bool {
        if key != REPORT || !matches!(value, "true" | "false") {
            return false;
        }
        self.remember_preference_checked(key, value).is_ok()
    }

    /// The switch in Settings › Library, as its row shows it.
    pub(crate) fn server_plays_row(&self, row: &mut crate::SettingsRow) -> bool {
        if row.key != REPORT {
            return false;
        }
        row.value = on().to_string();
        true
    }
}

#[cfg(test)]
mod tests;
