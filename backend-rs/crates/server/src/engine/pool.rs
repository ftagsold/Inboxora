//! Per-account body-fetch pool (size 2): reserved slots, FIFO waiters with a 10 s fallback to
//! a temporary connection, eviction on error/disconnect.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use inboxora_imap::Client;
use parking_lot::Mutex;
use tokio::sync::oneshot;

use super::policy::POOL_SIZE;

#[derive(Default)]
struct State {
    idle: Vec<Client>,
    in_use: usize,
    connecting: usize,
    waiters: VecDeque<oneshot::Sender<Client>>,
    evicted: bool,
}

#[derive(Default)]
pub struct Pool {
    state: Mutex<State>,
}

pub enum Slot {
    /// A pooled connection is ready.
    Ready(Lease),
    /// A slot is reserved: connect, then `Pool::admit`.
    Connect(Reservation),
    /// The pool is full: wait for a hand-off.
    Wait(oneshot::Receiver<Client>),
}

/// A reserved connect slot; dropping it without `admit` frees the slot.
pub struct Reservation {
    pool: Arc<Pool>,
    done: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.done {
            self.pool.state.lock().connecting -= 1;
        }
    }
}

impl Pool {
    pub fn try_acquire(self: &Arc<Self>) -> Slot {
        let mut s = self.state.lock();
        while let Some(c) = s.idle.pop() {
            if c.is_usable() {
                s.in_use += 1;
                return Slot::Ready(Lease { client: Some(c), pool: Some(self.clone()), evict: false });
            }
        }
        if s.idle.len() + s.in_use + s.connecting < POOL_SIZE {
            s.connecting += 1;
            return Slot::Connect(Reservation { pool: self.clone(), done: false });
        }
        let (tx, rx) = oneshot::channel();
        s.waiters.push_back(tx);
        Slot::Wait(rx)
    }

    /// Turn a reservation into a lease for the freshly connected client.
    pub fn admit(self: &Arc<Self>, mut r: Reservation, client: Client) -> Result<Lease, Client> {
        r.done = true;
        let mut s = self.state.lock();
        s.connecting -= 1;
        if s.evicted {
            return Err(client);
        }
        s.in_use += 1;
        Ok(Lease { client: Some(client), pool: Some(self.clone()), evict: false })
    }

    /// Lease for a client handed over by a releasing lease (already counted as in use).
    pub fn handed(self: &Arc<Self>, client: Client) -> Lease {
        Lease { client: Some(client), pool: Some(self.clone()), evict: false }
    }

    pub fn evict(&self) {
        let mut s = self.state.lock();
        s.evicted = true;
        for mut c in s.idle.drain(..) {
            tokio::spawn(async move { c.logout().await });
        }
        s.waiters.clear();
    }

    fn release(&self, client: Client, evict: bool) {
        let mut s = self.state.lock();
        if evict || s.evicted || !client.is_usable() {
            s.in_use = s.in_use.saturating_sub(1);
            drop(s);
            close_later(client);
            return;
        }
        let mut client = client;
        while let Some(w) = s.waiters.pop_front() {
            match w.send(client) {
                Ok(()) => return, // handed over; still in use
                Err(c) => client = c,
            }
        }
        s.in_use = s.in_use.saturating_sub(1);
        s.idle.push(client);
    }
}

fn close_later(mut c: Client) {
    if c.is_usable() {
        tokio::spawn(async move { c.logout().await });
    } else {
        c.close();
    }
}

/// A connection borrowed from the pool (or a temporary one); returned on drop.
pub struct Lease {
    client: Option<Client>,
    pool: Option<Arc<Pool>>,
    evict: bool,
}

impl Lease {
    pub fn temporary(client: Client) -> Self {
        Self { client: Some(client), pool: None, evict: false }
    }

    pub fn client(&mut self) -> &mut Client {
        self.client.as_mut().expect("lease holds a client")
    }

    /// Drop this connection instead of returning it (after an error).
    pub fn evict(&mut self) {
        self.evict = true;
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(c) = self.client.take() else { return };
        match &self.pool {
            Some(p) => p.release(c, self.evict),
            None => close_later(c),
        }
    }
}

pub const WAIT_FOR_SLOT: Duration = Duration::from_secs(10);
