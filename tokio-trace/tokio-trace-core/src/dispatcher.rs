//! Dispatches trace events to `Subscriber`s.c
use {
    callsite, span,
    subscriber::{self, Subscriber},
    Event, Metadata,
};

use std::{
    any::Any,
    cell::{Cell, RefCell},
    fmt,
    sync::{Arc, Weak},
};

/// `Dispatch` trace data to a [`Subscriber`].
///
/// [`Subscriber`]: ../subscriber/trait.Subscriber.html
#[derive(Clone)]
pub struct Dispatch {
    subscriber: Arc<Subscriber + Send + Sync>,
}

thread_local! {
    static CURRENT_STATE: State = State {
        default: RefCell::new(Dispatch::none()),
        can_enter: Cell::new(true),
    };
}

/// The dispatch state of a thread.
struct State {
    /// This thread's current default dispatcher.
    default: RefCell<Dispatch>,
    /// Whether or not we can currently begin dispatching a trace event.
    ///
    /// This is set to `false` when functions such as `enter`, `exit`, `event`,
    /// and `new_span` are called on this thread's default dispatcher, to
    /// prevent further trace events triggered inside those functions from
    /// creating an infinite recursion. When we finish handling a dispatch, this
    /// is set back to `true`.
    can_enter: Cell<bool>,
}

/// A guard returned by `State::enter`, borrowing the thread's current default
/// dispatcher.
///
/// While this guard is active, additional calls to subscriber functions on the
/// default dispatcher will not be able to access the dispatch context. Dropping
/// the guard will allow the dispatch context to be re-entered.
struct Entered<'a> {
    can_enter: &'a Cell<bool>,
}

/// A guard that resets the current default dispatcher to the prior
/// default dispatcher when dropped.
struct ResetGuard(Option<Dispatch>);

/// Sets this dispatch as the default for the duration of a closure.
///
/// The default dispatcher is used when creating a new [span] or
/// [`Event`], _if no span is currently executing_. If a span is currently
/// executing, new spans or events are dispatched to the subscriber that
/// tagged that span, instead.
///
/// [span]: ../span/index.html
/// [`Subscriber`]: ../subscriber/trait.Subscriber.html
/// [`Event`]: ../event/struct.Event.html
pub fn with_default<T>(dispatcher: &Dispatch, f: impl FnOnce() -> T) -> T {
    // When this guard is dropped, the default dispatcher will be reset to the
    // prior default. Using this (rather than simply resetting after calling
    // `f`) ensures that we always reset to the prior dispatcher even if `f`
    // panics.
    let _guard = State::set_default(dispatcher.clone());
    f()
}
/// Executes a closure with a reference to this thread's current [dispatcher].
///
/// Note that calls to `get_default` should not be nested; if this function is
/// called while inside of another `get_default`, that closure will be provided
/// with `Dispatch::none` rather than the previously set dispatcher.
///
/// [dispatcher]: ../dispatcher/struct.Dispatch.html
pub fn get_default<T, F>(mut f: F) -> T
where
    F: FnMut(&Dispatch) -> T,
{
    CURRENT_STATE
        .try_with(|state| f(&state.default.borrow()))
        .unwrap_or_else(|_| f(&Dispatch::none()))
}

pub(crate) struct Registrar(Weak<Subscriber + Send + Sync>);

impl Dispatch {
    /// Returns a new `Dispatch` that discards events and spans.
    #[inline]
    pub fn none() -> Self {
        Dispatch {
            subscriber: Arc::new(NoSubscriber),
        }
    }

    /// Returns a `Dispatch` that forwards to the given [`Subscriber`].
    ///
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    pub fn new<S>(subscriber: S) -> Self
    where
        S: Subscriber + Send + Sync + 'static,
    {
        let me = Dispatch {
            subscriber: Arc::new(subscriber),
        };
        callsite::register_dispatch(&me);
        me
    }

    pub(crate) fn registrar(&self) -> Registrar {
        Registrar(Arc::downgrade(&self.subscriber))
    }

