//! Geese executor is a runtime for futures, integrated into the Geese event system. It
//! provides the ability to perform asynchronous work and then send the results back via events.
//! Each time the `geese_executor::notify::Poll` event is raised, each future will be polled
//! a single time, and any events raised by futures will be broadcast to all other systems.
//! 
//! A simple example of executor usage is provided below.
//! 
//! ```
//! use geese::*;
//! use geese_executor::*;
//! 
//! struct A {
//!     cancellation: CancellationToken,
//!     ctx: GeeseContextHandle,
//!     result: Option<i32>
//! }
//! 
//! impl A {
//!     async fn do_background_work() -> i32 {
//!         // Do some awaiting
//!         42
//!     }
//! 
//!     fn handle_result(&mut self, future_result: &i32) {
//!         self.result = Some(*future_result);
//!     }
//! }
//! 
//! impl GeeseSystem for A {
//!     fn new(ctx: GeeseContextHandle) -> Self {
//!         let cancellation = CancellationToken::default();
//!         let result = None;
//! 
//!         ctx.system::<GeeseExecutor>().spawn_event(Self::do_background_work())
//!             .with_cancellation(&cancellation);
//! 
//!         Self { cancellation, ctx, result }
//!     }
//! 
//!     fn register(with: &mut GeeseSystemData<Self>) {
//!         with.dependency::<GeeseExecutor>();
//! 
//!         with.event(Self::handle_result);
//!     }
//! }
//! 
//! let mut ctx = GeeseContext::default();
//! ctx.raise_event(geese::notify::AddSystem::new::<A>());
//! ctx.flush_events();
//! ctx.raise_event(geese_executor::notify::Poll);
//! assert_eq!(Some(42), ctx.system::<A>().result);
//! ```

#![deny(warnings)]

use dummy_waker::*;
use geese::*;
use std::any::*;
use std::cell::*;
use std::future::*;
use std::mem::*;
use std::ops::*;
use std::pin::*;
use std::sync::*;
use std::sync::atomic::*;
use std::task::*;
use takecell::*;

/// Provides the ability to cancel an asynchronous operation. This token
/// cancels automatically when dropped.
#[derive(Debug, PartialEq, Eq)]
pub struct CancellationToken(CancellationTokenListener);

impl CancellationToken {
    /// Cancels the operation.
    pub fn cancel(self) {
        drop(self);
    }

    /// Drops the cancellation token without cancelling the event.
    /// Any listeners will always observe that the event has not been cancelled.
    pub fn forget(mut self) {
        self.0 = CancellationTokenListener(Arc::default());
    }
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self(CancellationTokenListener(Arc::default()))
    }
}

impl Deref for CancellationToken {
    type Target = CancellationTokenListener;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for CancellationToken {
    fn drop(&mut self) {
        self.0.0.store(true, Ordering::Release);
    }
}

/// Provides the ability to query whether an operation has been cancelled.
#[derive(Clone, Debug)]
pub struct CancellationTokenListener(Arc<AtomicBool>);

impl CancellationTokenListener {
    /// Whether the operation has been cancelled.
    pub fn canceled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl PartialEq for CancellationTokenListener {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CancellationTokenListener {}

/// Stores a group of events in order to inject them into a `geese` context.
#[derive(Debug, Default)]
pub struct EventSink {
    events: Arc<RefCell<Vec<Box<dyn Any>>>>
}

impl EventSink {
    /// Places the given dynamically-typed event into the event queue.
    pub fn raise_boxed_event(&self, event: Box<dyn Any>) {
        self.events.borrow_mut().push(event);
    }

    /// Places the given event into the event queue.
    pub fn raise_event<T: 'static>(&self, event: T) {
        self.raise_boxed_event(Box::new(event));
    }

    /// Creates a copy of this event sink.
    fn clone(&self) -> Self {
        Self {
            events: self.events.clone()
        }
    }

    /// Drains all of the events into a `geese` context's event queue.
    fn drain(&self, ctx: &GeeseContextHandle) {
        for event in take(&mut *self.events.borrow_mut()) {
            ctx.raise_boxed_event(event);
        }
    }
}

/// Executes futures on the Geese event thread and converts their
/// results into events, injecting them back into the event system.
pub struct GeeseExecutor {
    ctx: GeeseContextHandle,
    default_cancellation: CancellationToken,
    futures: Vec<FutureHolder>,
    sink: EventSink
}

impl GeeseExecutor {
    /// Spawns a new future for asynchronous execution.
    pub fn spawn(&self, f: impl 'static + Future<Output = ()>) -> FutureBuilder<'_> {
        FutureBuilder { executor: self, holder: TakeOwnCell::new(FutureHolder {
                cancellation: (*self.default_cancellation).clone(),
                future: Box::pin(f)
            })
        }
    }

    /// Spawns a new future for asynchronous execution, and raises the future's result
    /// as a `geese` event.
    pub fn spawn_event<T: 'static>(&self, f: impl 'static + Future<Output = T>) -> FutureBuilder<'_> {
        self.spawn_with_sink(move |sink| async move { sink.raise_event(f.await) })
    }

