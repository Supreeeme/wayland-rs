use std::any::Any;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::task;

use futures_channel::mpsc::{unbounded, UnboundedReceiver, UnboundedSender};
use futures_core::stream::Stream;
use nix::Error;
use wayland_backend::{
    client::{Backend, ObjectData, ObjectId, ReadEventsGuard, WaylandError},
    protocol::{Argument, Message},
};

use crate::{conn::SyncData, Connection, DispatchError, Proxy};

/// A trait providing an implementation for handling events a proxy through an [`EventQueue`].
///
/// ## General usage
///
/// You need to implement this trait on your `State` for every type of Wayland object that will be processed
/// by the [`EventQueue`] working with your `State`.
///
/// You can have different implementations of the trait for the same interface but different `UserData` type,
/// this way the events for a given object will be processed by the adequate implementation depending on
/// which `UserData` was assigned to it at creation.
///
/// The way this trait works is that the [`Dispatch::event()`] method will be invoked by the event queue for
/// every event received by an object associated to this event queue. Your implementation can then match on
/// the associated [`Proxy::Event`] enum and do any processing needed with that event.
///
/// In the rare case of an interface with *events* creating new objects (in the core protocol, the only
/// instance of this is the `wl_data_device.data_offer` event), you'll need to implement the
/// [`Dispatch::event_created_child()`] method. See the [`event_created_child!`](event_created_child!) macro
/// for a simple way to do this.
///
/// ## Modularity
///
/// To provide generic handlers for downstream usage, it is possible to make an implementation of the trait
/// that is generic over the last type argument, as illustrated below. Users will then be able to
/// automatically delegate their implementation to yours using the [`delegate_dispatch!`] macro.
///
/// As a result, when your implementation is instanciated, the last type parameter `State` will be the state
/// struct of the app using your generic implementation. You can put additional trait constraints on it to
/// specify an interface between your module and downstream code, as illustrated in this example:
///
/// ```
/// # // Maintainers: If this example changes, please make sure you also carry those changes over to the delegate_dispatch macro.
/// use wayland_client::{protocol::wl_registry, Dispatch};
///
/// /// The type we want to delegate to
/// struct DelegateToMe;
///
/// /// The user data relevant for your implementation.
/// /// When providing delegate implementation, it is recommended to use your own type here, even if it is
/// /// just a unit struct: using () would cause a risk of clashing with an other such implementation.
/// struct MyUserData;
///
/// // Now a generic implementation of Dispatch, we are generic over the last type argument instead of using
/// // the default State=Self.
/// impl<State> Dispatch<wl_registry::WlRegistry, MyUserData, State> for DelegateToMe
/// where
///     // State is the type which has delegated to this type, so it needs to have an impl of Dispatch itself
///     State: Dispatch<wl_registry::WlRegistry, MyUserData>,
///     // If your delegate type has some internal state, it'll need to access it, and you can
///     // require it by adding custom trait bounds.
///     // In this example, we just require an AsMut implementation
///     State: AsMut<DelegateToMe>,
/// {
///     fn event(
///         state: &mut State,
///         _proxy: &wl_registry::WlRegistry,
///         _event: wl_registry::Event,
///         _udata: &MyUserData,
///         _conn: &wayland_client::Connection,
///         _qhandle: &wayland_client::QueueHandle<State>,
///     ) {
///         // Here the delegate may handle incoming events as it pleases.
///
///         // For example, it retrives its state and does some processing with it
///         let me: &mut DelegateToMe = state.as_mut();
///         // do something with `me` ...
/// #       std::mem::drop(me) // use `me` to avoid a warning
///     }
/// }
/// ```
///
/// **Note:** Due to limitations in Rust's trait resolution algorithm, a type providing a generic
/// implementation of [`Dispatch`] cannot be used directly as the dispatching state, as rustc
/// currently fails to understand that it also provides `Dispatch<I, U, Self>` (assuming all other
/// trait bounds are respected as well).
pub trait Dispatch<I, UserData, State = Self>
where
    Self: Sized,
    I: Proxy,
    State: Dispatch<I, UserData, State>,
{
    /// Called when an event from the server is processed
    ///
    /// This method contains your logic for processing events, which can vary wildly from an object to the
    /// other. You are given as argument:
    ///
    /// - a proxy representing the object that received this event
    /// - the event itself as the [`Proxy::Event`] enum (which you'll need to match against)
    /// - a reference to the `UserData` that was associated with that object on creation
    /// - a reference to the [`Connection`] in case you need to access it
    /// - a reference to a [`QueueHandle`] associated with the [`EventQueue`] currently processing events, in
    ///   case you need to create new objects that you want associated to the same [`EventQueue`].
    fn event(
        state: &mut State,
        proxy: &I,
        event: I::Event,
        data: &UserData,
        conn: &Connection,
        qhandle: &QueueHandle<State>,
    );

    /// Method used to initialize the user-data of objects created by events
    ///
    /// If the interface does not have any such event, you can ignore it. If not, the
    /// [`event_created_child!`](event_created_child!) macro is provided for overriding it.
    #[cfg_attr(coverage, no_coverage)]
    fn event_created_child(opcode: u16, _qhandle: &QueueHandle<State>) -> Arc<dyn ObjectData> {
        panic!(
            "Missing event_created_child specialization for event opcode {} of {}",
            opcode,
            I::interface().name
        );
    }
}

