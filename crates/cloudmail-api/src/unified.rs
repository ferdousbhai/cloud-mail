//! Your worker plus any linked accounts, as one mailbox.
//!
//! The worker is authoritative: its errors fail an operation as they always have. A linked
//! account's errors never do; they come back as warnings next to whatever the worker returned.
//!
//! The Screener for accounts without one (Gmail, iCloud Mail; `Provider::screened_by_cloudmail`) is
//! Cloudmail's, with the decisions kept in each account itself (Gmail filters, iCloud Mail rules;
//! see `screening.rs`), so every computer and the account's own apps go by them, worker or not. A
//! decision is recorded in every screened account and in the worker (whose Screener decides its
//! own mail as it arrives): one decision per address, wherever the mail comes. A thread whose sender (the
//! latest one not you) has no decision waits in the Screener and is left out of the Inbox; an
//! approved sender's is in the Inbox; a blocked sender's is left out of the Inbox, everything and
//! search. Saying no also archives that sender's Inbox threads in the account itself, so they
//! leave its own Inbox (on the phone too); later mail from them stays in the account's Inbox but
//! never shows here. People you write to through an account are screened in, as the worker does
//! for its own mail, and linking an account screens in the people it already corresponds with.
//!
//! Duplicates: a worker that forwards to a linked account (`forward_to` = your HEY address), or
//! an account that forwards into your worker, puts every message in both; the account's copy is
//! hidden. Where the account exposes Message-IDs (Gmail), a thread is a copy when one of its
//! messages has the Message-ID of a message in a worker thread with the same subject. Otherwise
//! (HEY's CLI exposes none) it is a copy when the sender's address and the subject (ignoring
//! Re:/Fwd:) match and the latest activity is within `DUPLICATE_WINDOW_MS`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::client::{Client, ThreadQuery};
use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::provider::{self, AccountWarning, ExtraFolder, Provider};
use crate::screening::{APPROVED, BLOCKED, Lists};
use crate::types::*;

pub const DUPLICATE_WINDOW_MS: i64 = 15 * 60 * 1000;
/// How many Inbox threads of a worker-screened account the Screener looks through.
const SCREEN_SCAN: u32 = 200;

/// Where a worker-screened account's thread belongs, by its sender's decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screening {
    Approved,
    Blocked,
    Waiting,
}

/// A decision as the worker records it ("approved", "blocked", "pending" or none).
pub fn screening(status: Option<&str>) -> Screening {
    match status {
        Some("approved") => Screening::Approved,
        Some("blocked") => Screening::Blocked,
        _ => Screening::Waiting,
    }
}

/// Whether a worker-screened account's thread shows in a folder ("screener" for the waiting).
pub fn shows_in(folder: &str, s: Screening) -> bool {
    match folder {
        "inbox" => s == Screening::Approved,
        "screener" => s == Screening::Waiting,
        // Archived and sent mail is yours; a blocked sender's stays out of everything else.
        "archive" | "sent" => true,
        _ => s != Screening::Blocked,
    }
}

/// A worker thread's `last_at` when read, and its messages' Message-IDs.
type WorkerIds = (i64, Vec<String>);

/// Every screened account's decisions together, and the accounts whose lists couldn't be read.
type Decisions = (HashMap<String, &'static str>, HashMap<String, Error>);

#[derive(Clone)]
pub struct Mail {
    /// The worker, if there is one; without it only linked accounts are shown (still screened: their
    /// decisions live in the accounts).
    pub client: Option<Client>,
    pub accounts: Vec<Arc<dyn Provider>>,
    /// Configured accounts that couldn't be opened (unknown provider, …).
    pub broken: Vec<AccountWarning>,
    /// Worker threads' Message-IDs, by thread ID, with the `last_at` they were read at.
    worker_ids: Arc<Mutex<HashMap<String, WorkerIds>>>,
}

/// One sender's Screener decisions, wherever they are kept (`Mail::decided_senders`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Decided {
    pub email: String,
    pub name: String,
    /// What Cloudmail does with their mail: blocked when any place blocks them, as screening does.
    pub status: String,
    /// When the worker recorded it (Gmail's filters and iCloud's rules keep no time).
    pub decided_at: Option<i64>,
    /// Each place's decision: `worker`, or a linked account's name.
    #[serde(rename = "in")]
    pub places: BTreeMap<String, String>,
    /// Whether the places differ, or some place that keeps decisions has none for them.
    pub disagrees: bool,
}

impl Decided {
    /// Works out `status` and `disagrees` from `places` (each place's decision) and the places
    /// that keep decisions.
    fn settle(&mut self, keepers: &[String]) {
        let blocked = self.places.values().any(|s| s == BLOCKED);
        self.status = if blocked { BLOCKED } else { APPROVED }.into();
        let first = self.places.values().next();
        self.disagrees =
            keepers.iter().any(|k| !self.places.contains_key(k)) || self.places.values().any(|s| Some(s) != first);
    }
}

