//! The Screener's decisions for accounts without a Screener of their own (Gmail, iCloud Mail),
//! kept in each account itself so that every computer, and the account's own apps, go by them.
//! Gmail filters and iCloud Mail rules hold two lists of senders:
//!
//! - blocked: their new mail skips the Inbox on the server (archived as it arrives), on every device;
//! - approved: harmless on the server (Gmail: never Spam; iCloud Mail: the Personal category) and
//!   there to be read back.
//!
//! Waiting in the Screener is Inbox mail from anyone on neither list. A list too long for one
//! filter or rule is spread over several; only Cloudmail's own (by label or name) are ever changed.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Each list's filters or rules, in order: (the filter or rule, its addresses).
pub type Pieces<T> = HashMap<&'static str, Vec<(T, Vec<String>)>>;

pub const APPROVED: &str = "approved";
pub const BLOCKED: &str = "blocked";

/// How long a read of an account's lists is reused within one process: longer than the app's
/// default poll, so a poll reuses the previous one's.
const TTL: Duration = Duration::from_secs(90);

/// An address a decision can be kept for: one `@`, and a local part within RFC 5321's 64
/// characters (iCloud refuses longer ones, which only one-off bounce addresses have).
pub fn valid(email: &str) -> bool {
    email
        .split_once('@')
        .is_some_and(|(local, domain)| !local.is_empty() && local.len() <= 64 && !domain.is_empty())
}

/// One account's decisions, by lowercase address.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Lists {
    pub approved: BTreeSet<String>,
    pub blocked: BTreeSet<String>,
    /// Whether Cloudmail keeps lists in this account yet; it seeds them on first use.
    pub kept: bool,
}

impl Lists {
    /// The lists a provider's filters or rules hold; kept when there are any.
    pub fn from_pieces<T>(pieces: &Pieces<T>) -> Self {
        let list = |kind| {
            pieces
                .get(kind)
                .into_iter()
                .flatten()
                .flat_map(|(_, a)| a.clone())
                .collect()
        };
        Lists {
            kept: !pieces.is_empty(),
            approved: list(APPROVED),
            blocked: list(BLOCKED),
        }
    }

    /// Whether deciding these senders would change nothing.
    pub fn settled<'a>(&self, emails: impl IntoIterator<Item = &'a str>, status: &str, only_undecided: bool) -> bool {
        self.kept && self.clone().decide(emails, status, only_undecided) == 0
    }

    /// Puts senders on `status`'s list and takes them off the other; with `only_undecided`, a
    /// sender already on either list keeps that decision. How many changed.
    pub fn decide<'a>(
        &mut self,
        emails: impl IntoIterator<Item = &'a str>,
        status: &str,
        only_undecided: bool,
    ) -> usize {
        let (on, off) = if status == BLOCKED {
            (&mut self.blocked, &mut self.approved)
        } else {
            (&mut self.approved, &mut self.blocked)
        };
        let mut changed = 0;
        for e in emails {
            let e = e.trim().to_ascii_lowercase();
            if !valid(&e) || on.contains(&e) || (only_undecided && off.contains(&e)) {
                continue;
            }
            off.remove(&e);
            on.insert(e);
            changed += 1;
        }
        changed
    }

    /// Every decision, a block winning.
    pub fn decisions(&self) -> HashMap<String, &'static str> {
        let mut out: HashMap<String, &'static str> = self.approved.iter().map(|e| (e.clone(), APPROVED)).collect();
        out.extend(self.blocked.iter().map(|e| (e.clone(), BLOCKED)));
        out
    }

    /// The list for a status.
    pub fn list(&self, status: &str) -> &BTreeSet<String> {
        if status == BLOCKED {
            &self.blocked
        } else {
            &self.approved
        }
    }
}

/// The lists as a provider last read or saved them, reused for `TTL` unless a fresh read is asked for.
#[derive(Default)]
pub struct Cache(Mutex<Option<(Instant, Lists)>>);

impl Cache {
    pub fn get(&self, fresh: bool, read: impl FnOnce() -> crate::Result<Lists>) -> crate::Result<Lists> {
        if !fresh
            && let Some((at, lists)) = self.0.lock().unwrap().as_ref()
            && at.elapsed() < TTL
        {
            return Ok(lists.clone());
        }
        let lists = read()?;
        self.put(&lists);
        Ok(lists)
    }

    pub fn put(&self, lists: &Lists) {
        *self.0.lock().unwrap() = Some((Instant::now(), lists.clone()));
    }

    pub fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

/// Spreads `want` over pieces that each `fit`, keeping the existing pieces as they are where it
/// can: a sender no longer wanted leaves its piece, a new one goes into the last piece with room,
/// so one decision changes one filter or rule. Empty pieces are dropped.
pub fn rechunk(
    existing: &[Vec<String>],
    want: &BTreeSet<String>,
    fits: impl Fn(&[String]) -> bool,
) -> Vec<Vec<String>> {
    let mut placed = BTreeSet::new();
    let mut out: Vec<Vec<String>> = existing
        .iter()
        .map(|piece| {
            piece
                .iter()
                .filter(|e| want.contains(*e) && placed.insert((*e).clone()))
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|p| !p.is_empty())
        .collect();
    for e in want.iter().filter(|e| !placed.contains(*e)) {
        match out.last_mut() {
            Some(last) if fits(&[last.as_slice(), std::slice::from_ref(e)].concat()) => last.push(e.clone()),
            _ => out.push(vec![e.clone()]),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn deciding_moves_a_sender_between_lists() {
        let mut l = Lists::default();
        assert_eq!(
            l.decide(["Ana@X.com", "bob@y.com", "not-an-address"], APPROVED, false),
            2
        );
        assert_eq!(l.decisions()["ana@x.com"], APPROVED);
        assert_eq!(l.decide(["ana@x.com"], BLOCKED, false), 1);
        assert_eq!((l.decisions()["ana@x.com"], l.approved.len()), (BLOCKED, 1));
        // Screening in correspondents never undoes a no, and repeating a decision changes nothing.
        assert_eq!(l.decide(["ana@x.com", "bob@y.com", "cy@z.com"], APPROVED, true), 1);
        assert_eq!(l.decide(["cy@z.com"], APPROVED, false), 0);
        assert_eq!(l.decisions()["ana@x.com"], BLOCKED);
    }

    #[test]
    fn rechunking_keeps_existing_pieces() {
        let fits = |p: &[String]| p.len() <= 2;
        let fresh = rechunk(&[], &set(&["a", "b", "c"]), fits);
        assert_eq!(fresh, vec![vec!["a", "b"], vec!["c"]]);
        // Removing "a" and adding "d" touches only the pieces they were in or go to.
        let next = rechunk(&fresh, &set(&["b", "c", "d"]), fits);
        assert_eq!(next, vec![vec!["b"], vec!["c", "d"]]);
        assert_eq!(rechunk(&next, &BTreeSet::new(), fits), Vec::<Vec<String>>::new());
    }
}