/// Macro used to override [`Dispatch::event_created_child()`]
///
/// Use this macro inside the [`Dispatch`] implementation to override this method, to implement the
/// initialization of the user data for event-created objects. The usage syntax is as follow:
///
/// ```ignore
/// impl Dispatch<WlFoo, FooUserData> for MyState {
///     fn event(
///         &mut self,
///         proxy: &WlFoo,
///         event: FooEvent,
///         data: &FooUserData,
///         connhandle: &mut ConnectionHandle,
///         qhandle: &QueueHandle<MyState>
///     ) {
///         /* ... */
///     }
///
///     event_created_child!(MyState, WlFoo, [
///     // there can be multiple lines if this interface has multiple object-creating event
///         EVT_CREATE_BAR => (WlBar, BarUserData::new()),
///     //  ~~~~~~~~~~~~~~     ~~~~~  ~~~~~~~~~~~~~~~~~~
///     //    |                  |      |
///     //    |                  |      +-- an expression whose evaluation produces the
///     //    |                  |          user data value
///     //    |                  +-- the type of the newly created object
///     //    +-- the opcode of the event that creates a new object, constants for those are
///     //        generated alongside the `WlFoo` type in the `wl_foo` module
///     ]);
/// }
/// ```
#[macro_export]
macro_rules! event_created_child {
    // Must match `pat` to allow paths `wl_data_device::EVT_DONE_OPCODE` and expressions `0` to both work.
    ($selftype:ty, $iface:ty, [$($opcode:pat => ($child_iface:ty, $child_udata:expr)),* $(,)?]) => {
        fn event_created_child(
            opcode: u16,
            qhandle: &$crate::QueueHandle<$selftype>
        ) -> std::sync::Arc<dyn $crate::backend::ObjectData> {
            match opcode {
                $(
                    $opcode => {
                        qhandle.make_data::<$child_iface, _>({$child_udata})
                    },
                )*
                _ => {
                    panic!("Missing event_created_child specialization for event opcode {} of {}", opcode, <$iface as $crate::Proxy>::interface().name);
                },
            }
        }
    };
}

type QueueCallback<State> = fn(
    &Connection,
    Message<ObjectId>,
    &mut State,
    Arc<dyn ObjectData>,
    &QueueHandle<State>,
) -> Result<(), DispatchError>;

struct QueueEvent<State>(QueueCallback<State>, Message<ObjectId>, Arc<dyn ObjectData>);

impl<State> std::fmt::Debug for QueueEvent<State> {
    #[cfg_attr(coverage, no_coverage)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueEvent").field("msg", &self.1).finish_non_exhaustive()
    }
}

