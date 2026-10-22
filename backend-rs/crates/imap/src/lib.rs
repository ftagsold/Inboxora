//! IMAP client for the mail engine: framing over `imap-proto`, imapflow-compatible session
//! behaviour and result shapes.

pub mod client;
pub mod special_use;
pub mod transport;
pub mod types;
pub mod utf7;

pub use client::{
    Auth, Client, CmdStatus, Config, CopyResult, CreateResult, Error, Event, FetchQuery, ListEntry, Mailbox, Namespace,
    Result, Status_ as MailboxStatus, uid_set,
};
pub use types::{Address, Envelope, Message, parse_internal_date, parse_mail_date, structure_json};