    /// Registers a new callsite with this subscriber, returning whether or not
    /// the subscriber is interested in being notified about the callsite.
    ///
    /// This calls the [`register_callsite`] function on the [`Subscriber`]
    /// that this `Dispatch` forwards to.
    ///
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`register_callsite`]: ../subscriber/trait.Subscriber.html#method.register_callsite
    #[inline]
    pub fn register_callsite(&self, metadata: &Metadata) -> subscriber::Interest {
        self.subscriber.register_callsite(metadata)
    }

    /// Record the construction of a new span, returning a new [ID] for the
    /// span being constructed.
    ///
    /// This calls the [`new_span`] function on the [`Subscriber`] that this
    /// `Dispatch` forwards to.
    ///
    /// [ID]: ../span/struct.Id.html
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`new_span`]: ../subscriber/trait.Subscriber.html#method.new_span
    #[inline]
    pub fn new_span(&self, span: &span::Attributes) -> span::Id {
        Self::if_enabled(|| self.subscriber.new_span(span))
            .unwrap_or_else(|| span::Id::from_u64(0xDEADFACE))
    }

    /// Record a set of values on a span.
    ///
    /// This calls the [`record`] function on the [`Subscriber`] that this
    /// `Dispatch` forwards to.
    ///
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`record`]: ../subscriber/trait.Subscriber.html#method.record
    #[inline]
    pub fn record(&self, span: &span::Id, values: &span::Record) {
        self.subscriber.record(span, values)
    }

    /// Adds an indication that `span` follows from the span with the id
    /// `follows`.
    ///
    /// This calls the [`record_follows_from`] function on the [`Subscriber`]
    /// that this `Dispatch` forwards to.
    ///
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`record_follows_from`]: ../subscriber/trait.Subscriber.html#method.record_follows_from
    #[inline]
    pub fn record_follows_from(&self, span: &span::Id, follows: &span::Id) {
        self.subscriber.record_follows_from(span, follows)
    }

    /// Returns true if a span with the specified [metadata] would be
    /// recorded.
    ///
    /// This calls the [`enabled`] function on the [`Subscriber`] that this
    /// `Dispatch` forwards to.
    ///
    /// [metadata]: ../metadata/struct.Metadata.html
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`enabled`]: ../subscriber/trait.Subscriber.html#method.enabled
    #[inline]
    pub fn enabled(&self, metadata: &Metadata) -> bool {
        Self::if_enabled(|| self.subscriber.enabled(metadata)).unwrap_or(false)
    }

    /// Records that an [`Event`] has occurred.
    ///
    /// This calls the [`event`] function on the [`Subscriber`] that this
    /// `Dispatch` forwards to.
    ///
    /// [`Event`]: ../event/struct.Event.html
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`event`]: ../subscriber/trait.Subscriber.html#method.event
    #[inline]
    pub fn event(&self, event: &Event) {
        Self::if_enabled(|| self.subscriber.event(event));
    }

    /// Records that a span has been can_enter.
    ///
    /// This calls the [`enter`] function on the [`Subscriber`] that this
    /// `Dispatch` forwards to.
    ///
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`event`]: ../subscriber/trait.Subscriber.html#method.event
    #[inline]
    pub fn enter(&self, span: &span::Id) {
        self.subscriber.enter(span);
    }

    /// Records that a span has been exited.
    ///
    /// This calls the [`exit`](::Subscriber::exit) function on the `Subscriber`
    /// that this `Dispatch` forwards to.
    #[inline]
    pub fn exit(&self, span: &span::Id) {
        self.subscriber.exit(span);
    }

    /// Notifies the subscriber that a [span ID] has been cloned.
    ///
    /// This function is guaranteed to only be called with span IDs that were
    /// returned by this `Dispatch`'s [`new_span`] function.
    ///
    /// This calls the [`clone_span`] function on the `Subscriber` that this
    /// `Dispatch` forwards to.
    ///
    /// [span ID]: ../span/struct.Id.html
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`clone_span`]: ../subscriber/trait.Subscriber.html#method.clone_span
    /// [`new_span`]: ../subscriber/trait.Subscriber.html#method.new_span
    #[inline]
    pub fn clone_span(&self, id: &span::Id) -> span::Id {
        self.subscriber.clone_span(&id)
    }

    /// Notifies the subscriber that a [span ID] has been dropped.
    ///
    /// This function is guaranteed to only be called with span IDs that were
    /// returned by this `Dispatch`'s [`new_span`] function.
    ///
    /// This calls the [`drop_span`]  function on the [`Subscriber`] that this
    ///  `Dispatch` forwards to.
    ///
    /// [span ID]: ../span/struct.Id.html
    /// [`Subscriber`]: ../subscriber/trait.Subscriber.html
    /// [`clone_span`]: ../subscriber/trait.Subscriber.html#method.clone_span
    /// [`new_span`]: ../subscriber/trait.Subscriber.html#method.new_span
    #[inline]
    pub fn drop_span(&self, id: span::Id) {
        self.subscriber.drop_span(id)
    }

    /// Returns `true` if this `Dispatch` forwards to a `Subscriber` of type
    /// `T`.
    #[inline]
    pub fn is<T: Any>(&self) -> bool {
        Subscriber::is::<T>(&*self.subscriber)
    }

    /// Returns some reference to the `Subscriber` this `Dispatch` forwards to
    /// if it is of type `T`, or `None` if it isn't.
    #[inline]
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        Subscriber::downcast_ref(&*self.subscriber)
    }

    /// If this dispatcher is not currently dispatching a trace event on this
    /// thread, invokes the provided function with a reference to the subscriber
    /// this dispatcher forwards to. If the subscriber is currently busy on this
    /// thread, returns `None`.
    #[inline(always)]
    fn if_enabled<F, T>(f: F) -> Option<T>
    where
        F: FnOnce() -> T,
    {
        CURRENT_STATE
            .try_with(|state| {
                if let Some(_enter) = state.enter() {
                    return Some(f());
                }
                None
            })
            .unwrap_or(None)
    }
}