/// An event queue
///
/// This is an abstraction for handling event dispatching, that allows you to ensure
/// access to some common state `&mut State` to your event handlers.
///
/// Event queues are created through [`Connection::new_event_queue()`](crate::Connection::new_event_queue).
///
/// Upon creation, a wayland object is assigned to an event queue by passing the associated [`QueueHandle`]
/// as argument to the method creating it. All events received by that object will be processed by that event
/// queue, when [`dispatch_pending()`](EventQueue::dispatch_pending) or
/// [`blocking_dispatch()`](EventQueue::blocking_dispatch) is invoked.
///
/// ## Usage
///
/// ### Single queue app
///
/// If your app is simple enough that the only source of event to process is the Wayland socket and you only
/// need a single event queue, your main loop can be as simple as this:
///
/// ```rust,no_run
/// use wayland_client::Connection;
///
/// let connection = Connection::connect_to_env().unwrap();
/// let mut event_queue = connection.new_event_queue();
///
/// /*
///  * Here your initial setup
///  */
/// # struct State {
/// #     exit: bool
/// # }
/// # let mut state = State { exit: false };
///
/// // And the main loop:
/// while !state.exit {
///     event_queue.blocking_dispatch(&mut state).unwrap();
/// }
/// ```
///
/// The [`blocking_dispatch()`](EventQueue::blocking_dispatch) will wait (by putting the thread to sleep)
/// until there are some events from the server that can be processed, and all your actual app logic can be
/// done in the callbacks of the [`Dispatch`] implementations, and in the main `loop` after the
/// `blocking_dispatch()` call.
///
/// ### Multi-thread multi-queue app
///
/// In a case where you app is multithreaded and you want to process events in multiple thread, a simple
/// pattern is to have one [`EventQueue`] per thread processing Wayland events.
///
/// With this pattern, each thread can use [`EventQueue::blocking_dispatch()`](EventQueue::blocking_dispatch
/// on its own event loop, and everything will "Just Work".
///
/// ### Single-queue guest library
///
/// If your code is some library code that will act on a Wayland connection shared by the main program, it is
/// likely you should not trigger socket reads yourself and instead let the main app take care of it. In this
/// case, to ensure your [`EventQueue`] still makes progress, you should regularly invoke
/// [`EventQueue::dispatch_pending()`](EventQueue::dispatch_pending) which will process the events that were
/// enqueued in the inner buffer of your [`EventQueue`] by the main app reading the socket.
///
/// ### Integrating the event queue with other sources of events
///
/// If your program needs to monitor other sources of events alongside the Wayland socket using a monitoring
/// system like `epoll`, you can integrate the Wayland socket into this system. This is done with the help
/// of the [`EventQueue::prepare_read()`](EventQueue::prepare_read) method. You event loop will be a bit more
/// explicit:
///
/// ```rust,no_run
/// # use wayland_client::Connection;
/// # let connection = Connection::connect_to_env().unwrap();
/// # let mut event_queue = connection.new_event_queue();
/// # let mut state = ();
///
/// loop {
///     // flush the outgoing buffers to ensure that the server does receive the messages
///     // you've sent
///     event_queue.flush().unwrap();
///
///     // (this step is only relevant if other threads might be reading the socket as well)
///     // make sure you don't have any pending events if the event queue that might have been
///     // enqueued by other threads reading the socket
///     event_queue.dispatch_pending(&mut state).unwrap();
///
///     // This puts in place some internal synchronization to prepare for the fact that
///     // you're going to wait for events on the socket and read them, in case other threads
///     // are doing the same thing
///     let read_guard = event_queue.prepare_read().unwrap();
///
///     /*
///      * At this point you can invoke epoll(..) to wait for readiness on the multiple FD you
///      * are working with, and read_guard.connection_fd() will give you the FD to wait on for
///      * the Wayland connection
///      */
/// # let wayland_socket_ready = true;
///
///     if wayland_socket_ready {
///         // If epoll notified readiness of the Wayland socket, you can now proceed to the read
///         read_guard.read().unwrap();
///         // And now, you must invoke dispatch_pending() to actually process the events
///         event_queue.dispatch_pending(&mut state).unwrap();
///     } else {
///         // otherwise, some of your other FD are ready, but you didn't receive Wayland events,
///         // you can drop the guard to cancel the read preparation
///         std::mem::drop(read_guard);
///     }
///
///     /*
///      * There you process all relevant events from your other event sources
///      */
/// }
/// ```
pub struct EventQueue<State> {
    rx: UnboundedReceiver<QueueEvent<State>>,
    handle: QueueHandle<State>,
    conn: Connection,
}

