use std::prelude::v1::*;

use std::cell::Cell;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::{Arc, Once, ONCE_INIT};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use {Future, Stream, Sink, Poll, Async, StartSend, AsyncSink};
use super::core;
use super::{BorrowedTask, NotifyHandle, Spawn, spawn, Notify, UnsafeNotify};

mod unpark_mutex;
pub use self::unpark_mutex::UnparkMutex;

mod data;
pub use self::data::*;

mod task_rc;
#[allow(deprecated)]
#[cfg(feature = "with-deprecated")]
pub use self::task_rc::TaskRc;

pub use task_impl::core::init;

thread_local!(static CURRENT_TASK: Cell<*mut u8> = Cell::new(ptr::null_mut()));

static INIT: Once = ONCE_INIT;

pub fn get_ptr() -> Option<*mut u8> {
    // Since this condition will always return true when TLS task storage is
    // used (the default), the branch predictor will be able to optimize the
    // branching and a dynamic dispatch will be avoided, which makes the
    // compiler happier.
    if core::is_get_ptr(0x1) {
        Some(CURRENT_TASK.with(|c| c.get()))
    } else {
        core::get_ptr()
    }
}

fn tls_slot() -> *const Cell<*mut u8> {
    CURRENT_TASK.with(|c| c as *const _)
}

pub fn set<'a, F, R>(task: &BorrowedTask<'a>, f: F) -> R
    where F: FnOnce() -> R
{
    // Lazily initialize the get / set ptrs
    //
    // Note that we won't actually use these functions ever, we'll instead be
    // testing the pointer's value elsewhere and calling our own functions.
    INIT.call_once(|| unsafe {
        let get = mem::transmute::<usize, _>(0x1);
        let set = mem::transmute::<usize, _>(0x2);
        init(get, set);
    });

    // Same as above.
    if core::is_get_ptr(0x1) {
        struct Reset(*const Cell<*mut u8>, *mut u8);

        impl Drop for Reset {
            #[inline]
            fn drop(&mut self) {
                unsafe {
                    (*self.0).set(self.1);
                }
            }
        }

        unsafe {
            let slot = tls_slot();
            let _reset = Reset(slot, (*slot).get());
            (*slot).set(task as *const _ as *mut u8);
            f()
        }
    } else {
        core::set(task, f)
    }
}

