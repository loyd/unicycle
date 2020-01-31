#![deny(missing_docs)]
#![allow(clippy::needless_doctest_main)]
//! A scheduler for driving a large number of futures.
//!
//! Unicycle provides a collection of [Unordered] types:
//!
//! * [FuturesUnordered]
//! * [StreamsUnordered]
//! * [IndexedStreamsUnordered]
//!
//! These are async abstractions that runs a set of futures or streams which may
//! complete in any order.
//! Similarly to [FuturesUnordered][futures-rs] from the [futures crate].
//! But we aim to provide a stronger guarantee of fairness (see below), and
//! better memory locality for the futures being pollled.
//!
//! **Note:** This project is experimental. It involves some amount of unsafe and
//! possibly bad assumptions which needs to be either vetted or removed before you
//! should consider putting it in production.
//!
//! ## Features
//!
//! * `parking-lot` - To enable locking using the [parking_lot] crate (optional).
//! * `vec-safety` - Avoid relying on the assumption that `&mut Vec<T>` can be
//!   safely coerced to `&mut Vec<U>` if `T` and `U` have an identical memory
//!   layouts (enabled by default, [issue #1]).
//!
//! [issue #1]: https://github.com/udoprog/unicycle/issues/1
//! [parking_lot]: https://crates.io/crates/parking_lot
//!
//! ## Examples
//!
//! ```rust
//! use tokio::{stream::StreamExt as _, time};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut futures = unicycle::FuturesUnordered::new();
//!
//!     futures.push(time::delay_for(Duration::from_secs(2)));
//!     futures.push(time::delay_for(Duration::from_secs(3)));
//!     futures.push(time::delay_for(Duration::from_secs(1)));
//!
//!     while let Some(_) = futures.next().await {
//!         println!("tick");
//!     }
//!
//!     println!("done!");
//! }
//! ```
//!
//! ## Fairness
//!
//! You can think of abstractions like Unicycle as schedulers. They are provided a
//! set of child tasks, and try to do their best to drive them to completion. In
//! this regard, it's interesting to talk about _fairness_ in how the tasks are
//! being driven.
//!
//! The current implementation of [FuturesUnordered] maintains a queue of tasks
//! interested in waking up. As a task is woken up, it's added to the head of this
//! queue to signal its interest.
//! When [FuturesUnordered] is being polled, it drains this queue in a loop and
//! polls the associated task.
//! This process has a side effect of tasks who aggressively signal interest in
//! waking up will receive priority and be polled more frequently, since there is a
//! higher chance that while the queue is being drained, their interest will be
//! re-added to the queue.
//! This can lead to instances where a small number of tasks can can cause the
//! polling loop of [FuturesUnordered] to [spin abnormally].
//! This issue was [reported by Jon Gjengset], and improved on by [limiting the
//! amount FuturesUnordered is allowed to spin].
//!
//! Unicycle addresses this by limiting how frequently a child task may be polled
//! per _polling cycle_.
//! This is done by tracking polling interest in two separate sets.
//! Once we are polled, we swap out the active set, then take the swapped out set
//! and use as a basis for what to poll in order, but we limit ourselves to only
//! poll _once_ per child task.
//! Additional wakeups are only registered in the swapped in set which will be
//! polled the next cycle.
//!
//! This way we hope to achieve a higher degree of fairness, never favoring the
//! behavior of one particular task.
//!
//! For more details, see the _Architecture_ section below.
//!
//! [spin abnormally]: https://github.com/udoprog/unicycle/blob/master/tests/spinning_futures_unordered.rs
//! [limiting the amount FuturesUnordered is allowed to spin]: https://github.com/rust-lang/futures-rs/pull/2049
//! [reported by Jon Gjengset]: https://github.com/rust-lang/futures-rs/issues/2047
//!
//! ## Architecture
//!
//! The [Unordered] type stores all futures being polled in a [PinSlab] (Inspired by
//! the [slab] crate).
//! A slab is capable of utomatically reclaiming storage at low cost, and will
//! maintain decent memory locality.
//! A [PinSlab] is different from a [Slab] in how it allocates the memory regions it
//! uses to store objects.
//! While a regular [Slab] is simply backed by a vector which grows as appropriate,
//! this approach is not viable for pinning, since it would cause the objects to
//! move while being reallocated.
//! Instead [PinSlab] maintains a growable collection of fixed-size memory regions,
//! allowing it to store and reference immovable objects through the [pin API].
//! Each future inserted into the slab is assigned an _index_, which we will be
//! using below.
//! We now call the inserted future a _task_, and you can think of this index as a
//! unique task identifier.
//!
//! [slab]: https://github.com/carllerche/slab
//! [pin API]: https://doc.rust-lang.org/std/pin/index.html
//!
//! Next to the slab we maintain two [BitSet]s, one _active_ and one _alternate_.
//! When a task registers interest in waking up, the bit associated with its index
//! is set in the active set, and the latest waker passed into [Unordered] is called
//! to wake it up.
//! Once [Unordered] is polled, it atomically swaps the active and alternate
//! [BitSet]s, waits until it has exclusive access to the now _alternate_ [BitSet], and
//! drains it from all the indexes which have been flagged to determine which tasks
//! to poll.
//! Each task is then polled _once_ in order.
//! If the task is [Ready], its result is yielded.
//! After we receive control again, we continue draining the alternate set in this manner, until it is empty.
//! When this is done we yield once, then we start the cycle over again.
//!
//! [Ready]: https://doc.rust-lang.org/std/task/enum.Poll.html
//! [Slab]: https://docs.rs/slab/latest/slab/struct.Slab.html
//! [futures-rs]: https://docs.rs/futures/latest/futures/stream/struct.FuturesUnordered.html
//! [futures crate]: https://docs.rs/futures/latest/futures