impl<State> std::fmt::Debug for EventQueue<State> {
    #[cfg_attr(coverage, no_coverage)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventQueue")
            .field("rx", &self.rx)
            .field("handle", &self.handle)
            .finish_non_exhaustive()
    }
}

impl<State> EventQueue<State> {
    pub(crate) fn new(conn: Connection) -> Self {
        let (tx, rx) = unbounded();
        Self { rx, handle: QueueHandle { tx }, conn }
    }

    /// Get a [`QueueHandle`] for this event queue
    pub fn handle(&self) -> QueueHandle<State> {
        self.handle.clone()
    }

    /// Dispatch pending events
    ///
    /// Events are accumulated in the event queue internal buffer when the Wayland socket is read using
    /// the read APIs on [`Connection`](crate::Connection), or when reading is done from an other thread.
    /// This method will dispatch all such pending events by sequentially invoking their associated handlers:
    /// the [`Dispatch`](crate::Dispatch) implementations on the provided `&mut D`.
    pub fn dispatch_pending(&mut self, data: &mut State) -> Result<usize, DispatchError> {
        Self::dispatching_impl(&self.conn, &mut self.rx, &self.handle, data)
    }

    /// Block waiting for events and dispatch them
    ///
    /// This method is similar to [`dispatch_pending`](EventQueue::dispatch_pending), but if there are no
    /// pending events it will also flush the connection and block waiting for the Wayland server to send an
    /// event.
    ///
    /// A simple app event loop can consist of invoking this method in a loop.
    pub fn blocking_dispatch(&mut self, data: &mut State) -> Result<usize, DispatchError> {
        let dispatched = Self::dispatching_impl(&self.conn, &mut self.rx, &self.handle, data)?;
        if dispatched > 0 {
            Ok(dispatched)
        } else {
            crate::conn::blocking_dispatch_impl(self.conn.backend())?;
            Self::dispatching_impl(&self.conn, &mut self.rx, &self.handle, data)
        }
    }

    /// Synchronous roundtrip
    ///
    /// This function will cause a synchronous round trip with the wayland server. This function will block
    /// until all requests in the queue are sent and processed by the server.
    ///
    /// This function may be useful during initial setup of your app. This function may also be useful
    /// where you need to guarantee all requests prior to calling this function are completed.
    pub fn roundtrip(&mut self, data: &mut State) -> Result<usize, DispatchError> {
        let done = Arc::new(AtomicBool::new(false));

        {
            let display = self.conn.display();
            let cb_done = done.clone();
            let sync_data = Arc::new(SyncData { done: cb_done });
            self.conn
                .send_request(
                    &display,
                    crate::protocol::wl_display::Request::Sync {},
                    Some(sync_data),
                )
                .map_err(|_| WaylandError::Io(Error::EPIPE.into()))?;
        }

        let mut dispatched = 0;

        while !done.load(Ordering::Acquire) {
            dispatched += self.blocking_dispatch(data)?;
        }

        Ok(dispatched)
    }

    /// Start a synchronized read from the socket
    ///
    /// This is needed if you plan to wait on readiness of the Wayland socket using an event
    /// loop. See the [`EventQueue`] and [`ReadEventsGuard`] docs for details. Once the events are received,
    /// you'll then need to dispatch them from the event queue using
    /// [`EventQueue::dispatch_pending()`](EventQueue::dispatch_pending).
    ///
    /// If you don't need to manage multiple event sources, see
    /// [`blocking_dispatch()`](EventQueue::blocking_dispatch) for a simpler mechanism.
    ///
    /// This method is identical to [`Connection::prepare_read()`].
    pub fn prepare_read(&self) -> Result<ReadEventsGuard, WaylandError> {
        self.conn.prepare_read()
    }

    /// Flush pending outgoing events to the server
    ///
    /// This needs to be done regularly to ensure the server receives all your requests.
    /// /// This method is identical to [`Connection::flush()`].
    pub fn flush(&self) -> Result<(), WaylandError> {
        self.conn.flush()
    }