#[derive(Copy, Clone)]
#[allow(deprecated)]
pub enum BorrowedUnpark<'a> {
    Old(&'a Arc<Unpark>),
    New(core::BorrowedUnpark<'a>),
}

#[derive(Copy, Clone)]
#[allow(deprecated)]
pub enum BorrowedEvents<'a> {
    None,
    One(&'a UnparkEvent, &'a BorrowedEvents<'a>),
}

#[derive(Clone)]
pub enum TaskUnpark {
    #[allow(deprecated)]
    Old(Arc<Unpark>),
    New(core::TaskUnpark),
}

#[derive(Clone)]
#[allow(deprecated)]
pub enum UnparkEvents {
    None,
    One(UnparkEvent),
    Many(Box<[UnparkEvent]>),
}

impl<'a> BorrowedUnpark<'a> {
    #[inline]
    pub fn new(f: &'a Fn() -> NotifyHandle, id: usize) -> BorrowedUnpark<'a> {
        BorrowedUnpark::New(core::BorrowedUnpark::new(f, id))
    }

    #[inline]
    pub fn to_owned(&self) -> TaskUnpark {
        match *self {
            BorrowedUnpark::Old(old) => TaskUnpark::Old(old.clone()),
            BorrowedUnpark::New(new) => TaskUnpark::New(new.to_owned()),
        }
    }
}

impl<'a> BorrowedEvents<'a> {
    #[inline]
    pub fn new() -> BorrowedEvents<'a> {
        BorrowedEvents::None
    }

    #[inline]
    pub fn to_owned(&self) -> UnparkEvents {
        let mut one_event = None;
        let mut list = Vec::new();
        let mut cur = self;
        while let BorrowedEvents::One(event, next) = *cur {
            let event = event.clone();
            match one_event.take() {
                None if list.len() == 0 => one_event = Some(event),
                None => list.push(event),
                Some(event2) =>  {
                    list.push(event2);
                    list.push(event);
                }
            }
            cur = next;
        }

        match one_event {
            None if list.len() == 0 => UnparkEvents::None,
            None => UnparkEvents::Many(list.into_boxed_slice()),
            Some(e) => UnparkEvents::One(e),
        }
    }
}

impl UnparkEvents {
    pub fn notify(&self) {
        match *self {
            UnparkEvents::None => {}
            UnparkEvents::One(ref e) => e.unpark(),
            UnparkEvents::Many(ref list) => {
                for event in list.iter() {
                    event.unpark();
                }
            }
        }
    }

    pub fn will_notify(&self, events: &BorrowedEvents) -> bool {
        // Pessimistically assume that any unpark events mean that we're not
        // equivalent to the current task.
        match *self {
            UnparkEvents::None => {}
            _ => return false,
        }

        match *events {
            BorrowedEvents::None => return true,
            _ => {},
        }

        return false
    }
}

#[allow(deprecated)]
impl TaskUnpark {
    pub fn notify(&self) {
        match *self {
            TaskUnpark::Old(ref old) => old.unpark(),
            TaskUnpark::New(ref new) => new.notify(),
        }
    }

    pub fn will_notify(&self, unpark: &BorrowedUnpark) -> bool {
        match (unpark, self) {
            (&BorrowedUnpark::Old(old1), &TaskUnpark::Old(ref old2)) => {
                &**old1 as *const Unpark == &**old2 as *const Unpark
            }
            (&BorrowedUnpark::New(ref new1), &TaskUnpark::New(ref new2)) => {
                new2.will_notify(new1)
            }
            _ => false,
        }
    }
}

impl<F: Future> Spawn<F> {
    /// Polls the internal future, scheduling notifications to be sent to the
    /// `unpark` argument.
    ///
    /// This method will poll the internal future, testing if it's completed
    /// yet. The `unpark` argument is used as a sink for notifications sent to
    /// this future. That is, while the future is being polled, any call to
    /// `task::park()` will return a handle that contains the `unpark`
    /// specified.
    ///
    /// If this function returns `NotReady`, then the `unpark` should have been
    /// scheduled to receive a notification when poll can be called again.
    /// Otherwise if `Ready` or `Err` is returned, the `Spawn` task can be
    /// safely destroyed.
    #[deprecated(note = "recommended to use `poll_future_notify` instead")]
    #[allow(deprecated)]
    pub fn poll_future(&mut self, unpark: Arc<Unpark>) -> Poll<F::Item, F::Error> {
        self.enter(BorrowedUnpark::Old(&unpark), |f| f.poll())
    }

    /// Waits for the internal future to complete, blocking this thread's
    /// execution until it does.
    ///
    /// This function will call `poll_future` in a loop, waiting for the future
    /// to complete. When a future cannot make progress it will use
    /// `thread::park` to block the current thread.
    pub fn wait_future(&mut self) -> Result<F::Item, F::Error> {
        let unpark = Arc::new(ThreadNotify::new(thread::current()));

        loop {
            match self.poll_future_notify(&unpark, 0)? {
                Async::NotReady => unpark.park(),
                Async::Ready(e) => return Ok(e),
            }
        }
    }

    /// A specialized function to request running a future to completion on the
    /// specified executor.
    ///
    /// This function only works for futures whose item and error types are `()`
    /// and also implement the `Send` and `'static` bounds. This will submit
    /// units of work (instances of `Run`) to the `exec` argument provided
    /// necessary to drive the future to completion.
    ///
    /// When the future would block, it's arranged that when the future is again
    /// ready it will submit another unit of work to the `exec` provided. This
    /// will happen in a loop until the future has completed.
    ///
    /// This method is not appropriate for all futures, and other kinds of
    /// executors typically provide a similar function with perhaps relaxed
    /// bounds as well.
    ///
    /// Note that this method is likely to be deprecated in favor of the
    /// `futures::Executor` trait and `execute` method, but if this'd cause
    /// difficulty for you please let us know!
    pub fn execute(self, exec: Arc<Executor>)
        where F: Future<Item=(), Error=()> + Send + 'static,
    {
        exec.clone().execute(Run {
            // Ideally this method would be defined directly on
            // `Spawn<BoxFuture<(), ()>>` so we wouldn't have to box here and
            // it'd be more explicit, but unfortunately that currently has a
            // link error on nightly: rust-lang/rust#36155
            spawn: spawn(Box::new(self.into_inner())),
            inner: Arc::new(RunInner {
                exec: exec,
                mutex: UnparkMutex::new()
            }),
        })
    }
}

impl<S: Stream> Spawn<S> {
    /// Like `poll_future`, except polls the underlying stream.
    #[deprecated(note = "recommended to use `poll_stream_notify` instead")]
    #[allow(deprecated)]
    pub fn poll_stream(&mut self, unpark: Arc<Unpark>)
                       -> Poll<Option<S::Item>, S::Error> {
        self.enter(BorrowedUnpark::Old(&unpark), |s| s.poll())
    }

    /// Like `wait_future`, except only waits for the next element to arrive on
    /// the underlying stream.
    pub fn wait_stream(&mut self) -> Option<Result<S::Item, S::Error>> {
        let unpark = Arc::new(ThreadNotify::new(thread::current()));
        loop {
            match self.poll_stream_notify(&unpark, 0) {
                Ok(Async::NotReady) => unpark.park(),
                Ok(Async::Ready(Some(e))) => return Some(Ok(e)),
                Ok(Async::Ready(None)) => return None,
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

impl<S: Sink> Spawn<S> {
    /// Invokes the underlying `start_send` method with this task in place.
    ///
    /// If the underlying operation returns `NotReady` then the `unpark` value
    /// passed in will receive a notification when the operation is ready to be
    /// attempted again.
    #[deprecated(note = "recommended to use `start_send_notify` instead")]
    #[allow(deprecated)]
    pub fn start_send(&mut self, value: S::SinkItem, unpark: &Arc<Unpark>)
                       -> StartSend<S::SinkItem, S::SinkError> {
        self.enter(BorrowedUnpark::Old(unpark), |s| s.start_send(value))
    }

    /// Invokes the underlying `poll_complete` method with this task in place.
    ///
    /// If the underlying operation returns `NotReady` then the `unpark` value
    /// passed in will receive a notification when the operation is ready to be
    /// attempted again.
    #[deprecated(note = "recommended to use `poll_flush_notify` instead")]
    #[allow(deprecated)]
    pub fn poll_flush(&mut self, unpark: &Arc<Unpark>)
                       -> Poll<(), S::SinkError> {
        self.enter(BorrowedUnpark::Old(unpark), |s| s.poll_complete())
    }

    /// Blocks the current thread until it's able to send `value` on this sink.
    ///
    /// This function will send the `value` on the sink that this task wraps. If
    /// the sink is not ready to send the value yet then the current thread will
    /// be blocked until it's able to send the value.
    pub fn wait_send(&mut self, mut value: S::SinkItem)
                     -> Result<(), S::SinkError> {
        let notify = Arc::new(ThreadNotify::new(thread::current()));
        loop {
            value = match self.start_send_notify(value, &notify, 0)? {
                AsyncSink::NotReady(v) => v,
                AsyncSink::Ready => return Ok(()),
            };
            notify.park();
        }
    }

    /// Blocks the current thread until it's able to flush this sink.
    ///
    /// This function will call the underlying sink's `poll_complete` method
    /// until it returns that it's ready, proxying out errors upwards to the
    /// caller if one occurs.
    ///
    /// The thread will be blocked until `poll_complete` returns that it's
    /// ready.
    pub fn wait_flush(&mut self) -> Result<(), S::SinkError> {
        let notify = Arc::new(ThreadNotify::new(thread::current()));
        loop {
            if self.poll_flush_notify(&notify, 0)?.is_ready() {
                return Ok(())
            }
            notify.park();
        }
    }

    /// Blocks the current thread until it's able to close this sink.
    ///
    /// This function will close the sink that this task wraps. If the sink
    /// is not ready to be close yet, then the current thread will be blocked
    /// until it's closed.
    pub fn wait_close(&mut self) -> Result<(), S::SinkError> {
        let notify = Arc::new(ThreadNotify::new(thread::current()));
        loop {
            if self.close_notify(&notify, 0)?.is_ready() {
                return Ok(())
            }
            notify.park();
        }
    }
}

/// A trait which represents a sink of notifications that a future is ready to
/// make progress.
///
/// This trait is provided as an argument to the `Spawn::poll_future` and
/// `Spawn::poll_stream` functions. It's transitively used as part of the
/// `Task::unpark` method to internally deliver notifications of readiness of a
/// future to move forward.
#[deprecated(note = "recommended to use `Notify` instead")]
pub trait Unpark: Send + Sync {
    /// Indicates that an associated future and/or task are ready to make
    /// progress.
    ///
    /// Typically this means that the receiver of the notification should
    /// arrange for the future to get poll'd in a prompt fashion.
    fn unpark(&self);
}

/// A trait representing requests to poll futures.
///
/// This trait is an argument to the `Spawn::execute` which is used to run a
/// future to completion. An executor will receive requests to run a future and
/// an executor is responsible for ensuring that happens in a timely fashion.
///
/// Note that this trait is likely to be deprecated and/or renamed to avoid
/// clashing with the `future::Executor` trait. If you've got a use case for
/// this or would like to comment on the name please let us know!
pub trait Executor: Send + Sync + 'static {
    /// Requests that `Run` is executed soon on the given executor.
    fn execute(&self, r: Run);
}

/// Units of work submitted to an `Executor`, currently only created
/// internally.
pub struct Run {
    spawn: Spawn<Box<Future<Item = (), Error = ()> + Send>>,
    inner: Arc<RunInner>,
}

struct RunInner {
    mutex: UnparkMutex<Run>,
    exec: Arc<Executor>,
}

impl Run {
    /// Actually run the task (invoking `poll` on its future) on the current
    /// thread.
    #[allow(deprecated)]
    pub fn run(self) {
        let Run { mut spawn, inner } = self;

        // SAFETY: the ownership of this `Run` object is evidence that
        // we are in the `POLLING`/`REPOLL` state for the mutex.
        unsafe {
            inner.mutex.start_poll();

            loop {
                let unpark: Arc<Unpark> = inner.clone();
                match spawn.poll_future(unpark) {
                    Ok(Async::NotReady) => {}
                    Ok(Async::Ready(())) |
                    Err(()) => return inner.mutex.complete(),
                }
                let run = Run { spawn: spawn, inner: inner.clone() };
                match inner.mutex.wait(run) {
                    Ok(()) => return,            // we've waited
                    Err(r) => spawn = r.spawn,   // someone's notified us
                }
            }
        }
    }
}

impl fmt::Debug for Run {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Run")
         .field("contents", &"...")
         .finish()
    }
}

#[allow(deprecated)]
impl Unpark for RunInner {
    fn unpark(&self) {
        match self.mutex.notify() {
            Ok(run) => self.exec.execute(run),
            Err(()) => {}
        }
    }
}

// ===== ThreadNotify =====

struct ThreadNotify {
    thread: thread::Thread,
    ready: AtomicBool,
}

impl ThreadNotify {
    fn new(thread: thread::Thread) -> ThreadNotify {
        ThreadNotify {
            thread: thread,
            ready: AtomicBool::new(false),
        }
    }

    fn park(&self) {
        if !self.ready.swap(false, Ordering::SeqCst) {
            thread::park();
        }
    }
}

impl Notify for ThreadNotify {
    fn notify(&self, _unpark_id: usize) {
        self.ready.store(true, Ordering::SeqCst);
        self.thread.unpark()
    }
}

// ===== UnparkEvent =====

/// For the duration of the given callback, add an "unpark event" to be
/// triggered when the task handle is used to unpark the task.
///
/// Unpark events are used to pass information about what event caused a task to
/// be unparked. In some cases, tasks are waiting on a large number of possible
/// events, and need precise information about the wakeup to avoid extraneous
/// polling.
///
/// Every `Task` handle comes with a set of unpark events which will fire when
/// `unpark` is called. When fired, these events insert an identifier into a
/// concurrent set, which the task can read from to determine what events
/// occurred.
///
/// This function immediately invokes the closure, `f`, but arranges things so
/// that `task::park` will produce a `Task` handle that includes the given
/// unpark event.
///
/// # Panics
///
/// This function will panic if a task is not currently being executed. That
/// is, this method can be dangerous to call outside of an implementation of
/// `poll`.
#[deprecated(note = "recommended to use `FuturesUnordered` instead")]
#[allow(deprecated)]
pub fn with_unpark_event<F, R>(event: UnparkEvent, f: F) -> R
    where F: FnOnce() -> R
{
    super::with(|task| {
        let new_task = BorrowedTask {
            id: task.id,
            unpark: task.unpark,
            events: BorrowedEvents::One(&event, &task.events),
            map: task.map,
        };

        super::set(&new_task, f)
    })
}

/// A set insertion to trigger upon `unpark`.
///
/// Unpark events are used to communicate information about *why* an unpark
/// occurred, in particular populating sets with event identifiers so that the
/// unparked task can avoid extraneous polling. See `with_unpark_event` for
/// more.
#[derive(Clone)]
#[deprecated(note = "recommended to use `FuturesUnordered` instead")]
#[allow(deprecated)]
pub struct UnparkEvent {
    set: Arc<EventSet>,
    item: usize,
}

#[allow(deprecated)]
impl UnparkEvent {
    /// Construct an unpark event that will insert `id` into `set` when
    /// triggered.
    #[deprecated(note = "recommended to use `FuturesUnordered` instead")]
    pub fn new(set: Arc<EventSet>, id: usize) -> UnparkEvent {
        UnparkEvent {
            set: set,
            item: id,
        }
    }

    fn unpark(&self) {
        self.set.insert(self.item);
    }
}

#[allow(deprecated)]
impl fmt::Debug for UnparkEvent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("UnparkEvent")
         .field("set", &"...")
         .field("item", &self.item)
         .finish()
    }
}

/// A concurrent set which allows for the insertion of `usize` values.
///
/// `EventSet`s are used to communicate precise information about the event(s)
/// that trigged a task notification. See `task::with_unpark_event` for details.
pub trait EventSet: Send + Sync + 'static {
    /// Insert the given ID into the set
    fn insert(&self, id: usize);
}