impl fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.pad("Dispatch(...)")
    }
}

impl<S> From<S> for Dispatch
where
    S: Subscriber + Send + Sync + 'static,
{
    #[inline]
    fn from(subscriber: S) -> Self {
        Dispatch::new(subscriber)
    }
}

struct NoSubscriber;
impl Subscriber for NoSubscriber {
    #[inline]
    fn register_callsite(&self, _: &Metadata) -> subscriber::Interest {
        subscriber::Interest::never()
    }

    fn new_span(&self, _: &span::Attributes) -> span::Id {
        span::Id::from_u64(0xDEAD)
    }

    fn event(&self, _event: &Event) {}

    fn record(&self, _span: &span::Id, _values: &span::Record) {}

    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}

    #[inline]
    fn enabled(&self, _metadata: &Metadata) -> bool {
        false
    }

    fn enter(&self, _span: &span::Id) {}
    fn exit(&self, _span: &span::Id) {}
}

impl Registrar {
    pub(crate) fn try_register(&self, metadata: &Metadata) -> Option<subscriber::Interest> {
        self.0.upgrade().map(|s| s.register_callsite(metadata))
    }
}

// ===== impl State =====

impl State {
    /// Replaces the current default dispatcher on this thread with the provided
    /// dispatcher.Any
    ///
    /// Dropping the returned `ResetGuard` will reset the default dispatcher to
    /// the previous value.
    #[inline]
    fn set_default(new_dispatch: Dispatch) -> ResetGuard {
        let prior = CURRENT_STATE
            .try_with(|state| {
                state.can_enter.set(true);
                state.default.replace(new_dispatch)
            })
            .ok();
        ResetGuard(prior)
    }

    /// Begins recording a trace event with this thread's current
    /// default dispatcher.
    ///
    /// If this thread's dispatcher is currently recording, this returns
    /// `None`. Otherwise, a guard is returned allowing access to the
    /// thread's current `Subscriber`. Dropping the guard will reset the
    /// flag indicating that we are currently recording a trace event.
    #[inline]
    fn enter(&self) -> Option<Entered> {
        if self.can_enter.replace(false) {
            Some(Entered {
                can_enter: &self.can_enter,
            })
        } else {
            None
        }
    }
}