    fn dispatching_impl(
        backend: &Connection,
        rx: &mut UnboundedReceiver<QueueEvent<State>>,
        qhandle: &QueueHandle<State>,
        data: &mut State,
    ) -> Result<usize, DispatchError> {
        // This call will most of the time do nothing, but ensure that if the Connection is in guest mode
        // from some external connection, only invoking `EventQueue::dispatch_pending()` will be enough to
        // process the events assuming the host program already takes care of reading the socket.
        //
        // We purposefully ignore the possible error, as that would make us early return in a way that might
        // lose events, and the potential socket error will be caught in other places anyway.
        let _ = backend.backend.dispatch_inner_queue();

        let mut dispatched = 0;
        while let Ok(Some(QueueEvent(cb, msg, odata))) = rx.try_next() {
            cb(backend, msg, data, odata, qhandle)?;
            dispatched += 1;
        }
        Ok(dispatched)
    }

    /// Attempt to dispatch events from this queue, registering the current task for wakeup if no
    /// events are pending.
    ///
    /// This method is similar to [`dispatch_pending`](EventQueue::dispatch_pending); it will not
    /// perform reads on the Wayland socket.  Reads on the socket by other tasks or threads will
    /// cause the current task to wake up if events are pending on this queue.
    ///
    /// ```
    /// use futures_channel::mpsc::Receiver;
    /// use futures_util::future::{poll_fn,select};
    /// use futures_util::stream::StreamExt;
    /// use wayland_client::EventQueue;
    ///
    /// struct Data;
    ///
    /// enum AppEvent {
    ///     SomethingHappened(u32),
    /// }
    ///
    /// impl Data {
    ///     fn handle(&mut self, event: AppEvent) {
    ///         // actual event handling goes here
    ///     }
    /// }
    ///
    /// // An async task that is spawned on an executor in order to handle events that need access
    /// // to a specific data object.
    /// async fn run(data: &mut Data, mut wl_queue: EventQueue<Data>, mut app_queue: Receiver<AppEvent>)
    ///     -> Result<(), Box<dyn std::error::Error>>
    /// {
    ///     use futures_util::future::Either;
    ///     loop {
    ///         match select(
    ///             poll_fn(|cx| wl_queue.poll_dispatch_pending(cx, data)),
    ///             app_queue.next(),
    ///         ).await {
    ///             Either::Left((res, _)) => match res? {},
    ///             Either::Right((Some(event), _)) => {
    ///                 data.handle(event);
    ///             }
    ///             Either::Right((None, _)) => return Ok(()),
    ///         }
    ///     }
    /// }
    /// ```
    pub fn poll_dispatch_pending(
        &mut self,
        cx: &mut task::Context,
        data: &mut State,
    ) -> task::Poll<Result<Infallible, DispatchError>> {
        loop {
            if let Err(e) = self.conn.backend.dispatch_inner_queue() {
                return task::Poll::Ready(Err(e.into()));
            }
            match Pin::new(&mut self.rx).poll_next(cx) {
                task::Poll::Pending => return task::Poll::Pending,
                task::Poll::Ready(None) => {
                    // We never close the channel, and we hold a valid sender in self.handle.tx, so
                    // our event stream will never reach an end.
                    unreachable!("Got end of stream while holding a valid sender");
                }
                task::Poll::Ready(Some(QueueEvent(cb, msg, odata))) => {
                    cb(&self.conn, msg, data, odata, &self.handle)?
                }
            }
        }
    }
}

/// A handle representing an [`EventQueue`], used to assign objects upon creation.
pub struct QueueHandle<State> {
    tx: UnboundedSender<QueueEvent<State>>,
}

impl<State> std::fmt::Debug for QueueHandle<State> {
    #[cfg_attr(coverage, no_coverage)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueHandle").field("tx", &self.tx).finish()
    }
}

impl<State> Clone for QueueHandle<State> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

pub(crate) struct QueueSender<State> {
    func: QueueCallback<State>,
    pub(crate) handle: QueueHandle<State>,
}

pub(crate) trait ErasedQueueSender<I> {
    fn send(&self, msg: Message<ObjectId>, odata: Arc<dyn ObjectData>);
}