pub use self::bit_set::{AtomicBitSet, BitSet, Drain, DrainSnapshot, Iter};
pub use self::pin_slab::PinSlab;
use self::wake_set::{LocalWakeSet, SharedWakeSet, WakeSet};
use self::waker::SharedWaker;
use futures_core::Stream;
use std::{
    future::Future,
    iter, marker, mem,
    pin::Pin,
    ptr,
    sync::Arc,
    task::{Context, Poll},
};

pub mod bit_set;
mod lock;
mod pin_slab;
mod wake_set;
mod waker;

/// A container for an unordered collection of [Future]s.
///
/// # Examples
///
/// ```rust,no_run
/// use tokio::{stream::StreamExt as _, time};
/// use std::time::Duration;
///
/// #[tokio::main]
/// async fn main() {
///     let mut futures = unicycle::FuturesUnordered::new();
///
///     futures.push(time::delay_for(Duration::from_secs(2)));
///     futures.push(time::delay_for(Duration::from_secs(3)));
///     futures.push(time::delay_for(Duration::from_secs(1)));
///
///     while let Some(_) = futures.next().await {
///         println!("tick");
///     }
///
///     println!("done!");
/// }
/// ```
pub type FuturesUnordered<T> = Unordered<T, Futures>;

/// A container for an unordered collection of [Stream]s.
///
/// # Examples
///
/// ```rust,no_run
/// use tokio::{net::TcpListener, stream::StreamExt as _, time};
/// use tokio_util::codec::{Framed, LengthDelimitedCodec};
/// use std::error::Error;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn Error>> {
///     let mut listener = TcpListener::bind("127.0.0.1:8080").await?;
///     let mut clients = unicycle::StreamsUnordered::new();
///
///     loop {
///         tokio::select! {
///             result = listener.accept() => {
///                 let (stream, _) = result?;
///                 clients.push(Framed::new(stream, LengthDelimitedCodec::new()));
///             },
///             Some(frame) = clients.next() => {
///                 println!("received frame: {:?}", frame);
///             }
///         }
///     }
/// }
/// ```
pub type StreamsUnordered<T> = Unordered<T, Streams>;

/// A container for an unordered collection of [Stream]s, which also yields the
/// index that produced the next item.
///
/// # Examples
///
/// ```rust,no_run
/// use tokio::{net::TcpListener, stream::StreamExt as _, time};
/// use tokio_util::codec::{Framed, LengthDelimitedCodec};
/// use std::error::Error;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn Error>> {
///     let mut listener = TcpListener::bind("127.0.0.1:8080").await?;
///     let mut clients = unicycle::IndexedStreamsUnordered::new();
///
///     loop {
///         tokio::select! {
///             result = listener.accept() => {
///                 let (stream, _) = result?;
///                 clients.push(Framed::new(stream, LengthDelimitedCodec::new()));
///             },
///             Some((index, frame)) = clients.next() => {
///                 match frame {
///                     Some(frame) => println!("{}: received frame: {:?}", index, frame),
///                     None => println!("{}: client disconnected", index),
///                 }
///             }
///         }
///     }
/// }
/// ```
pub type IndexedStreamsUnordered<T> = Unordered<T, IndexedStreams>;

