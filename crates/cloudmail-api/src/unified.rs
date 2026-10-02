//! Your worker plus any linked accounts, as one mailbox.
//!
//! The worker is authoritative: its errors fail an operation as they always have. A linked
//! account's errors never do; they come back as warnings next to whatever the worker returned.
//!
//! Duplicates: a worker that forwards to a linked account (`forward_to` = your HEY address), or
//! an account that forwards into your worker, puts every message in both; the account's copy is
//! hidden. Where the account exposes Message-IDs (Gmail), a thread is a copy when one of its
//! messages has the Message-ID of a message in a worker thread with the same subject. Otherwise
//! (HEY's CLI exposes none) it is a copy when the sender's address and the subject (ignoring
//! Re:/Fwd:) match and the latest activity is within `DUPLICATE_WINDOW_MS`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::client::{Client, ThreadQuery};
use crate::config::Config;
use crate::error::{Error, ErrorKind, Result};
use crate::provider::{self, AccountWarning, ExtraFolder, Provider};
use crate::types::*;

pub const DUPLICATE_WINDOW_MS: i64 = 15 * 60 * 1000;

/// A worker thread's `last_at` when read, and its messages' Message-IDs.
type WorkerIds = (i64, Vec<String>);

#[derive(Clone)]
pub struct Mail {
    pub client: Client,
    pub accounts: Vec<Arc<dyn Provider>>,
    /// Configured accounts that couldn't be opened (unknown provider, …).
    pub broken: Vec<AccountWarning>,
    /// Worker threads' Message-IDs, by thread ID, with the `last_at` they were read at.
    worker_ids: Arc<Mutex<HashMap<String, WorkerIds>>>,
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
        let Some(p) = ["re:", "fwd:", "fw:", "aw:", "sv:"].iter().find(|p| lower.starts_with(**p)) else { break };
        s = s[p.len()..].trim_start();
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Whether `copy` (from a linked account) is the same conversation as the worker's `original`.
pub fn is_copy(copy: &ThreadSummary, original: &ThreadSummary) -> bool {
    let sender = |t: &ThreadSummary| t.from.as_ref().map(|a| a.email.to_ascii_lowercase()).filter(|e| !e.is_empty());
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

/// Newest first, at most `limit`.
pub fn merge(mut a: Vec<ThreadSummary>, b: Vec<ThreadSummary>, limit: u32) -> Vec<ThreadSummary> {
    a.extend(b);
    a.sort_by_key(|t| std::cmp::Reverse(t.last_at));
    a.truncate(limit.max(1) as usize);
    a
}

impl Mail {
    pub fn new(client: Client, config: &Config) -> Self {
        let mut accounts = Vec::new();
        let mut broken = Vec::new();
        for (name, cfg) in &config.accounts {
            match provider::open(name, cfg) {
                Ok(p) => accounts.push(p),
                Err(e) => broken.push(AccountWarning::new(name, &e)),
            }
        }
        Self { client, accounts, broken, worker_ids: Default::default() }
    }

    pub fn from_config(config: &Config) -> Self {
        Self::new(Client::new(config), config)
    }

    pub fn has_accounts(&self) -> bool {
        !self.accounts.is_empty()
    }

    /// The linked account that owns an ID, or None for the worker's own.
    pub fn account_for(&self, id: &str) -> Option<&Arc<dyn Provider>> {
        self.accounts.iter().find(|p| p.owns(id))
    }

    /// Who handles an ID: its linked account, else the worker.
    pub fn provider(&self, id: &str) -> &dyn Provider {
        match self.account_for(id) {
            Some(p) => p.as_ref(),
            None => &self.client,
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
        let folder = if q.folder.is_empty() { "inbox" } else { q.folder.as_str() };
        let worker_folder = provider::CORE_FOLDERS.contains(&folder);
        if !self.has_accounts() {
            let threads = if worker_folder { self.client.list_threads(q)? } else { Vec::new() };
            let more = threads.len() as u32 >= q.limit.max(1);
            return Ok(Listing { threads, more, warnings: self.broken.clone(), ..Default::default() });
        }
        let (worker, (extra, mut warnings, hidden)) = std::thread::scope(|s| {
            let extra = s.spawn(|| self.accounts_list(q));
            let worker = if worker_folder { self.client.list_threads(q) } else { Ok(Vec::new()) };
            (worker, extra.join().unwrap_or_default())
        });
        let worker = worker?;
        let more = worker.len() as u32 >= q.limit.max(1);
        warnings.extend(self.broken.iter().cloned());
        Ok(Listing { threads: merge(worker, extra, q.limit), warnings, duplicates_hidden: hidden, more })
    }

    /// Linked accounts' threads in a folder, with copies of worker mail left out.
    pub fn accounts_list(&self, q: &ThreadQuery) -> (Vec<ThreadSummary>, Vec<AccountWarning>, usize) {
        let folder = if q.folder.is_empty() { "inbox" } else { q.folder.as_str() };
        let results = self.each_account(|p| if p.has_folder(folder) { p.threads(q) } else { Ok(Vec::new()) });
        self.collect(results, folder != "screener")
    }

    /// Full-text search across the worker and every account.
    pub fn search(&self, query: &str, limit: u32) -> Result<Listing> {
        let q = ThreadQuery { folder: "all".into(), q: Some(query.to_string()), limit, ..Default::default() };
        if !self.has_accounts() {
            return Ok(Listing { threads: self.client.list_threads(&q)?, warnings: self.broken.clone(), ..Default::default() });
        }
        let (worker, (extra, mut warnings, hidden)) = std::thread::scope(|s| {
            let extra = s.spawn(|| self.accounts_search(query, limit));
            (self.client.list_threads(&q), extra.join().unwrap_or_default())
        });
        warnings.extend(self.broken.iter().cloned());
        Ok(Listing { threads: merge(worker?, extra, limit), warnings, duplicates_hidden: hidden, more: false })
    }

    pub fn accounts_search(&self, query: &str, limit: u32) -> (Vec<ThreadSummary>, Vec<AccountWarning>, usize) {
        let results = self.each_account(|p| p.search(query, limit));
        self.collect(results, true)
    }

    fn each_account<T: Send>(&self, f: impl Fn(&dyn Provider) -> Result<T> + Sync) -> Vec<(String, Result<T>)> {
        std::thread::scope(|s| {
            let handles: Vec<_> = self.accounts.iter().map(|p| (p.name().to_string(), s.spawn(|| f(p.as_ref())))).collect();
            handles
                .into_iter()
                .map(|(name, h)| {
                    let r = h.join().unwrap_or_else(|_| Err(Error::new(ErrorKind::AccountUnavailable, format!("{name}: failed unexpectedly"))));
                    (name, r)
                })
                .collect()
        })
    }

    fn collect(&self, results: Vec<(String, Result<Vec<ThreadSummary>>)>, dedupe: bool) -> (Vec<ThreadSummary>, Vec<AccountWarning>, usize) {
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
            let exact: Vec<Vec<String>> = threads.iter().map(|t| self.account_for(&t.id).map(|p| p.message_ids(&t.id)).unwrap_or_default()).collect();
            let same_subject = |t: &ThreadSummary| index.iter().filter(|w| subject_key(&w.subject) == subject_key(&t.subject)).collect::<Vec<_>>();
            let candidates: Vec<&ThreadSummary> = threads.iter().zip(&exact).filter(|(_, ids)| !ids.is_empty()).flat_map(|(t, _)| same_subject(t)).collect();
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
        let Some(oldest) = threads.iter().map(|t| t.last_at).filter(|t| *t > 0).min() else { return Vec::new() };
        let since = Some(oldest - DUPLICATE_WINDOW_MS - 1);
        // 200 is the most the worker returns per request.
        let fetch = |folder: &str| self.client.list_threads(&ThreadQuery { folder: folder.into(), since, limit: 200, ..Default::default() }).unwrap_or_default();
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
        let mut todo: Vec<(String, i64)> = Vec::new();
        {
            let cache = self.worker_ids.lock().unwrap();
            for t in threads {
                match cache.get(&t.id).filter(|(at, _)| *at == t.last_at) {
                    Some((_, ids)) => {
                        out.insert(t.id.clone(), ids.clone());
                    }
                    None if !todo.iter().any(|(id, _)| *id == t.id) => todo.push((t.id.clone(), t.last_at)),
                    None => {}
                }
            }
        }
        for chunk in todo.chunks(8) {
            let read: Vec<_> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk.iter().map(|(id, at)| (id.clone(), *at, s.spawn(move || self.client.thread(id)))).collect();
                handles.into_iter().map(|(id, at, h)| (id, at, h.join().ok().and_then(|r| r.ok()))).collect()
            });
            for (id, at, detail) in read {
                let Some(detail) = detail else { continue };
                let ids: Vec<String> = detail.messages.iter().filter_map(|m| m.message_id.as_deref()).map(crate::gmail::normalize_message_id).filter(|m| !m.is_empty()).collect();
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
            return Ok((self.client.screener()?, self.broken.clone()));
        }
        let (worker, (extra, mut warnings)) = std::thread::scope(|s| {
            let extra = s.spawn(|| self.accounts_screener());
            (self.client.screener(), extra.join().unwrap_or_default())
        });
        warnings.extend(self.broken.iter().cloned());
        Ok((merge_senders(worker?, extra), warnings))
    }

    /// Senders waiting in the linked accounts' Screeners.
    pub fn accounts_screener(&self) -> (Vec<PendingSender>, Vec<AccountWarning>) {
        let mut senders = Vec::new();
        let mut warnings = Vec::new();
        for (name, r) in self.each_account(|p| p.screener()) {
            match r {
                Ok(list) => senders.extend(list),
                Err(e) => warnings.push(AccountWarning::new(&name, &e)),
            }
        }
        (senders, warnings)
    }

    /// Screens a sender in or out. `key` is an address (decided in the worker, and in every
    /// account where that address is waiting) or an account's sender ID such as `hey:123`.
    pub fn decide_sender(&self, key: &str, status: &str) -> Result<(i64, Vec<AccountWarning>)> {
        if let Some(p) = self.account_for(key) {
            return p.decide_sender(key, status).map(|n| (n, Vec::new()));
        }
        if !self.has_accounts() {
            return self.client.decide_sender(key, status).map(|n| (n, Vec::new()));
        }
        let (worker, extra) = std::thread::scope(|s| {
            let extra = s.spawn(|| {
                self.each_account(|p| {
                    let mut moved = 0;
                    for sender in p.screener()?.into_iter().filter(|s| s.email.eq_ignore_ascii_case(key)) {
                        moved += p.decide_sender(sender.id.as_deref().unwrap_or(&sender.email), status)?;
                    }
                    Ok(moved)
                })
            });
            (self.client.decide_sender(key, status), extra.join().unwrap_or_default())
        });
        let mut moved = worker?;
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
    /// the From address, else the worker.
    pub fn sender_for(&self, req: &SendRequest) -> &dyn Provider {
        if let Some(p) = req.reply_to_message_id.as_deref().and_then(|id| self.account_for(id)) {
            return p.as_ref();
        }
        if let Some(from) = req.from.as_deref().map(crate::text::bare_email).filter(|f| !f.is_empty()) {
            for p in &self.accounts {
                if p.identities().is_ok_and(|ids| ids.iter().any(|a| a.email.eq_ignore_ascii_case(&from))) {
                    return p.as_ref();
                }
            }
        }
        &self.client
    }

    pub fn send(&self, req: &SendRequest) -> Result<SendResponse> {
        let sender = self.sender_for(req);
        crate::attach::check_limit(&req.attachments, sender.attachment_limit(), sender.label())?;
        sender.send(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(id: &str, email: &str, subject: &str, at: i64) -> ThreadSummary {
        ThreadSummary { id: id.into(), subject: subject.into(), from: Some(Address { name: None, email: email.into() }), last_at: at, ..Default::default() }
    }

    #[test]
    fn copies_match_on_sender_subject_and_time() {
        let original = t("t_1", "Ann@Example.com", "Lunch?", 1_000_000);
        assert!(is_copy(&t("hey:1:2", "ann@example.com", "Re: lunch?", 1_000_000 + 60_000), &original));
        assert!(!is_copy(&t("hey:1:2", "ann@example.com", "Lunch?", 1_000_000 + DUPLICATE_WINDOW_MS + 1), &original), "too far apart");
        assert!(!is_copy(&t("hey:1:2", "bob@example.com", "Lunch?", 1_000_000), &original), "another sender");
        assert!(!is_copy(&t("hey:1:2", "ann@example.com", "Dinner?", 1_000_000), &original), "another subject");
        assert!(!is_copy(&t("hey:1:2", "", "", 0), &t("t_2", "", "", 0)), "unknown senders never match");
    }

    #[test]
    fn subjects_ignore_prefixes_case_and_spacing() {
        assert_eq!(subject_key("Re: FWD:  Hello   World"), "hello world");
        assert_eq!(subject_key("Fw:Re:x"), "x");
    }

    #[test]
    fn merge_is_newest_first_and_limited() {
        let m = merge(vec![t("a", "x@y", "", 3), t("b", "x@y", "", 1)], vec![t("hey:c", "x@y", "", 2)], 2);
        assert_eq!(m.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["a", "hey:c"]);
    }
}