impl<I: Proxy, State> ErasedQueueSender<I> for QueueSender<State> {
    fn send(&self, msg: Message<ObjectId>, odata: Arc<dyn ObjectData>) {
        if self.handle.tx.unbounded_send(QueueEvent(self.func, msg, odata)).is_err() {
            crate::log_error!("Event received for EventQueue after it was dropped.");
        }
    }
}

impl<State: 'static> QueueHandle<State> {
    /// Create an object data associated with this event queue
    ///
    /// This creates an implementation of [`ObjectData`] fitting for direct use with `wayland-backend` APIs
    /// that forwards all events to the event queue associated with this token, integrating the object into
    /// the [`Dispatch`]-based logic of `wayland-client`.
    pub fn make_data<I: Proxy + 'static, U: Send + Sync + 'static>(
        &self,
        user_data: U,
    ) -> Arc<dyn ObjectData>
    where
        State: Dispatch<I, U, State>,
    {
        let sender: Box<dyn ErasedQueueSender<I> + Send + Sync> =
            Box::new(QueueSender { func: queue_callback::<I, U, State>, handle: self.clone() });

        let has_creating_event =
            I::interface().events.iter().any(|desc| desc.child_interface.is_some());

        let odata_maker = if has_creating_event {
            let qhandle = self.clone();
            Box::new(move |msg: &Message<ObjectId>| {
                for arg in &msg.args {
                    match arg {
                        Argument::NewId(id) if id.is_null() => {
                            return None;
                        }
                        Argument::NewId(_) => {
                            return Some(<State as Dispatch<I, U, State>>::event_created_child(
                                msg.opcode, &qhandle,
                            ));
                        }
                        _ => continue,
                    }
                }
                None
            }) as Box<_>
        } else {
            Box::new(|_: &Message<ObjectId>| None) as Box<_>
        };
        Arc::new(QueueProxyData { sender, odata_maker, udata: user_data })
    }
}

fn queue_callback<
    I: Proxy + 'static,
    U: Send + Sync + 'static,
    State: Dispatch<I, U, State> + 'static,
>(
    handle: &Connection,
    msg: Message<ObjectId>,
    data: &mut State,
    odata: Arc<dyn ObjectData>,
    qhandle: &QueueHandle<State>,
) -> Result<(), DispatchError> {
    let (proxy, event) = I::parse_event(handle, msg)?;
    let udata = odata.data_as_any().downcast_ref().expect("Wrong user_data value for object");
    <State as Dispatch<I, U, State>>::event(data, &proxy, event, udata, handle, qhandle);
    Ok(())
}

type ObjectDataFactory = dyn Fn(&Message<ObjectId>) -> Option<Arc<dyn ObjectData>> + Send + Sync;

/// The [`ObjectData`] implementation used by Wayland proxies, integrating with [`Dispatch`]
pub struct QueueProxyData<I: Proxy, U> {
    pub(crate) sender: Box<dyn ErasedQueueSender<I> + Send + Sync>,
    odata_maker: Box<ObjectDataFactory>,
    /// The user data associated with this object
    pub udata: U,
}

impl<I: Proxy + 'static, U: Send + Sync + 'static> ObjectData for QueueProxyData<I, U> {
    fn event(self: Arc<Self>, _: &Backend, msg: Message<ObjectId>) -> Option<Arc<dyn ObjectData>> {
        let ret = (self.odata_maker)(&msg);
        self.sender.send(msg, self.clone());
        ret
    }

    fn destroyed(&self, _: ObjectId) {}

    fn data_as_any(&self) -> &dyn Any {
        &self.udata
    }
}

impl<I: Proxy, U: std::fmt::Debug> std::fmt::Debug for QueueProxyData<I, U> {
    #[cfg_attr(coverage, no_coverage)]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueueProxyData").field("udata", &self.udata).finish()
    }
}

struct TemporaryData;

impl ObjectData for TemporaryData {
    fn event(self: Arc<Self>, _: &Backend, _: Message<ObjectId>) -> Option<Arc<dyn ObjectData>> {
        unreachable!()
    }

    fn destroyed(&self, _: ObjectId) {}
}

/*
 * Dispatch delegation helpers
 */