macro_rules! ready {
    ($expr:expr) => {
        match $expr {
            Poll::Ready(value) => value,
            Poll::Pending => return Poll::Pending,
        }
    };
}

/// Data that is shared across all sub-tasks.
struct Shared {
    /// The currently registered parent waker.
    waker: SharedWaker,
    /// The currently registered wake set.
    wake_set: SharedWakeSet,
}

impl Shared {
    /// Construct new shared data.
    fn new() -> Self {
        Self {
            waker: SharedWaker::new(),
            wake_set: SharedWakeSet::new(),
        }
    }

    /// Swap the active wake set with the alternate one.
    /// Also makes sure that the capacity of the active bitset is updated if the
    /// alternate one has.
    ///
    /// # Safety
    ///
    /// Caller must be assured that they are the only one who is attempting to
    /// swap out the wake sets.
    unsafe fn swap_active<'a>(
        &self,
        cx: &mut Context<'_>,
        alternate: &'a mut *mut WakeSet,
        active_capacity: &mut usize,
    ) -> Poll<&'a mut LocalWakeSet> {
        let wake_last = (**alternate).as_local_mut();
        let capacity = wake_last.set.capacity();

        if !wake_last.set.is_empty() && *active_capacity == capacity {
            return Poll::Ready(wake_last);
        }

        // Note: We defer swapping the waker until we are here since we `wake_by_ref` when
        // reading results, and if we don't have any child tasks (slab is empty) no one would
        // benefit from an update anyways.
        if !self.waker.swap(cx.waker()) {
            return Poll::Pending;
        }

        // Note: at this point we should have had at least one element
        // added to the slab.
        debug_assert!(capacity > 0);

        // Safety: This drop here is important to avoid aliasing the pointer to
        // the alternate, soon-to-be active set.
        drop(wake_last);

        // Unlock. At this position, if someone adds an element to the wake set they are
        // also bound to call wake, which will cause us to wake up.
        //
        // There is a race going on between locking and unlocking, and it's beneficial
        // for child tasks to observe the locked state of the wake set so they refetch
        // the other set instead of having to wait until another wakeup.
        (**alternate).unlock_exclusive();

        let next = mem::replace(alternate, ptr::null_mut());
        *alternate = self.wake_set.swap(next);

        // Make sure no one else is using the alternate wake.
        //
        // Safety: We are the only one swapping alternate, so at
        // this point we know that we have access to the most recent
        // active set. We _must_ call lock_exclusive before we
        // can punt this into a mutable reference though, because at
        // this point inner futures will still have access to references
        // to it (under a lock!). We must wait for these to expire.
        //
        // We also unfortunately can't yield here, because we've swapped the
        // alternate set which could be used when pushing to the set.
        (**alternate).lock_exclusive();

        // Safety: While this is live we must _not_ mess with
        // `alternate` in any way.
        let wake_set = (**alternate).as_local_mut();

        // Make sure the capacity of the active set matches the now alternate
        // set.
        wake_set.set.reserve(capacity);
        *active_capacity = wake_set.set.capacity();

        Poll::Ready(wake_set)
    }
}

trait Sentinel {}
/// Sentinel type for streams.
///
/// [Unordered] instances which handle futures have the signature
/// `Unordered<T, Streams>`, since it allows for a different implementation of
/// [Stream].
pub struct Streams(());

impl Sentinel for Streams {}

/// Sentinel type for futures.
///
/// [Unordered] instances which handle futures have the signature
/// `Unordered<T, Futures>`, since it allows for a different implementation of
/// [Stream].
pub struct Futures(());

impl Sentinel for Futures {}

/// Sentinel type for streams which are indexed - for each value they yield,
/// they also yield the task identifier associated with them.
///
/// [Unordered] instances which handle futures have the signature
/// `Unordered<T, IndexedStreams>`, since it allows for a different
/// implementation of [Stream].
pub struct IndexedStreams(());

impl Sentinel for IndexedStreams {}