// ===== impl ResetGuard =====

impl Drop for ResetGuard {
    #[inline]
    fn drop(&mut self) {
        if let Some(dispatch) = self.0.take() {
            let _ = CURRENT_STATE.try_with(|state| {
                *state.default.borrow_mut() = dispatch;
            });
        }
    }
}

// ===== impl Entered =====

impl<'a> Drop for Entered<'a> {
    #[inline]
    fn drop(&mut self) {
        self.can_enter.set(true);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use {
        callsite::Callsite,
        metadata::{Level, Metadata},
        span,
        subscriber::{Interest, Subscriber},
        Event,
    };

    #[test]
    fn dispatch_is() {
        let dispatcher = Dispatch::new(NoSubscriber);
        assert!(dispatcher.is::<NoSubscriber>());
    }

    #[test]
    fn dispatch_downcasts() {
        let dispatcher = Dispatch::new(NoSubscriber);
        assert!(dispatcher.downcast_ref::<NoSubscriber>().is_some());
    }

    struct TestCallsite;
    static TEST_CALLSITE: TestCallsite = TestCallsite;
    static TEST_META: Metadata<'static> = metadata! {
        name: "test",
        target: module_path!(),
        level: Level::DEBUG,
        fields: &[],
        callsite: &TEST_CALLSITE,
    };

    impl Callsite for TestCallsite {
        fn add_interest(&self, _: Interest) {}
        fn clear_interest(&self) {}
        fn metadata(&self) -> &Metadata {
            &TEST_META
        }
    }

    #[test]
    fn events_dont_infinite_loop() {
        // This test ensures that an event triggered within a subscriber
        // won't cause an infinite loop of events.
        struct TestSubscriber;
        impl Subscriber for TestSubscriber {
            fn enabled(&self, _: &Metadata) -> bool {
                true
            }

            fn new_span(&self, _: &span::Attributes) -> span::Id {
                span::Id::from_u64(0xAAAA)
            }

            fn record(&self, _: &span::Id, _: &span::Record) {}

            fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

            fn event(&self, _: &Event) {
                static EVENTS: AtomicUsize = AtomicUsize::new(0);
                assert_eq!(
                    EVENTS.fetch_add(1, Ordering::Relaxed),
                    0,
                    "event method called twice!"
                );
                Event::dispatch(&TEST_META, &TEST_META.fields().value_set(&[]))
            }

            fn enter(&self, _: &span::Id) {}

            fn exit(&self, _: &span::Id) {}
        }

        with_default(&Dispatch::new(TestSubscriber), || {
            Event::dispatch(&TEST_META, &TEST_META.fields().value_set(&[]))
        })
    }

    #[test]
    fn spans_dont_infinite_loop() {
        // This test ensures that a span created within a subscriber
        // won't cause an infinite loop of new spans.

        fn mk_span() {
            get_default(|current| {
                current.new_span(&span::Attributes::new(
                    &TEST_META,
                    &TEST_META.fields().value_set(&[]),
                ))
            });
        }

        struct TestSubscriber;
        impl Subscriber for TestSubscriber {
            fn enabled(&self, _: &Metadata) -> bool {
                true
            }

            fn new_span(&self, _: &span::Attributes) -> span::Id {
                static NEW_SPANS: AtomicUsize = AtomicUsize::new(0);
                assert_eq!(
                    NEW_SPANS.fetch_add(1, Ordering::Relaxed),
                    0,
                    "new_span method called twice!"
                );
                mk_span();
                span::Id::from_u64(0xAAAA)
            }

            fn record(&self, _: &span::Id, _: &span::Record) {}

            fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}

            fn event(&self, _: &Event) {}

            fn enter(&self, _: &span::Id) {}

            fn exit(&self, _: &span::Id) {}
        }

        with_default(&Dispatch::new(TestSubscriber), || mk_span())
    }
}