/// A helper macro which delegates a set of [`Dispatch`] implementations for proxies to some other type which
/// provides a generic [`Dispatch`] implementation.
///
/// This macro allows more easily delegating smaller parts of the protocol an application may wish to handle
/// in a modular fashion.
///
/// # Usage
///
/// For example, say you want to delegate events for [`WlRegistry`](crate::protocol::wl_registry::WlRegistry)
/// to the struct `DelegateToMe` for the [`Dispatch`] documentatione example.
///
/// ```
/// use wayland_client::{delegate_dispatch, protocol::wl_registry};
/// #
/// # use wayland_client::Dispatch;
/// #
/// # struct DelegateToMe;
/// # struct MyUserData;
/// #
/// # impl<State> Dispatch<wl_registry::WlRegistry, MyUserData, State> for DelegateToMe
/// # where
/// #     State: Dispatch<wl_registry::WlRegistry, MyUserData> + AsMut<DelegateToMe>,
/// # {
/// #     fn event(
/// #         _state: &mut State,
/// #         _proxy: &wl_registry::WlRegistry,
/// #         _event: wl_registry::Event,
/// #         _udata: &MyUserData,
/// #         _conn: &wayland_client::Connection,
/// #         _qhandle: &wayland_client::QueueHandle<State>,
/// #     ) {
/// #     }
/// # }
///
/// // ExampleApp is the type events will be dispatched to.
///
/// /// The application state
/// struct ExampleApp {
///     /// The delegate for handling wl_registry events.
///     delegate: DelegateToMe,
/// }
///
/// // Use delegate_dispatch to implement Dispatch<wl_registry::WlRegistry, MyUserData> for ExampleApp
/// delegate_dispatch!(ExampleApp: [wl_registry::WlRegistry: MyUserData] => DelegateToMe);
///
/// // DelegateToMe requires that ExampleApp implements AsMut<DelegateToMe>, so we provide the
/// // trait implementation.
/// impl AsMut<DelegateToMe> for ExampleApp {
///     fn as_mut(&mut self) -> &mut DelegateToMe {
///         &mut self.delegate
///     }
/// }
///
/// // To explain the macro above, you may read it as the following:
/// //
/// // For ExampleApp, delegate WlRegistry to DelegateToMe.
///
/// // Assert ExampleApp can Dispatch events for wl_registry
/// fn assert_is_registry_delegate<T>()
/// where
///     T: Dispatch<wl_registry::WlRegistry, MyUserData>,
/// {
/// }
///
/// assert_is_registry_delegate::<ExampleApp>();
/// ```
///
/// You may also delegate multiple proxies to a single type. This is especially useful for handling multiple
/// related protocols in the same modular component.
///
/// For example, a type which can dispatch both the `wl_output` and `xdg_output` protocols may be used as a
/// delegate:
///
/// ```ignore
/// # // This is not tested because xdg_output is in wayland-protocols.
/// delegate_dispatch!(ExampleApp: [
///     wl_output::WlOutput: OutputData,
///     xdg_output::XdgOutput: XdgOutputData,
/// ] => OutputDelegate);
/// ```
#[macro_export]
macro_rules! delegate_dispatch {
    ($dispatch_from: ty: [ $($interface: ty : $user_data: ty),* $(,)?] => $dispatch_to: ty) => {
        $(
            impl $crate::Dispatch<$interface, $user_data> for $dispatch_from {
                fn event(
                    state: &mut Self,
                    proxy: &$interface,
                    event: <$interface as $crate::Proxy>::Event,
                    data: &$user_data,
                    conn: &$crate::Connection,
                    qhandle: &$crate::QueueHandle<Self>,
                ) {
                    <$dispatch_to as $crate::Dispatch<$interface, $user_data, Self>>::event(state, proxy, event, data, conn, qhandle)
                }

                fn event_created_child(
                    opcode: u16,
                    qhandle: &$crate::QueueHandle<Self>
                ) -> ::std::sync::Arc<dyn $crate::backend::ObjectData> {
                    <$dispatch_to as $crate::Dispatch<$interface, $user_data, Self>>::event_created_child(opcode, qhandle)
                }
            }
        )*
    };
}