/// A container for an unordered collection of [Future]s or [Stream]s.
///
/// You should use one of the following type aliases to construct it:
/// * [FuturesUnordered]
/// * [StreamsUnordered]
/// * [IndexedStreamsUnordered]
///
/// # Examples
///
/// ```rust,no_run
/// use tokio::{stream::StreamExt as _, time};
/// use std::time::Duration;
///
/// #[tokio::main]
/// async fn main() {
///     let mut futures = unicycle::FuturesUnordered::new();
///
///     futures.push(time::delay_for(Duration::from_secs(2)));
///     futures.push(time::delay_for(Duration::from_secs(3)));
///     futures.push(time::delay_for(Duration::from_secs(1)));
///
///     while let Some(_) = futures.next().await {
///         println!("tick");
///     }
///
///     println!("done!");
/// }
/// ```
pub struct Unordered<F, S> {
    /// Slab of futures being polled.
    /// They need to be pinned on the heap, since the slab might grow to
    /// accomodate more futures.
    slab: PinSlab<F>,
    /// Shared parent waker.
    /// Includes the current wake target. Each time we poll, we swap back and
    /// forth between this and `alternate`.
    shared: Arc<Shared>,
    /// Alternate wake set, used for growing the existing set when futures are
    /// added. This is then swapped out with the active set to receive polls.
    alternate: *mut WakeSet,
    /// The capacity of the active bit set.
    ///
    /// This is used to determine if we need to swap out the active set in case
    /// the alternate has grown. We store it locally instead of accessing it
    /// through `shared` since it's a hot field to access.
    active_capacity: usize,
    /// Marker for the sentinel.
    _marker: marker::PhantomData<S>,
}

unsafe impl<T, S> Send for Unordered<T, S> {}
unsafe impl<T, S> Sync for Unordered<T, S> {}

impl<T, S> Unpin for Unordered<T, S> {}

impl<T> FuturesUnordered<T> {
    /// Construct a new, empty [FuturesUnordered].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::FuturesUnordered;
    ///
    /// let mut futures = FuturesUnordered::new();
    /// assert!(futures.is_empty());
    ///
    /// futures.push(async { 42 });
    /// ```
    pub fn new() -> Self {
        Self::new_internal()
    }
}

impl<T> StreamsUnordered<T> {
    /// Construct a new, empty [StreamsUnordered].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::StreamsUnordered;
    /// use tokio::stream::{StreamExt as _, iter};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut streams = StreamsUnordered::new();
    ///     assert!(streams.is_empty());
    ///
    ///     streams.push(iter(vec![1, 2, 3, 4]));
    ///     streams.push(iter(vec![5, 6, 7, 8]));
    ///
    ///     let mut received = Vec::new();
    ///
    ///     while let Some(value) = streams.next().await {
    ///         received.push(value);
    ///     }
    ///
    ///     assert_eq!(vec![1, 5, 2, 6, 3, 7, 4, 8], received);
    /// }
    /// ```
    pub fn new() -> Self {
        Self::new_internal()
    }
}

impl<F> IndexedStreamsUnordered<F> {
    /// Construct a new, empty [IndexedStreamsUnordered].
    ///
    /// This is the same as [StreamsUnordered], except that it yields the index
    /// of the stream who'se value was just yielded, alongside the yielded
    /// value.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::IndexedStreamsUnordered;
    /// use tokio::stream::{StreamExt as _, iter};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut streams = IndexedStreamsUnordered::new();
    ///     assert!(streams.is_empty());
    ///
    ///     streams.push(iter(vec![1, 2]));
    ///     streams.push(iter(vec![5, 6]));
    ///
    ///     let mut received = Vec::new();
    ///
    ///     while let Some(value) = streams.next().await {
    ///         received.push(value);
    ///     }
    ///
    ///     assert_eq!(
    ///         vec![
    ///             (0, Some(1)),
    ///             (1, Some(5)),
    ///             (0, Some(2)),
    ///             (1, Some(6)),
    ///             (0, None),
    ///             (1, None)
    ///         ],
    ///         received
    ///     );
    /// }
    /// ```
    pub fn new() -> Self {
        Self::new_internal()
    }
}

impl<T, S> Unordered<T, S> {
    #[inline(always)]
    fn new_internal() -> Self {
        Self {
            slab: PinSlab::new(),
            shared: Arc::new(Shared::new()),
            alternate: Box::into_raw(Box::new(WakeSet::locked())),
            active_capacity: 0,
            _marker: marker::PhantomData,
        }
    }