#[derive(Debug, Clone, Default)]
pub struct Listing {
    pub threads: Vec<ThreadSummary>,
    pub warnings: Vec<AccountWarning>,
    /// Linked-account copies of worker threads that were left out.
    pub duplicates_hidden: usize,
    /// The worker returned a full page, so there may be more.
    pub more: bool,
}

/// Subject without reply/forward prefixes, for matching copies.
pub fn subject_key(subject: &str) -> String {
    let mut s = subject.trim();
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(p) = ["re:", "fwd:", "fw:", "aw:", "sv:"]
            .iter()
            .find(|p| lower.starts_with(**p))
        else {
            break;
        };
        s = s[p.len()..].trim_start();
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Whether `copy` (from a linked account) is the same conversation as the worker's `original`.
pub fn is_copy(copy: &ThreadSummary, original: &ThreadSummary) -> bool {
    let sender = |t: &ThreadSummary| {
        t.from
            .as_ref()
            .map(|a| a.email.to_ascii_lowercase())
            .filter(|e| !e.is_empty())
    };
    sender(copy).is_some()
        && sender(copy) == sender(original)
        && subject_key(&copy.subject) == subject_key(&original.subject)
        && (copy.last_at - original.last_at).abs() <= DUPLICATE_WINDOW_MS
}

/// The worker's waiting senders followed by the accounts' that aren't also waiting in the worker.
pub fn merge_senders(mut worker: Vec<PendingSender>, accounts: Vec<PendingSender>) -> Vec<PendingSender> {
    for s in accounts {
        if !worker.iter().any(|w| w.email.eq_ignore_ascii_case(&s.email)) {
            worker.push(s);
        }
    }
    worker
}

/// The senders of threads waiting in the Screener, one entry each, newest first.
pub fn waiting_senders(threads: &[ThreadSummary]) -> Vec<PendingSender> {
    let mut out: Vec<PendingSender> = Vec::new();
    let mut by_email = HashMap::<String, usize>::new();
    for t in threads {
        let Some(from) = t.from.as_ref().filter(|a| a.email.contains('@')) else {
            continue;
        };
        let email = from.email.to_ascii_lowercase();
        match by_email.get(&email).copied() {
            Some(index) => {
                let s = &mut out[index];
                s.thread_count += 1;
                if t.last_at > s.last_at {
                    s.last_at = t.last_at;
                    s.last_subject = Some(t.subject.clone());
                }
            }
            None => {
                by_email.insert(email.clone(), out.len());
                out.push(PendingSender {
                    email,
                    name: from.name.clone(),
                    thread_count: 1,
                    last_subject: Some(t.subject.clone()),
                    last_at: t.last_at,
                    account: t.account.clone(),
                    id: None,
                });
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.last_at));
    out
}

/// Newest first, at most `limit`.
pub fn merge(mut a: Vec<ThreadSummary>, b: Vec<ThreadSummary>, limit: u32) -> Vec<ThreadSummary> {
    a.extend(b);
    a.sort_by_key(|t| std::cmp::Reverse(t.last_at));
    a.truncate(limit.max(1) as usize);
    a
}

impl Mail {
    pub fn new(client: Option<Client>, config: &Config) -> Self {
        let mut accounts = Vec::new();
        let mut broken = Vec::new();
        for (name, cfg) in &config.accounts {
            match provider::open(name, cfg) {
                Ok(p) => accounts.push(p),
                Err(e) => broken.push(AccountWarning::new(name, &e)),
            }
        }
        Self {
            client,
            accounts,
            broken,
            worker_ids: Default::default(),
        }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::new(config.has_worker().then(|| Client::new(config)), config)
    }

    /// The worker, or an error saying what this needs when there's none.
    pub fn worker(&self) -> Result<&Client> {
        self.client.as_ref().ok_or_else(no_worker)
    }

    pub fn has_accounts(&self) -> bool {
        !self.accounts.is_empty()
    }

    /// The linked account that owns an ID, or None for the worker's own.
    pub fn account_for(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.accounts.iter().find(|p| p.owns(id))
    }

    /// Who handles an ID: its linked account, else the worker.
    pub fn provider(&self, id: &str) -> Result<&dyn Provider> {
        match self.account_for(id) {
            Some(p) => Ok(p.as_ref()),
            None => Ok(self.worker()?),
        }
    }

    /// Extra folders the linked accounts have, each once.
    pub fn extra_folders(&self) -> Vec<ExtraFolder> {
        let mut out: Vec<ExtraFolder> = Vec::new();
        for f in self.accounts.iter().flat_map(|p| p.extra_folders().iter().copied()) {
            if !out.iter().any(|o| o.folder == f.folder) {
                out.push(f);
            }
        }
        out
    }

    /// Whether any source (the worker or an account) has this folder.
    pub fn knows_folder(&self, folder: &str) -> bool {
        provider::CORE_FOLDERS.contains(&folder) || self.accounts.iter().any(|p| p.has_folder(folder))
    }

    /// Threads in a folder across the worker and every account, newest first.
    pub fn list(&self, q: &ThreadQuery) -> Result<Listing> {
        let folder = if q.folder.is_empty() {
            "inbox"
        } else {
            q.folder.as_str()
        };
        let worker_folder = provider::CORE_FOLDERS.contains(&folder);
        if !self.has_accounts() {
            let threads = if worker_folder {
                self.worker()?.list_threads(q)?
            } else {
                Vec::new()
            };
            let more = threads.len() as u32 >= q.limit.max(1);
            return Ok(Listing {
                threads,
                more,
                warnings: self.broken.clone(),
                ..Default::default()
            });
        }
        let (worker, (extra, mut warnings, hidden)) = std::thread::scope(|s| {
            let extra = s.spawn(|| self.accounts_list(q));
            let worker = match &self.client {
                Some(c) if worker_folder => c.list_threads(q),
                _ => Ok(Vec::new()),
            };
            (worker, extra.join().unwrap_or_default())
        });
        let worker = worker?;
        let more = self.client.is_some() && worker.len() as u32 >= q.limit.max(1);
        warnings.extend(self.broken.iter().cloned());
        Ok(Listing {
            threads: merge(worker, extra, q.limit),
            warnings,
            duplicates_hidden: hidden,
            more,
        })
    }

    /// Linked accounts' threads in a folder, with copies of worker mail left out and, for the
    /// accounts your worker screens, only what that folder shows of them.
    pub fn accounts_list(&self, q: &ThreadQuery) -> (Vec<ThreadSummary>, Vec<AccountWarning>, usize) {
        let folder = if q.folder.is_empty() {
            "inbox"
        } else {
            q.folder.as_str()
        };
        let (results, decisions) = self.with_decisions(folder, || {
            self.each_account(|p| {
                if p.screened_by_cloudmail() {
                    // An account without a Screener has its waiting mail in its Inbox.
                    let (inner, limit) = if folder == "screener" {
                        ("inbox", SCREEN_SCAN)
                    } else {
                        (folder, q.limit)
                    };
                    if !p.has_folder(inner) {
                        return Ok(Vec::new());
                    }
                    p.threads(&ThreadQuery {
                        folder: inner.into(),
                        limit,
                        ..q.clone()
                    })
                } else if p.has_folder(folder) {
                    p.threads(q)
                } else {
                    Ok(Vec::new())
                }
            })
        });
        let results = self.screen(results, folder, decisions);
        // The Screener too: a copy of worker mail (your own message to yourself, a forward) never
        // waits there when its folders hide it.
        self.collect(results, true)
    }

    /// Runs `fetch` (the accounts' listing) and, alongside it, reads the decisions `folder` needs:
    /// none for Archive and Sent, where everything shows.
    fn with_decisions<T: Send>(&self, folder: &str, fetch: impl FnOnce() -> T + Send) -> (T, Option<Decisions>) {
        if matches!(folder, "archive" | "sent") {
            return (fetch(), None);
        }
        std::thread::scope(|s| {
            let decisions = s.spawn(|| self.screen_decisions());
            let fetched = fetch();
            (fetched, decisions.join().ok())
        })
    }

    /// Keeps of each screened account's threads those `folder` shows, by their senders' decisions;
    /// the waiting ones are marked as in the Screener. Without decisions (Archive, Sent) all show.
    fn screen(
        &self,
        results: Vec<(String, Result<Vec<ThreadSummary>>)>,
        folder: &str,
        decisions: Option<Decisions>,
    ) -> Vec<(String, Result<Vec<ThreadSummary>>)> {
        let Some((decisions, failed)) = decisions else {
            return results;
        };
        let sender = |t: &ThreadSummary| {
            t.from
                .as_ref()
                .map(|a| a.email.to_ascii_lowercase())
                .filter(|e| e.contains('@'))
        };
        results
            .into_iter()
            .map(|(name, r)| {
                let Some(p) = self
                    .accounts
                    .iter()
                    .find(|p| p.name() == name)
                    .filter(|p| p.screened_by_cloudmail())
                else {
                    return (name, r);
                };
                let list = match (failed.get(&name), r) {
                    (_, Err(e)) => Err(e),
                    (Some(e), Ok(_)) => Err(Error::new(
                        e.kind,
                        format!(
                            "{name}: the Screener's decisions couldn't be read ({}), so its mail isn't shown",
                            e.message
                        ),
                    )),
                    (None, Ok(list)) => {
                        let mine: Vec<String> = p
                            .identities()
                            .unwrap_or_default()
                            .into_iter()
                            .map(|a| a.email.to_ascii_lowercase())
                            .collect();
                        Ok(list
                            .into_iter()
                            .filter_map(|mut t| {
                                let s = match sender(&t) {
                                    None => Screening::Approved,
                                    Some(e) if mine.contains(&e) => Screening::Approved,
                                    Some(e) => screening(decisions.get(&e).copied()),
                                };
                                shows_in(folder, s).then(|| {
                                    if folder == "screener" {
                                        t.folder = "screener".into();
                                    }
                                    t
                                })
                            })
                            .collect())
                    }
                };
                (name, list)
            })
            .collect()
    }

    /// Every screened account's decisions together (a block anywhere wins), with the accounts whose
    /// lists couldn't be read.
    fn screen_decisions(&self) -> Decisions {
        let mut decisions: HashMap<String, &'static str> = HashMap::new();
        let mut failed = HashMap::new();
        let read = self.each_account(|p| p.screened_by_cloudmail().then(|| self.lists_of(p)).transpose());
        for (name, r) in read {
            match r {
                Ok(lists) => {
                    for (e, s) in lists.iter().flat_map(Lists::decisions) {
                        if s == BLOCKED || !decisions.contains_key(&e) {
                            decisions.insert(e, s);
                        }
                    }
                }
                Err(e) => {
                    failed.insert(name, e);
                }
            }
        }
        (decisions, failed)
    }

    /// An account's lists, started the first time (see `seed`).
    fn lists_of(&self, p: &dyn Provider) -> Result<Lists> {
        let lists = p.screen_lists(false)?;
        if lists.kept {
            return Ok(lists);
        }
        p.update_screen_lists(&mut |lists| self.seed(p, lists))?;
        p.screen_lists(false)
    }

    /// Starts lists that aren't kept yet: the account's own addresses screened in, and with a
    /// worker, its decisions carried over (they decided for the account before it kept its own;
    /// anyone undecided there is waiting in the Screener and stays so). Without one there were
    /// none, so the people the account already corresponds with are screened in, as linking does.
    /// How many changed (at least one, so a start is always written).
    fn seed(&self, p: &dyn Provider, lists: &mut Lists) -> Result<usize> {
        if lists.kept {
            return Ok(0);
        }
        let own = p.identities()?;
        let mut n = lists.decide(own.iter().map(|a| a.email.as_str()), APPROVED, false);
        match &self.client {
            Some(c) => {
                for (status, s) in worker_decisions(c)? {
                    n += lists.decide([s.email.as_str()], status, true);
                }
            }
            None => {
                if let Ok(people) = p.correspondents(100) {
                    n += lists.decide(people.iter().map(|a| a.email.as_str()), APPROVED, true);
                }
            }
        }
        Ok(n.max(1))
    }

    /// Records a decision for these senders in one account Cloudmail screens, in one read and at
    /// most one write; nothing at all when its lists (as last read) say so already. How many changed.
    fn decide_in(&self, p: &dyn Provider, senders: &[String], status: &str, only_undecided: bool) -> Result<usize> {
        let emails = || senders.iter().map(String::as_str);
        if p.screen_lists(false)?.settled(emails(), status, only_undecided) {
            return Ok(0);
        }
        p.update_screen_lists(&mut |lists| Ok(self.seed(p, lists)? + lists.decide(emails(), status, only_undecided)))
    }

    /// `decide_in` every account Cloudmail screens: the most that changed in one, and the failures.
    fn decide_everywhere(&self, senders: &[String], status: &str, only_undecided: bool) -> (i64, Vec<AccountWarning>) {
        let mut changed = 0;
        let mut warnings = Vec::new();
        let results = self.each_account(|p| {
            if p.screened_by_cloudmail() {
                self.decide_in(p, senders, status, only_undecided)
            } else {
                Ok(0)
            }
        });
        for (name, r) in results {
            match r {
                Ok(n) => changed = changed.max(n as i64),
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        (changed, warnings)
    }

    /// Every sender decided on anywhere: the worker's decisions and every screened account's
    /// lists, one row each, blocked first, then the most recent; and the places consulted (`worker`
    /// and account names). An account whose lists can't be read is a warning; the worker's failure
    /// fails this. Accounts with a Screener of their own (HEY) keep their decisions to themselves.
    pub fn decided_senders(&self) -> Result<(Vec<Decided>, Vec<String>, Vec<AccountWarning>)> {
        let (worker, read) = std::thread::scope(|s| {
            let worker = s.spawn(|| self.client.as_ref().map_or(Ok(Vec::new()), worker_decisions));
            // Each account's lists, and its own addresses: they sit on the lists so your mail is
            // never screened, and aren't decisions about anyone.
            let read = self.each_account(|p| {
                if !p.screened_by_cloudmail() {
                    return Ok(None);
                }
                Ok(Some((p.screen_lists(false)?, p.identities().unwrap_or_default())))
            });
            (worker.join().unwrap_or_else(|_| Ok(Vec::new())), read)
        });
        let mut rows: BTreeMap<String, Decided> = BTreeMap::new();
        let mut places = Vec::new();
        if self.client.is_some() {
            places.push("worker".to_string());
        }
        for (status, s) in worker? {
            let row = rows.entry(s.email.to_ascii_lowercase()).or_default();
            row.name = s.name;
            row.decided_at = s.decided_at;
            row.places.insert("worker".into(), status.into());
        }
        let mut warnings = self.broken.clone();
        let mut own = HashSet::new();
        for (name, r) in read {
            match r {
                Ok(Some((lists, identities))) => {
                    own.extend(identities.into_iter().map(|a| a.email.to_ascii_lowercase()));
                    for (email, status) in lists.decisions() {
                        rows.entry(email)
                            .or_default()
                            .places
                            .insert(name.clone(), status.into());
                    }
                    places.push(name);
                }
                Ok(None) => {}
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        let mut out: Vec<Decided> = rows
            .into_iter()
            // An address no place can keep (a one-off bounce address) isn't anyone to decide on.
            .filter(|(email, _)| !own.contains(email) && crate::screening::valid(email))
            .map(|(email, mut row)| {
                row.email = email;
                row.settle(&places);
                row
            })
            .collect();
        out.sort_by(|a, b| {
            (b.status == BLOCKED)
                .cmp(&(a.status == BLOCKED))
                .then(b.decided_at.cmp(&a.decided_at))
                .then(a.email.cmp(&b.email))
        });
        Ok((out, places, warnings))
    }

    /// Full-text search across the worker and every account.
    pub fn search(&self, query: &str, limit: u32) -> Result<Listing> {
        let q = ThreadQuery {
            folder: "all".into(),
            q: Some(query.to_string()),
            limit,
            ..Default::default()
        };
        if !self.has_accounts() {
            return Ok(Listing {
                threads: self.worker()?.list_threads(&q)?,
                warnings: self.broken.clone(),
                ..Default::default()
            });
        }
        let (worker, (extra, mut warnings, hidden)) = std::thread::scope(|s| {
            let extra = s.spawn(|| self.accounts_search(query, limit));
            let worker = self.client.as_ref().map_or(Ok(Vec::new()), |c| c.list_threads(&q));
            (worker, extra.join().unwrap_or_default())
        });
        warnings.extend(self.broken.iter().cloned());
        Ok(Listing {
            threads: merge(worker?, extra, limit),
            warnings,
            duplicates_hidden: hidden,
            more: false,
        })
    }

    pub fn accounts_search(&self, query: &str, limit: u32) -> (Vec<ThreadSummary>, Vec<AccountWarning>, usize) {
        let (results, decisions) = self.with_decisions("search", || self.each_account(|p| p.search(query, limit)));
        let results = self.screen(results, "search", decisions);
        self.collect(results, true)
    }

    fn each_account<T: Send>(&self, f: impl Fn(&dyn Provider) -> Result<T> + Sync) -> Vec<(String, Result<T>)> {
        std::thread::scope(|s| {
            let handles: Vec<_> = self
                .accounts
                .iter()
                .map(|p| (p.name().to_string(), s.spawn(|| f(p.as_ref()))))
                .collect();
            handles
                .into_iter()
                .map(|(name, h)| {
                    let r = h.join().unwrap_or_else(|_| {
                        Err(Error::new(
                            ErrorKind::AccountUnavailable,
                            format!("{name}: failed unexpectedly"),
                        ))
                    });
                    (name, r)
                })
                .collect()
        })
    }

    fn collect(
        &self,
        results: Vec<(String, Result<Vec<ThreadSummary>>)>,
        dedupe: bool,
    ) -> (Vec<ThreadSummary>, Vec<AccountWarning>, usize) {
        let mut threads = Vec::new();
        let mut warnings = Vec::new();
        for (name, r) in results {
            match r {
                Ok(list) => threads.extend(list),
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        let before = threads.len();
        if dedupe && !threads.is_empty() {
            let index = self.worker_index(&threads);
            let exact: Vec<Vec<String>> = threads
                .iter()
                .map(|t| {
                    self.account_for(&t.id)
                        .map(|p| p.message_ids(&t.id))
                        .unwrap_or_default()
                })
                .collect();
            let same_subject = |t: &ThreadSummary| {
                index
                    .iter()
                    .filter(|w| subject_key(&w.subject) == subject_key(&t.subject))
                    .collect::<Vec<_>>()
            };
            let candidates: Vec<&ThreadSummary> = threads
                .iter()
                .zip(&exact)
                .filter(|(_, ids)| !ids.is_empty())
                .flat_map(|(t, _)| same_subject(t))
                .collect();
            let known = self.worker_message_ids(&candidates);
            let mut keep = Vec::with_capacity(threads.len());
            for (t, ids) in threads.into_iter().zip(exact) {
                let copy = if ids.is_empty() {
                    index.iter().any(|w| is_copy(&t, w))
                } else {
                    // A worker thread whose messages couldn't be read is judged the other way.
                    same_subject(&t).into_iter().any(|w| match known.get(&w.id) {
                        Some(worker) => worker.iter().any(|m| ids.contains(m)),
                        None => is_copy(&t, w),
                    })
                };
                if !copy {
                    keep.push(t);
                }
            }
            threads = keep;
        }
        let hidden = before - threads.len();
        (threads, warnings, hidden)
    }

    /// The worker's threads with activity around the given threads' times: what their copies would match.
    fn worker_index(&self, threads: &[ThreadSummary]) -> Vec<ThreadSummary> {
        let Some(client) = &self.client else {
            return Vec::new();
        };
        let Some(oldest) = threads.iter().map(|t| t.last_at).filter(|t| *t > 0).min() else {
            return Vec::new();
        };
        let since = Some(oldest - DUPLICATE_WINDOW_MS - 1);
        // 200 is the most the worker returns per request.
        let fetch = |folder: &str| {
            client
                .list_threads(&ThreadQuery {
                    folder: folder.into(),
                    since,
                    limit: 200,
                    ..Default::default()
                })
                .unwrap_or_default()
        };
        let (mut all, blocked) = std::thread::scope(|s| {
            let blocked = s.spawn(|| fetch("blocked"));
            (fetch("all"), blocked.join().unwrap_or_default())
        });
        all.extend(blocked);
        all
    }

    /// Normalized Message-IDs of these worker threads' messages (cached until a thread changes);
    /// threads that couldn't be read are missing.
    fn worker_message_ids(&self, threads: &[&ThreadSummary]) -> HashMap<String, Vec<String>> {
        let mut out = HashMap::new();
        let Some(client) = &self.client else {
            return out;
        };
        let mut todo: Vec<(String, i64)> = Vec::new();
        let mut pending = HashSet::new();
        {
            let cache = self.worker_ids.lock().unwrap();
            for t in threads {
                match cache.get(&t.id).filter(|(at, _)| *at == t.last_at) {
                    Some((_, ids)) => {
                        out.insert(t.id.clone(), ids.clone());
                    }
                    None if pending.insert(&t.id) => todo.push((t.id.clone(), t.last_at)),
                    None => {}
                }
            }
        }
        for chunk in todo.chunks(8) {
            let read: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk
                    .iter()
                    .map(|(id, at)| (id.clone(), *at, s.spawn(move || client.thread(id))))
                    .collect();
                handles
                    .into_iter()
                    .map(|(id, at, h)| (id, at, h.join().ok().and_then(|r| r.ok())))
                    .collect()
            });
            for (id, at, detail) in read {
                let Some(detail) = detail else { continue };
                let ids: Vec<String> = detail
                    .messages
                    .iter()
                    .filter_map(|m| m.message_id.as_deref())
                    .map(crate::gmail::normalize_message_id)
                    .filter(|m| !m.is_empty())
                    .collect();
                self.worker_ids.lock().unwrap().insert(id.clone(), (at, ids.clone()));
                out.insert(id, ids);
            }
        }
        out
    }

    /// Senders waiting in the worker's Screener and every account's. An account's sender who is
    /// also waiting in the worker's is shown once (deciding decides both).
    pub fn screener(&self) -> Result<(Vec<PendingSender>, Vec<AccountWarning>)> {
        if !self.has_accounts() {
            return Ok((self.worker()?.screener()?, self.broken.clone()));
        }
        let (worker, (extra, mut warnings)) = std::thread::scope(|s| {
            let extra = s.spawn(|| self.accounts_screener());
            let worker = self.client.as_ref().map_or(Ok(Vec::new()), |c| c.screener());
            (worker, extra.join().unwrap_or_default())
        });
        warnings.extend(self.broken.iter().cloned());
        Ok((merge_senders(worker?, extra), warnings))
    }

    /// Senders waiting in the linked accounts' Screeners: HEY's own, and the undecided senders in
    /// the Inboxes your worker screens.
    pub fn accounts_screener(&self) -> (Vec<PendingSender>, Vec<AccountWarning>) {
        let mut senders = Vec::new();
        let mut warnings = Vec::new();
        for (name, r) in self.each_account(|p| p.screener()) {
            match r {
                Ok(list) => senders.extend(list),
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        if self.accounts.iter().any(|p| p.screened_by_cloudmail()) {
            let (waiting, w, _) = self.accounts_list(&ThreadQuery {
                folder: "screener".into(),
                limit: SCREEN_SCAN,
                ..Default::default()
            });
            warnings.extend(w);
            let screened: Vec<ThreadSummary> = waiting
                .into_iter()
                .filter(|t| self.account_for(&t.id).is_some_and(|p| p.screened_by_cloudmail()))
                .collect();
            senders.extend(waiting_senders(&screened));
        }
        (senders, warnings)
    }

    /// Screens a sender in or out. `key` is an address (decided in the worker, in every account
    /// Cloudmail screens, and in every HEY account where that address is waiting) or an account's
    /// sender ID such as `hey:123`. A no also archives that sender's Inbox threads in the screened
    /// accounts, whose filters or rules keep their later mail out of the Inbox too.
    pub fn decide_sender(&self, key: &str, status: &str) -> Result<(i64, Vec<AccountWarning>)> {
        if let Some(p) = self.account_for(key) {
            return p.decide_sender(key, status).map(|n| (n, Vec::new()));
        }
        if !self.has_accounts() {
            return self.worker()?.decide_sender(key, status).map(|n| (n, Vec::new()));
        }
        // The worker's decision moves its own threads; it comes first, and its failure fails this.
        let mut moved = match &self.client {
            Some(c) => c.decide_sender(key, status)?,
            None => 0,
        };
        let email = [key.trim().to_ascii_lowercase()];
        // Recorded where Cloudmail keeps decisions (the worker has it already), and a no archives
        // the sender's Inbox threads there; HEY decides its own.
        let extra = self.each_account(|p| {
            if p.screened_by_cloudmail() {
                self.decide_in(p, &email, status, false)?;
                if status != BLOCKED {
                    return Ok(0);
                }
                let inbox = p.threads(&ThreadQuery {
                    folder: "inbox".into(),
                    limit: SCREEN_SCAN,
                    ..Default::default()
                })?;
                let mut moved = 0;
                for t in inbox.iter().filter(|t| t.is_from(key)) {
                    p.move_thread(&t.id, p.archive_folder())?;
                    moved += 1;
                }
                return Ok(moved);
            }
            let mut moved = 0;
            for sender in p.screener()?.into_iter().filter(|s| s.email.eq_ignore_ascii_case(key)) {
                moved += p.decide_sender(sender.id.as_deref().unwrap_or(&sender.email), status)?;
            }
            Ok(moved)
        });
        let mut warnings = Vec::new();
        for (name, r) in extra {
            match r {
                Ok(n) => moved += n,
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        Ok((moved, warnings))
    }

    /// Every account's send-from addresses, by account name.
    pub fn account_identities(&self) -> (Vec<(String, Address)>, Vec<AccountWarning>) {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        for (name, r) in self.each_account(|p| p.identities()) {
            match r {
                Ok(list) => out.extend(list.into_iter().map(|a| (name.clone(), a))),
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        (out, warnings)
    }

    /// Who sends a message: the account whose message it replies to, else the account that owns
    /// the From address, else the worker (without one, the first linked account).
    pub fn sender_for(&self, req: &SendRequest) -> Result<&dyn Provider> {
        if let Some(p) = req.reply_to_message_id.as_deref().and_then(|id| self.account_for(id)) {
            return Ok(p.as_ref());
        }
        if let Some(from) = req
            .from
            .as_deref()
            .map(crate::text::bare_email)
            .filter(|f| !f.is_empty())
        {
            for p in &self.accounts {
                if p.identities()
                    .is_ok_and(|ids| ids.iter().any(|a| a.email.eq_ignore_ascii_case(&from)))
                {
                    return Ok(p.as_ref());
                }
            }
        }
        if let Some(c) = &self.client {
            return Ok(c);
        }
        if req.reply_to_message_id.is_some() || req.from.as_deref().is_some_and(|f| !f.is_empty()) {
            return Err(no_worker());
        }
        self.accounts
            .first()
            .map(|p| p.as_ref() as &dyn Provider)
            .ok_or_else(no_worker)
    }

    pub fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        let sender = self.sender_for(req)?;
        crate::attach::check_limit(&req.attachments, sender.attachment_limit(), sender.label())?;
        let mut sent = sender.send(req)?;
        if self.has_accounts() {
            // People you write to are screened in everywhere, as the worker does for its own mail.
            let people: Vec<Address> = req
                .to
                .iter()
                .chain(&req.cc)
                .chain(&req.bcc)
                .flat_map(|r| crate::text::split_addresses(r))
                .map(|a| crate::gmail::address_from(&a))
                .filter(|a| a.email.contains('@'))
                .collect();
            let to: Vec<String> = people.iter().map(|a| a.email.to_ascii_lowercase()).collect();
            let (_, mut warnings) = self.decide_everywhere(&to, APPROVED, false);
            // The worker screens in whom it sends to itself; mail sent through an account is told to it.
            let by_account = self.accounts.iter().any(|p| std::ptr::addr_eq(p.as_ref(), sender));
            if let Some(c) = self.client.as_ref().filter(|_| by_account)
                && let Err(e) = c.decide_senders(&people, APPROVED, false)
            {
                warnings.push(AccountWarning::new("worker", &e));
            }
            if let Some(w) = warnings.first() {
                sent.warning = Some(format!(
                    "sent, but its recipients couldn't be screened in: {}",
                    w.message
                ));
            }
        }
        Ok(sent)
    }

    /// Screens in the people a newly linked account already corresponds with (never overriding
    /// a decision); how many were new.
    pub fn screen_in_correspondents(&self, account: &dyn Provider) -> Result<i64> {
        if !account.screened_by_cloudmail() {
            return Ok(0);
        }
        // Into the account itself: it may be newer than this `Mail`, which doesn't list it yet.
        let people = account.correspondents(100)?;
        let emails: Vec<String> = people.iter().map(|a| a.email.to_ascii_lowercase()).collect();
        let n = self.decide_in(account, &emails, APPROVED, true)?;
        if let Some(c) = &self.client {
            c.decide_senders(&people, APPROVED, true)?;
        }
        Ok(n as i64)
    }
}

/// The worker's decisions, blocked first.
fn worker_decisions(c: &Client) -> Result<Vec<(&'static str, Sender)>> {
    let mut out = Vec::new();
    for status in [BLOCKED, APPROVED] {
        out.extend(c.senders(status)?.into_iter().map(|s| (status, s)));
    }
    Ok(out)
}

/// What a worker-only action says without a worker.
fn no_worker() -> Error {
    Error::new(
        ErrorKind::Config,
        "that needs a Cloudmail worker; this computer only shows linked accounts (Gmail, iCloud Mail, HEY). `cloudmail connect <invite link>` joins someone's, `cloudmail setup` deploys your own",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, email: &str, subject: &str, at: i64) -> ThreadSummary {
        ThreadSummary {
            id: id.into(),
            subject: subject.into(),
            from: Some(Address {
                name: None,
                email: email.into(),
            }),
            last_at: at,
            ..Default::default()
        }
    }

    #[test]
    fn waiting_senders_groups_case_insensitively_and_keeps_first_seen_ties() {
        let mut first = t("one", "Ann@Example.com", "older", 10);
        first.from.as_mut().unwrap().name = Some("First name".into());
        first.account = Some("first-account".into());
        let mut latest = t("two", "ANN@example.com", "newest", 30);
        latest.from.as_mut().unwrap().name = Some("Later name".into());
        let rows = waiting_senders(&[
            first,
            t("third", "bob@example.com", "same time", 30),
            latest,
            t("equal", "ann@example.com", "later input, same time", 30),
            t("invalid", "no-domain", "ignored", 50),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].email, "ann@example.com");
        assert_eq!(rows[0].thread_count, 3);
        assert_eq!(rows[0].last_subject.as_deref(), Some("newest"));
        assert_eq!(rows[0].name.as_deref(), Some("First name"));
        assert_eq!(rows[0].account.as_deref(), Some("first-account"));
        assert_eq!(rows[1].email, "bob@example.com");
    }

    #[test]
    fn copies_match_on_sender_subject_and_time() {
        let original = t("t_1", "Ann@Example.com", "Lunch?", 1_000_000);
        assert!(is_copy(
            &t("hey:1:2", "ann@example.com", "Re: lunch?", 1_000_000 + 60_000),
            &original
        ));
        assert!(
            !is_copy(
                &t(
                    "hey:1:2",
                    "ann@example.com",
                    "Lunch?",
                    1_000_000 + DUPLICATE_WINDOW_MS + 1
                ),
                &original
            ),
            "too far apart"
        );
        assert!(
            !is_copy(&t("hey:1:2", "bob@example.com", "Lunch?", 1_000_000), &original),
            "another sender"
        );
        assert!(
            !is_copy(&t("hey:1:2", "ann@example.com", "Dinner?", 1_000_000), &original),
            "another subject"
        );
        assert!(
            !is_copy(&t("hey:1:2", "", "", 0), &t("t_2", "", "", 0)),
            "unknown senders never match"
        );
    }

    #[test]
    fn subjects_ignore_prefixes_case_and_spacing() {
        assert_eq!(subject_key("Re: FWD:  Hello   World"), "hello world");
        assert_eq!(subject_key("Fw:Re:x"), "x");
    }

    #[test]
    fn merge_is_newest_first_and_limited() {
        let m = merge(
            vec![t("a", "x@y", "", 3), t("b", "x@y", "", 1)],
            vec![t("hey:c", "x@y", "", 2)],
            2,
        );
        assert_eq!(m.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a", "hey:c"]);
    }
}
