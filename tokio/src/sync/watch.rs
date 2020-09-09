//! A single-producer, multi-consumer channel that only retains the *last* sent
//! value.
//!
//! This channel is useful for watching for changes to a value from multiple
//! points in the code base, for example, changes to configuration values.
//!
//! # Usage
//!
//! [`channel`] returns a [`Sender`] / [`Receiver`] pair. These are the producer
//! and sender halves of the channel. The channel is created with an initial
//! value. The **latest** value stored in the channel is accessed with
//! [`Receiver::borrow()`]. Awaiting [`Receiver::changed()`] waits for a new
//! value to sent by the [`Sender`] half.
//!
//! # Examples
//!
//! ```
//! use tokio::sync::watch;
//!
//! # async fn dox() -> Result<(), Box<dyn std::error::Error>> {
//!     let (tx, mut rx) = watch::channel("hello");
//!
//!     tokio::spawn(async move {
//!         while rx.changed().await.is_ok() {
//!             println!("received = {:?}", *rx.borrow());
//!         }
//!     });
//!
//!     tx.send("world")?;
//! # Ok(())
//! # }
//! ```
//!
//! # Closing
//!
//! [`Sender::closed`] allows the producer to detect when all [`Receiver`]
//! handles have been dropped. This indicates that there is no further interest
//! in the values being produced and work can be stopped.
//!
//! # Thread safety
//!
//! Both [`Sender`] and [`Receiver`] are thread safe. They can be moved to other
//! threads and can be used in a concurrent environment. Clones of [`Receiver`]
//! handles may be moved to separate threads and also used concurrently.
//!
//! [`Sender`]: crate::sync::watch::Sender
//! [`Receiver`]: crate::sync::watch::Receiver
//! [`Receiver::changed()`]: crate::sync::watch::Receiver::changed
//! [`Receiver::borrow()`]: crate::sync::watch::Receiver::borrow
//! [`channel`]: crate::sync::watch::channel
//! [`Sender::closed`]: crate::sync::watch::Sender::closed

use crate::sync::Notify;

use std::ops;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::{Arc, RwLock, RwLockReadGuard};

/// Receives values from the associated [`Sender`](struct@Sender).
///
/// Instances are created by the [`channel`](fn@channel) function.
#[derive(Debug)]
pub struct Receiver<T> {
    /// Pointer to the shared state
    shared: Arc<Shared<T>>,

    /// Last observed version
    version: usize,
}

/// Sends values to the associated [`Receiver`](struct@Receiver).
///
/// Instances are created by the [`channel`](fn@channel) function.
#[derive(Debug)]
pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

/// Returns a reference to the inner value
///
/// Outstanding borrows hold a read lock on the inner value. This means that
/// long lived borrows could cause the produce half to block. It is recommended
/// to keep the borrow as short lived as possible.
#[derive(Debug)]
pub struct Ref<'a, T> {
    inner: RwLockReadGuard<'a, T>,
}

#[derive(Debug)]
struct Shared<T> {
    /// The most recent value
    value: RwLock<T>,

    /// The current version
    ///
    /// The lowest bit represents a "closed" state. The rest of the bits
    /// represent the current version.
    version: AtomicUsize,

    /// Tracks the number of `Receiver` instances
    ref_count_rx: AtomicUsize,

    /// Notifies waiting receivers that the value changed.
    notify_rx: Notify,

    /// Notifies any task listening for `Receiver` dropped events
    notify_tx: Notify,
}

pub mod error {
    //! Watch error types

    use std::fmt;

    /// Error produced when sending a value fails.
    #[derive(Debug)]
    pub struct SendError<T> {
        pub(crate) inner: T,
    }

    // ===== impl SendError =====

    impl<T: fmt::Debug> fmt::Display for SendError<T> {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(fmt, "channel closed")
        }
    }

    impl<T: fmt::Debug> std::error::Error for SendError<T> {}

    /// Error produced when receiving a change notification.
    #[derive(Debug)]
    pub struct RecvError(pub(super) ());

    // ===== impl RecvError =====

    impl fmt::Display for RecvError {
        fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(fmt, "channel closed")
        }
    }

    impl std::error::Error for RecvError {}
}

const CLOSED: usize = 1;

/// Creates a new watch channel, returning the "send" and "receive" handles.
///
/// All values sent by [`Sender`] will become visible to the [`Receiver`] handles.
/// Only the last value sent is made available to the [`Receiver`] half. All
/// intermediate values are dropped.
///
/// # Examples
///
/// ```
/// use tokio::sync::watch;
///
/// # async fn dox() -> Result<(), Box<dyn std::error::Error>> {
///     let (tx, mut rx) = watch::channel("hello");
///
///     tokio::spawn(async move {
///         while rx.changed().await.is_ok() {
///             println!("received = {:?}", *rx.borrow());
///         }
///     });
///
///     tx.send("world")?;
/// # Ok(())
/// # }
/// ```
///
/// [`Sender`]: struct@Sender
/// [`Receiver`]: struct@Receiver
pub fn channel<T>(init: T) -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        value: RwLock::new(init),
        version: AtomicUsize::new(0),
        ref_count_rx: AtomicUsize::new(1),
        notify_rx: Notify::new(),
        notify_tx: Notify::new(),
    });

    let tx = Sender {
        shared: shared.clone(),
    };

    let rx = Receiver { shared, version: 0 };

    (tx, rx)
}