    /// Test if the collection of futures is empty.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::FuturesUnordered;
    ///
    /// let mut futures = FuturesUnordered::<tokio::time::Delay>::new();
    /// assert!(futures.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.slab.is_empty()
    }

    /// Push the given future or stream to [Unordered] and return its task
    /// index.
    ///
    /// Newly added futures are guaranteed to be polled, but there is no
    /// guarantee in which order this will happen.
    ///
    /// Pushed tasks are pinned by the [Unordered] collection automatically.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::FuturesUnordered;
    ///
    /// let mut futures = FuturesUnordered::new();
    /// assert!(futures.is_empty());
    /// futures.push(async { 42 });
    /// assert!(!futures.is_empty());
    /// ```
    pub fn push(&mut self, future: T) -> usize {
        let index = self.slab.insert(future);
        // Safety: At this point we know we have exclusive access to the set.
        let alternate = unsafe { (*self.alternate).as_local_mut() };
        alternate.set.set(index);
        index
    }

    /// Get a pinned mutable reference to the stream or future at the given
    /// index.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::FuturesUnordered;
    /// use futures::future::poll_fn;
    /// use std::future::Future as _;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut futures = FuturesUnordered::new();
    ///     let index = futures.push(async { 42 });
    ///
    ///     let result = poll_fn(|cx| {
    ///         futures.get_pin_mut(index).expect("expected future").poll(cx)
    ///     }).await;
    ///
    ///     assert_eq!(result, 42);
    /// }
    /// ```
    pub fn get_pin_mut(&mut self, index: usize) -> Option<Pin<&mut T>> {
        self.slab.get_pin_mut(index)
    }

    /// Get a mutable reference to the stream or future at the given index.
    /// Requires that the stores stream or future is [Unpin].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use unicycle::FuturesUnordered;
    /// use futures::future::{ready, poll_fn};
    /// use std::{pin::Pin, future::Future as _};
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut futures = FuturesUnordered::new();
    ///     let index = futures.push(ready(42));
    ///
    ///     let result = poll_fn(|cx| {
    ///         Pin::new(futures.get_mut(index).expect("expected future")).poll(cx)
    ///     }).await;
    ///
    ///     assert_eq!(result, 42);
    /// }
    /// ```
    pub fn get_mut(&mut self, index: usize) -> Option<&mut T>
    where
        T: Unpin,
    {
        self.slab.get_mut(index)
    }
}

impl<T> Default for Unordered<T, Futures> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, S> Drop for Unordered<T, S> {
    fn drop(&mut self) {
        // Cancel all child futures in an attempt to prevent them from
        // attempting to call wake on the shared wake set.
        self.slab.clear();

        // We intend to drop both wake sets. Therefore we need exclusive access
        // to both wakers. Unfortunately that means that at this point, any call
        // to wakes will have to serialize behind the shared wake set while the
        // alternate set is being dropped.
        let _write = self.shared.wake_set.prevent_drop_write();

        // Safety: we uniquely own `alternate`, so we are responsible for
        // dropping it. This is asserted when we swap it out during a poll by
        // calling WakeSet::lock_exclusive. We are also the _only_ one
        // swapping `wake_alternative`, so we know that can't happen here.
        unsafe {
            drop(Box::from_raw(self.alternate));
        }
    }
}