    /// Spawns a new future for asynchronous execution, and provides an `EventSink` for sending
    /// events back to the `geese` context.
    pub fn spawn_with_sink<R: 'static + Future<Output = ()>>(&self, f: impl FnOnce(EventSink) -> R) -> FutureBuilder<'_> {
        let sink = self.sink.clone();
        self.spawn(f(sink))
    }
    
    /// Emits all of the events created by futures.
    fn emit_events(&mut self) {
        self.sink.drain(&self.ctx);
    }

    /// Polls all current futures, and removes cancelled ones from running.
    fn poll_futures(&mut self) {
        let wake = dummy_waker();
        let mut ctx = Context::from_waker(&wake);

        let old_futures = take(&mut self.futures);
        for mut future in old_futures {
            if !future.cancellation.canceled() {
                match Pin::new(&mut future.future).poll(&mut ctx) {
                    Poll::Pending => { self.futures.push(future) },
                    _ => {}
                }
            }
        }
    }

    /// Polls all currently-registered futures and emits any events
    /// that the futures have created.
    fn poll_emit(&mut self, _: &notify::Poll) {
        self.poll_futures();
        self.emit_events();
    }

    /// Spawns a future for event polling.
    fn spawn_future(&mut self, event: &on::SpawnFuture) {
        self.futures.push(event.0.take().expect("The future was already processed."));
    }
}

impl GeeseSystem for GeeseExecutor {
    fn new(ctx: GeeseContextHandle) -> Self {
        let default_cancellation = CancellationToken::default();
        let futures = Vec::new();
        let sink = EventSink::default();
        
        Self { default_cancellation, ctx, futures, sink }
    }

    fn register(with: &mut GeeseSystemData<Self>) {
        with.event(Self::poll_emit);
        with.event(Self::spawn_future);
    }
}

/// Provides the ability to customize how a future is executed. When
/// an instance of this builder is dropped, the future is spawned
/// on the executor's queue.
pub struct FutureBuilder<'a> {
    /// The executor upon which the future will be spawned.
    executor: &'a GeeseExecutor,
    /// A holder that describes the future.
    holder: TakeOwnCell<FutureHolder>
}

impl<'a> FutureBuilder<'a> {
    /// Adds a cancellation token to this future.
    pub fn with_cancellation(mut self, token: &CancellationToken) -> Self {
        self.holder.get().expect("Future was already taken.").cancellation = (*token).clone();
        self
    }
}

impl<'a> Drop for FutureBuilder<'a> {
    fn drop(&mut self) {
        self.executor.ctx.raise_event(on::SpawnFuture(TakeOwnCell::new(self.holder.take().expect("Future was already taken."))));
    }
}

/// Holds a future's data so that it may be polled to completion.
struct FutureHolder {
    /// A token which allows the future to be cancelled.
    pub cancellation: CancellationTokenListener,
    pub future: Pin<Box<dyn Future<Output = ()>>>
}

/// The set of events to which this module responds.
pub mod notify {
    /// Instructs the event executor to poll all registered futures a single time.
    #[derive(Copy, Clone, Debug, Default)]
    pub struct Poll;
}

/// The set of events which this module raises.
pub(super) mod on {
    use super::*;

    /// Instructs the event executor to start polling the provided future.
    pub(super) struct SpawnFuture(pub TakeOwnCell<FutureHolder>);
}