impl<T> Receiver<T> {
    /// Returns a reference to the most recently sent value
    ///
    /// Outstanding borrows hold a read lock. This means that long lived borrows
    /// could cause the send half to block. It is recommended to keep the borrow
    /// as short lived as possible.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::watch;
    ///
    /// let (_, rx) = watch::channel("hello");
    /// assert_eq!(*rx.borrow(), "hello");
    /// ```
    pub fn borrow(&self) -> Ref<'_, T> {
        let inner = self.shared.value.read().unwrap();
        Ref { inner }
    }

    /// Wait for a change notification
    ///
    /// Returns when a new value has been sent by the [`Sender`] since the last
    /// time `changed()` was called. When the `Sender` half is dropped, `Err` is
    /// returned.
    ///
    /// [`Sender`]: struct@Sender
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::watch;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let (tx, mut rx) = watch::channel("hello");
    ///
    ///     tokio::spawn(async move {
    ///         tx.send("goodbye").unwrap();
    ///     });
    ///
    ///     assert!(rx.changed().await.is_ok());
    ///     assert_eq!(*rx.borrow(), "goodbye");
    ///
    ///     // The `tx` handle has been dropped
    ///     assert!(rx.changed().await.is_err());
    /// }
    /// ```
    pub async fn changed(&mut self) -> Result<(), error::RecvError> {
        // In order to avoid a race condition, we first request a notification,
        // **then** check the current value's version. If a new version exists,
        // the notification request is dropped.
        let notified = self.shared.notify_rx.notified();

        if let Some(ret) = maybe_changed(&self.shared, &mut self.version) {
            return ret;
        }

        notified.await;

        maybe_changed(&self.shared, &mut self.version)
            .expect("[bug] failed to observe change after notificaton.")
    }
}

fn maybe_changed<T>(
    shared: &Shared<T>,
    version: &mut usize,
) -> Option<Result<(), error::RecvError>> {
    // Load the version from the state
    let state = shared.version.load(SeqCst);
    let new_version = state & !CLOSED;

    if *version != new_version {
        // Observe the new version and return
        *version = new_version;
        return Some(Ok(()));
    }

    if CLOSED == state & CLOSED {
        // All receivers have dropped.
        return Some(Err(error::RecvError(())));
    }

    None
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        let version = self.version;
        let shared = self.shared.clone();

        // No synchronization necessary as this is only used as a counter and
        // not memory access.
        shared.ref_count_rx.fetch_add(1, Relaxed);

        Receiver { version, shared }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // No synchronization necessary as this is only used as a counter and
        // not memory access.
        if 1 == self.shared.ref_count_rx.fetch_sub(1, Relaxed) {
            // This is the last `Receiver` handle, tasks waiting on `Sender::closed()`
            self.shared.notify_tx.notify_waiters();
        }
    }
}

impl<T> Sender<T> {
    /// Sends a new value via the channel, notifying all receivers.
    pub fn send(&self, value: T) -> Result<(), error::SendError<T>> {
        // This is pretty much only useful as a hint anyway, so synchronization isn't critical.
        if 0 == self.shared.ref_count_rx.load(Relaxed) {
            return Err(error::SendError { inner: value });
        }

        *self.shared.value.write().unwrap() = value;

        // Update the version. 2 is used so that the CLOSED bit is not set.
        self.shared.version.fetch_add(2, SeqCst);

        // Notify all watchers
        self.shared.notify_rx.notify_waiters();

        Ok(())
    }

    /// Completes when all receivers have dropped.
    ///
    /// This allows the producer to get notified when interest in the produced
    /// values is canceled and immediately stop doing work.
    ///
    /// # Examples
    ///
    /// ```
    /// use tokio::sync::watch;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let (tx, rx) = watch::channel("hello");
    ///
    ///     tokio::spawn(async move {
    ///         // use `rx`
    ///         drop(rx);
    ///     });
    ///
    ///     // Waits for `rx` to drop
    ///     tx.closed().await;
    ///     println!("the `rx` handles dropped")
    /// }
    /// ```
    pub async fn closed(&self) {
        let notified = self.shared.notify_tx.notified();

        if self.shared.ref_count_rx.load(Relaxed) == 0 {
            return;
        }

        notified.await;
        debug_assert_eq!(0, self.shared.ref_count_rx.load(Relaxed));
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.shared.version.fetch_or(CLOSED, SeqCst);
        self.shared.notify_rx.notify_waiters();
    }
}

// ===== impl Ref =====

impl<T> ops::Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner.deref()
    }
}
