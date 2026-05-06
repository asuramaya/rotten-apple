//! One-shot reply channel built on `mpsc::sync_channel(1)`.
//!
//! The actor pattern wants "send a request, block on a single reply".
//! `mpsc::channel` would work but allows multiple sends; `sync_channel(1)`
//! is the right shape — bounded to one slot, one value, one receiver.
//!
//! No tokio. The receiver blocks on `recv()`; on actor crash the sender
//! is dropped and `recv()` returns `Err`, which the caller surfaces as
//! "actor crashed" rather than hanging.

use std::sync::mpsc::{self, RecvError, SendError, SyncSender};

pub struct Sender<T>(SyncSender<T>);

pub struct Receiver<T>(mpsc::Receiver<T>);

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = mpsc::sync_channel(1);
    (Sender(tx), Receiver(rx))
}

impl<T> Sender<T> {
    /// Send the one reply. Returns Err if the receiver was dropped.
    pub fn send(self, v: T) -> Result<(), SendError<T>> {
        self.0.send(v)
    }
}

impl<T> Receiver<T> {
    /// Block until the actor sends a reply (or drops the sender).
    pub fn recv(self) -> Result<T, RecvError> {
        self.0.recv()
    }
}