impl<T> Stream for Unordered<T, Futures>
where
    T: Future,
{
    type Item = T::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self {
            ref mut slab,
            ref shared,
            ref mut alternate,
            ref mut active_capacity,
            ..
        } = *self.as_mut();

        if slab.is_empty() {
            // Nothing to poll, nothing to add. End the stream since we don't have work to do.
            return Poll::Ready(None);
        }

        // Safety: We have exclusive access to Unordered, which is the only
        // implementation that is trying to swap the wake sets.
        let wake_last = ready!(unsafe { shared.swap_active(cx, alternate, active_capacity) });

        for index in wake_last.set.drain() {
            // NB: Since we defer pollables a little, a future might
            // have been polled and subsequently removed from the slab.
            // So we don't treat this as an error here.
            // If on the other hand it was removed _and_ re-added, we have
            // a case of a spurious poll. Luckily, that doesn't bother a
            // future much.
            let fut = match slab.get_pin_mut(index) {
                Some(fut) => fut,
                None => continue,
            };

            // Construct a new lightweight waker only capable of waking by
            // reference, with referential access to `shared`.
            let result = self::waker::poll_with_ref(shared, index, move |cx| fut.poll(cx));

            if let Poll::Ready(result) = result {
                let removed = slab.remove(index);
                debug_assert!(removed);
                cx.waker().wake_by_ref();
                return Poll::Ready(Some(result));
            }
        }

        if slab.is_empty() {
            return Poll::Ready(None);
        }

        // We have successfully polled the last snapshot.
        // Yield and make sure that we are polled again.
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl<T> Stream for Unordered<T, Streams>
where
    T: Stream,
{
    type Item = T::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self {
            ref mut slab,
            ref shared,
            ref mut alternate,
            ref mut active_capacity,
            ..
        } = *self.as_mut();

        if slab.is_empty() {
            // Nothing to poll, nothing to add. End the stream since we don't have work to do.
            return Poll::Ready(None);
        }

        // Safety: We have exclusive access to Unordered, which is the only
        // implementation that is trying to swap the wake sets.
        let wake_last = ready!(unsafe { shared.swap_active(cx, alternate, active_capacity) });

        for index in wake_last.set.drain() {
            // NB: Since we defer pollables a little, a future might
            // have been polled and subsequently removed from the slab.
            // So we don't treat this as an error here.
            // If on the other hand it was removed _and_ re-added, we have
            // a case of a spurious poll. Luckily, that doesn't bother a
            // future much.
            let stream = match slab.get_pin_mut(index) {
                Some(stream) => stream,
                None => continue,
            };

            // Construct a new lightweight waker only capable of waking by
            // reference, with referential access to `shared`.
            let result = self::waker::poll_with_ref(shared, index, move |cx| stream.poll_next(cx));

            if let Poll::Ready(result) = result {
                match result {
                    Some(value) => {
                        cx.waker().wake_by_ref();
                        shared.wake_set.wake(index);
                        return Poll::Ready(Some(value));
                    }
                    None => {
                        let removed = slab.remove(index);
                        debug_assert!(removed);
                    }
                }
            }
        }

        // We have successfully polled the last snapshot.
        // Yield and make sure that we are polled again.
        if slab.is_empty() {
            return Poll::Ready(None);
        }

        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl<T> Stream for IndexedStreamsUnordered<T>
where
    T: Stream,
{
    type Item = (usize, Option<T::Item>);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Self {
            ref mut slab,
            ref shared,
            ref mut alternate,
            ref mut active_capacity,
            ..
        } = *self.as_mut();

        if slab.is_empty() {
            // Nothing to poll, nothing to add. End the stream since we don't have work to do.
            return Poll::Ready(None);
        }

        // Safety: We have exclusive access to Unordered, which is the only
        // implementation that is trying to swap the wake sets.
        let wake_last = ready!(unsafe { shared.swap_active(cx, alternate, active_capacity) });

        for index in wake_last.set.drain() {
            // NB: Since we defer pollables a little, a future might
            // have been polled and subsequently removed from the slab.
            // So we don't treat this as an error here.
            // If on the other hand it was removed _and_ re-added, we have
            // a case of a spurious poll. Luckily, that doesn't bother a
            // future much.
            let stream = match slab.get_pin_mut(index) {
                Some(stream) => stream,
                None => continue,
            };

            // Construct a new lightweight waker only capable of waking by
            // reference, with referential access to `shared`.
            let result = self::waker::poll_with_ref(shared, index, move |cx| stream.poll_next(cx));

            if let Poll::Ready(result) = result {
                match result {
                    Some(value) => {
                        cx.waker().wake_by_ref();
                        shared.wake_set.wake(index);
                        return Poll::Ready(Some((index, Some(value))));
                    }
                    None => {
                        cx.waker().wake_by_ref();
                        let removed = slab.remove(index);
                        debug_assert!(removed);
                        return Poll::Ready(Some((index, None)));
                    }
                }
            }
        }

        // We have successfully polled the last snapshot.
        // Yield and make sure that we are polled again.
        if slab.is_empty() {
            return Poll::Ready(None);
        }

        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

impl<T, S> iter::Extend<T> for Unordered<T, S> {
    fn extend<I>(&mut self, iter: I)
    where
        I: IntoIterator<Item = T>,
    {
        for value in iter {
            self.push(value);
        }
    }
}

impl<T> iter::FromIterator<T> for FuturesUnordered<T>
where
    T: Future,
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut futures = FuturesUnordered::new();
        futures.extend(iter);
        futures
    }
}

impl<T> iter::FromIterator<T> for StreamsUnordered<T>
where
    T: Stream,
{
    #[inline]
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut streams = StreamsUnordered::new();
        streams.extend(iter);
        streams
    }
}
