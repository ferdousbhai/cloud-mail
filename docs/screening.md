# The Screener

The first email from someone new doesn't reach your Inbox. It waits in the **Screener** until you
say yes or no, once, for that sender.

- **Yes** (`y` in the app, `cloudmail screener approve alice@example.com`): their waiting mail moves
  to the Inbox, and everything they send later goes straight there.
- **No** (`n`, `cloudmail screener block spam@bad.biz`): their mail moves to Blocked, marked read,
  and later mail from them goes there too. Nothing is bounced, so the sender can't tell. Changed
  your mind? Approve them, and their blocked mail comes back to the Inbox, unread.

`cloudmail screener` lists who's waiting; `cloudmail senders` lists everyone you've already decided on.

## Who skips it

- **People you write to.** Anyone in To, Cc or Bcc of a message you send is screened in.
- **Direct addresses.** Addresses set up with `:direct` (for example `support@a.com:direct`) deliver
  straight to the Inbox, because a role address shouldn't make customers wait.
- **You.** Mail from your own addresses goes to the Inbox, as long as it really is from you (see below).

Mail to an address on your domain that has no mailbox entry is screened.

## Forged mail can't get past it

A decision is about an address, and anyone can type any address into `From`. So Cloudmail only
trusts `From` when the sender's domain authenticated the message: DMARC passed, or, for a domain
with no DMARC policy, DKIM or SPF aligned with the From domain (the same test DMARC applies). Mail
that fails:

- can't ride on an earlier "yes": it waits in the Screener even from someone you approved;
- can't join an existing conversation by quoting its Message-ID, and can't lift one out of the Screener;
- claiming to come from one of your own addresses goes to Blocked.

The same rules stop a sender who is still waiting from slipping into a conversation in your Inbox:
until you say yes, their mail only adds to their own Screener thread.

A domain with no working SPF or DKIM therefore waits in the Screener every time. That's on purpose.

## Linked accounts

HEY has its own Screener, and Cloudmail shows it as is: a sender waiting in both shows once, and one
decision covers both. Gmail and iCloud Mail have no Screener, so Cloudmail gives them one and keeps
its decisions in the accounts themselves: Gmail filters and iCloud Mail rules, so a "no" keeps that
sender's later mail out of the Inbox on your phone too, with or without a worker. One decision per
sender, wherever their mail comes. The forged-sender checks above are for mail your worker
receives; Gmail and iCloud Mail mail is screened by its From address. Linking an account screens in the people it already corresponds
with, without overriding a "no" you've already given. Details:
[linked-accounts.md](linked-accounts.md#one-screener-for-gmail-and-icloud-mail).