// Safe implementation of `UnsafeNotify` for `Arc` in the standard library.
//
// Note that this is a very unsafe implementation! The crucial pieces is that
// these two values are considered equivalent:
//
// * Arc<T>
// * *const ArcWrapped<T>
//
// We don't actually know the layout of `ArcWrapped<T>` as it's an
// implementation detail in the standard library. We can work, though, by
// casting it through and back an `Arc<T>`.
//
// This also means that you won't actually fine `UnsafeNotify for Arc<T>`
// because it's the wrong level of indirection. These methods are sort of
// receiving Arc<T>, but not an owned version. It's... complicated. We may be
// one of the first users of unsafe trait objects!

struct ArcWrapped<T>(PhantomData<T>);

impl<T: Notify + 'static> Notify for ArcWrapped<T> {
    fn notify(&self, id: usize) {
        unsafe {
            let me: *const ArcWrapped<T> = self;
            T::notify(&*(&me as *const *const ArcWrapped<T> as *const Arc<T>),
                      id)
        }
    }

    fn clone_id(&self, id: usize) -> usize {
        unsafe {
            let me: *const ArcWrapped<T> = self;
            T::clone_id(&*(&me as *const *const ArcWrapped<T> as *const Arc<T>),
                        id)
        }
    }

    fn drop_id(&self, id: usize) {
        unsafe {
            let me: *const ArcWrapped<T> = self;
            T::drop_id(&*(&me as *const *const ArcWrapped<T> as *const Arc<T>),
                       id)
        }
    }
}

unsafe impl<T: Notify + 'static> UnsafeNotify for ArcWrapped<T> {
    unsafe fn clone_raw(&self) -> NotifyHandle {
        let me: *const ArcWrapped<T> = self;
        let arc = (*(&me as *const *const ArcWrapped<T> as *const Arc<T>)).clone();
        NotifyHandle::from(arc)
    }

    unsafe fn drop_raw(&self) {
        let mut me: *const ArcWrapped<T> = self;
        let me = &mut me as *mut *const ArcWrapped<T> as *mut Arc<T>;
        ptr::drop_in_place(me);
    }
}

impl<T> From<Arc<T>> for NotifyHandle
    where T: Notify + 'static,
{
    fn from(rc: Arc<T>) -> NotifyHandle {
        unsafe {
            let ptr = mem::transmute::<Arc<T>, *mut ArcWrapped<T>>(rc);
            NotifyHandle::new(ptr)
        }
    }
}
