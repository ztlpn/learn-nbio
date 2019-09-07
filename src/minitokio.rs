use std::{
    sync::{Arc, Weak, Mutex, Condvar},
    cell::RefCell,
    pin::Pin,
    future::Future,
    task,
    io::{ self, prelude::* },
};

use slab::Slab;

use crossbeam::{
    channel,
};

struct ScopeGuard<T: FnOnce() -> ()> {
    on_exit: Option<T>
}

impl<T: FnOnce() -> ()> Drop for ScopeGuard<T> {
    fn drop(&mut self) {
        let on_exit = self.on_exit.take().unwrap();
        on_exit();
    }
}

// a task is a toplevel future that is driven by the executor.
type TaskBox = Pin<Box<dyn Future<Output = ()> + Send>>;

// current state of the task in the executor
enum TaskState {
    Waiting(TaskBox),
    Executing,
     // the task is currently executing and has signalled that it is ready to be executed again immediately.
    ScheduledAgain,
}

struct ExecutorInner {
    tasks: Mutex<Slab<TaskState>>,
    done_cond: Condvar,
    ready_sender: channel::Sender<(TaskBox, usize)>,
}

impl ExecutorInner {
    fn spawn<T>(&self, task: T) -> usize where T: Future<Output = ()> + Send + 'static {
        let pinned = Box::pin(task);
        let key = self.tasks.lock().unwrap().insert(TaskState::Executing);
        self.ready_sender.send((pinned, key)).unwrap();
        key
    }

    fn wakeup(&self, task_id: usize) {
        let mut tasks = self.tasks.lock().unwrap();
        let task_state = tasks.get_mut(task_id).expect("tried to wakeup unknown task!");
        if let TaskState::Waiting(task) = std::mem::replace(task_state, TaskState::ScheduledAgain) {
            *task_state = TaskState::Executing;
            drop(tasks);
            self.ready_sender.send((task, task_id)).unwrap();
        }
    }

    fn block_until_done(&self) {
        let mut tasks = self.tasks.lock().unwrap();
        while !tasks.is_empty() {
            tasks = self.done_cond.wait(tasks).unwrap();
        }
    }
}

thread_local! {
    static CURRENT_EXECUTOR: RefCell<Option<Weak<ExecutorInner>>> = RefCell::new(None);
}

struct Executor {
    inner: Option<Arc<ExecutorInner>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl Executor {
    fn new(nthreads: usize, reactor: Option<Arc<ReactorInner>>) -> Executor {
        let (ready_sender, ready_receiver) = channel::unbounded::<(TaskBox, usize)>();
        let inner = Arc::new(
            ExecutorInner {
                tasks: Mutex::new(Slab::new()),
                done_cond: Condvar::new(),
                ready_sender,
            });

        let mut workers = Vec::new();
        for _ in 0..nthreads {
            let inner_strong = Arc::clone(&inner);
            let ready_receiver = ready_receiver.clone();
            let reactor = reactor.clone();

            let worker = move || {
                let _executor_guard = Executor::set_current(&inner_strong);
                // downgrading to Weak so that Executor::drop can drop inner.ready_sender
                // and thus signal the thread to stop.
                let inner = Arc::downgrade(&inner_strong);
                drop(inner_strong);

                let _reactor_guard;
                if let Some(reactor) = reactor {
                    _reactor_guard = Reactor::set_current(&reactor);
                }

                while let Ok((mut ready_task, task_id)) = ready_receiver.recv() {
                    let waker = Executor::get_waker(inner.clone(), task_id);
                    let mut ctx = task::Context::from_waker(&waker);
                    match ready_task.as_mut().poll(&mut ctx) {
                        task::Poll::Ready(()) => {
                            eprintln!("task {} done", task_id);
                            if let Some(inner) = Weak::upgrade(&inner) {
                                let mut tasks = inner.tasks.lock().unwrap();
                                tasks.remove(task_id);
                                if tasks.is_empty() {
                                    inner.done_cond.notify_all();
                                }
                            } else {
                                return;
                            }
                        }

                        task::Poll::Pending => {
                            if let Some(inner) = Weak::upgrade(&inner) {
                                let mut tasks = inner.tasks.lock().unwrap();
                                let task_state = tasks.get_mut(task_id)
                                    .expect("got id for nonexistent task from ready queue");
                                match task_state {
                                    TaskState::Executing => {
                                        *task_state = TaskState::Waiting(ready_task);
                                    }

                                    TaskState::ScheduledAgain => {
                                        *task_state = TaskState::Executing;
                                        drop(tasks);
                                        inner.ready_sender.send((ready_task, task_id)).unwrap();
                                    }

                                    TaskState::Waiting(_) => panic!("two different tasks with the same task_id!"),
                                }
                            } else {
                                return;
                            }
                        }
                    }
                }
            };

            workers.push(std::thread::spawn(worker));
        };

        Executor {
            inner: Some(inner),
            workers,
        }
    }

    fn set_current(inner: &Arc<ExecutorInner>) -> ScopeGuard<impl FnOnce() -> ()> {
        CURRENT_EXECUTOR.with(|e| {
            *e.borrow_mut() = Some(Arc::downgrade(inner));
        });

        ScopeGuard {
            on_exit: Some(|| {
                CURRENT_EXECUTOR.with(|e| {
                    e.replace(None);
                });
            }),
        }
    }

    fn current() -> Option<Arc<ExecutorInner>> {
        CURRENT_EXECUTOR.with(|e| {
            Weak::upgrade(e.borrow().as_ref().expect("no current executor!"))
        })
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Note: it is a very unreliable way to stop workers. Some strong ref to inner can end up stored somewhere
        // resulting in a deadlock.
        self.inner = None;
        for worker in self.workers.drain(..) {
            worker.join().unwrap();
        }
        eprintln!("executor stopped!");
    }
}

struct Waker {
    executor: Weak<ExecutorInner>,
    task_id: usize,
}

impl Waker {
    fn wake(&self) {
        if let Some(executor) = Weak::upgrade(&self.executor) {
            executor.wakeup(self.task_id);
        }
    }
}

static WAKER_VTABLE: task::RawWakerVTable = {
    unsafe fn clone_fn(waker: *const ()) -> task::RawWaker {
        let this_waker: Arc<Waker> = Arc::from_raw(std::mem::transmute(waker));
        let new_waker = this_waker.clone();
        std::mem::forget(this_waker); // will be dropped in drop_fn
        task::RawWaker::new(std::mem::transmute(Arc::into_raw(new_waker)), &WAKER_VTABLE)
    }

    unsafe fn wake_fn(waker: *const ()) {
        // consumes the Arc
        let waker: Arc<Waker> = Arc::from_raw(std::mem::transmute(waker));
        waker.wake();
    }

    unsafe fn wake_by_ref_fn(waker: *const ()) {
        let waker: *const Waker = std::mem::transmute(waker);
        waker.as_ref().unwrap().wake();
    }

    unsafe fn drop_fn(waker: *const ()) {
        let waker: Arc<Waker> = Arc::from_raw(std::mem::transmute(waker));
        drop(waker);
    }

    task::RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn)
};

impl Executor {
    fn get_waker(executor: Weak<ExecutorInner>, task_id: usize) -> task::Waker {
        let waker = Arc::new(Waker {
            executor,
            task_id,
        });

        unsafe {
            task::Waker::from_raw(task::RawWaker::new(std::mem::transmute(Arc::into_raw(waker)), &WAKER_VTABLE))
        }
    }
}

pub fn spawn<T>(task: T)
where T: Future<Output = ()> + Send + 'static {
    if let Some(executor) = Executor::current() {
        executor.spawn(task);
    }
}

struct ReactorState {
     // it is expected that only one task owns the resource so at max one task is waiting for the wakeup.
    io_resources: Slab<Option<task::Waker>>,
    is_stopped: bool,
}

struct ReactorInner {
    poll: mio::Poll,
    state: Mutex<ReactorState>,
    _stop_registration: mio::Registration,
    stop_readiness: mio::SetReadiness,
}

impl ReactorInner {
    fn set_stopped(&self) {
        let mut state = self.state.lock().unwrap();

        state.is_stopped = true;
        for maybe_waker in state.io_resources.drain() {
            if let Some(waker) = maybe_waker {
                waker.wake();
            }
        }
    }
}

struct Reactor {
    inner: Arc<ReactorInner>,
    thread: Option<std::thread::JoinHandle<()>>,
}

thread_local! {
    static CURRENT_REACTOR: RefCell<Option<Arc<ReactorInner>>> = RefCell::new(None);
}

impl Reactor {
    fn new() -> io::Result<Reactor> {
        let poll = mio::Poll::new()?;
        let mut io_resources = Slab::new();
        let (stop_registration, stop_readiness) = mio::Registration::new2();
        let stop_key = io_resources.insert(None); // not strictly necessary but easier when tokens are equal to slab keys
        poll.register(&stop_registration, mio::Token(stop_key), mio::Ready::readable(), mio::PollOpt::edge())?;

        let inner = Arc::new(ReactorInner {
            poll,
            state: Mutex::new(ReactorState {
                io_resources,
                is_stopped: false,
            }),
            _stop_registration: stop_registration,
            stop_readiness,
        });

        let thread = std::thread::spawn({
            let inner = inner.clone();
            move || {
                let mut events = mio::Events::with_capacity(1024);
                loop {
                    if let Err(e) = inner.poll.poll(&mut events, None) {
                        eprintln!("reactor error while polling: {}", e);
                        inner.set_stopped();
                        return;
                    }

                    let mut state = inner.state.lock().unwrap();
                    if state.is_stopped {
                        return;
                    }

                    for event in &events {
                        let mut to_wake = state.io_resources.get_mut(event.token().0)
                            .expect("got token for nonexistent io resource")
                            .take();
                        if let Some(waker) = to_wake.take() {
                            waker.wake();
                        }
                    }
                }
            }
        });

        Ok(Reactor {
            inner,
            thread: Some(thread),
        })
    }

    fn set_current(reactor: &Arc<ReactorInner>) -> ScopeGuard<impl FnOnce() -> ()> {
        CURRENT_REACTOR.with(|r| {
            *r.borrow_mut() = Some(reactor.clone());
        });

        ScopeGuard {
            on_exit: Some(|| {
                CURRENT_REACTOR.with(|r| {
                    r.replace(None);
                });
            }),
        }
    }

    fn current() -> Arc<ReactorInner> {
        CURRENT_REACTOR.with(|e| {
            e.borrow().as_ref().expect("no current reactor!").clone()
        })
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        self.inner.set_stopped();
        self.inner.stop_readiness.set_readiness(mio::Ready::readable()).unwrap();

        if let Err(e) = self.thread.take().unwrap().join() {
            eprintln!("reactor error: {:?}", e);
        }
        eprintln!("reactor stopped!");
    }
}

pub struct Runtime {
    _reactor: Reactor,
    executor: Executor,
}

impl Runtime {
    pub fn new(nthreads: usize) -> io::Result<Runtime> {
        let reactor = Reactor::new()?;
        let executor = Executor::new(nthreads, Some(Arc::clone(&reactor.inner)));

        Ok(Runtime {
            _reactor: reactor,
            executor,
        })
    }

    pub fn run<T>(&mut self, task: T)
    where T: Future<Output = ()> + Send + 'static {
        let executor = self.executor.inner.as_ref().unwrap();
        executor.spawn(task);
        executor.block_until_done();
    }
}

struct Registration {
    reactor: Arc<ReactorInner>,
    key: usize,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut reactor_state = self.reactor.state.lock().unwrap();
        if !reactor_state.is_stopped {
            reactor_state.io_resources.remove(self.key);
        }
    }
}

struct IoResource<T: mio::Evented> {
    inner: T,
    registration: Option<Registration>,
}

impl<T: mio::Evented> IoResource<T> {
    fn register(&mut self, interest: mio::Ready, waker: task::Waker) -> io::Result<()> {
        match &self.registration {
            Some(Registration { reactor, key }) => {
                let mut reactor_state = reactor.state.lock().unwrap();
                if reactor_state.is_stopped {
                    return Err(
                        io::Error::new(
                            io::ErrorKind::Other, "reactor already stopped"))
                }

                reactor.poll.reregister(
                    &self.inner, mio::Token(*key), interest, mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                let to_wake = reactor_state.io_resources.get_mut(*key).expect("unknown io resource");
                if let Some(_another_waker) = to_wake.replace(waker) {
                    panic!("io resource was registered with another task!");
                }
            }

            None => {
                // register the waker with poll.
                // waker is associated with toplevel future
                // , so we must save the association io_resource -> waker somewhere.
                let reactor = Reactor::current();
                let mut reactor_state = reactor.state.lock().unwrap();
                if reactor_state.is_stopped {
                    return Err(
                        io::Error::new(
                            io::ErrorKind::Other, "reactor already stopped"))
                }

                let entry = reactor_state.io_resources.vacant_entry();
                let key = entry.key();
                reactor.poll.register(
                    &self.inner, mio::Token(key), interest, mio::PollOpt::edge() | mio::PollOpt::oneshot())?;
                entry.insert(Some(waker));
                self.registration = Some(Registration { reactor: reactor.clone(), key });
            }
        }

        Ok(())
    }
}

pub struct TcpStream {
    resource: IoResource<mio::net::TcpStream>,
}

impl TcpStream {
    pub fn connect(addr: &std::net::SocketAddr) -> io::Result<TcpStream> {
        Ok(TcpStream {
            resource: IoResource {
                inner: mio::net::TcpStream::connect(addr)?,
                registration: None,
            }
        })
    }


    pub fn read<'a, 'b>(&'a mut self, buf: &'b mut [u8]) -> ReadFuture<'a, 'b> {
        ReadFuture { stream: self, buf }
    }

    pub fn write<'a, 'b>(&'a mut self, buf: &'b [u8]) -> WriteFuture<'a, 'b> {
        WriteFuture { stream: self, buf }
    }
}

pub struct ReadFuture<'a, 'b> {
    stream: &'a mut TcpStream,
    buf: &'b mut [u8],
}

impl Future for ReadFuture<'_, '_> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream.resource.inner.read(this.buf) {
            Ok(nread) => task::Poll::Ready(Ok(nread)),

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                if let Err(e) = this.stream.resource.register(mio::Ready::readable(), ctx.waker().clone()) {
                    return task::Poll::Ready(Err(e))
                }
                task::Poll::Pending
            }

            Err(e) => task::Poll::Ready(Err(e)),
        }
    }
}

pub struct WriteFuture<'a, 'b> {
    stream: &'a mut TcpStream,
    buf: &'b [u8],
}

impl Future for WriteFuture<'_, '_> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream.resource.inner.write(this.buf) {
            Ok(nwritten) => task::Poll::Ready(Ok(nwritten)),

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                if let Err(e) = this.stream.resource.register(mio::Ready::writable(), ctx.waker().clone()) {
                    return task::Poll::Ready(Err(e))
                }
                task::Poll::Pending
            }

            Err(e) => task::Poll::Ready(Err(e)),
        }
    }
}

pub struct TcpListener {
    resource: IoResource<mio::net::TcpListener>,
}

impl TcpListener {
    pub fn bind(addr: &std::net::SocketAddr) -> io::Result<TcpListener> {
        Ok(TcpListener {
            resource: IoResource {
                inner: mio::net::TcpListener::bind(addr)?,
                registration: None,
            }
        })
    }


    pub fn accept(&mut self) -> AcceptFuture {
        AcceptFuture { listener: self }
    }
}

pub struct AcceptFuture<'a> {
    listener: &'a mut TcpListener,
}

impl Future for AcceptFuture<'_> {
    type Output = io::Result<(TcpStream, std::net::SocketAddr)>;

    fn poll(self: Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<Self::Output> {
        let this = self.get_mut();
        match this.listener.resource.inner.accept() {
            Ok((stream, addr)) => task::Poll::Ready(Ok(
                (TcpStream {
                    resource: IoResource {
                        inner: stream,
                        registration: None,
                    }
                },
                addr))),

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                if let Err(e) = this.listener.resource.register(mio::Ready::readable(), ctx.waker().clone()) {
                    return task::Poll::Ready(Err(e))
                }
                task::Poll::Pending
            }

            Err(e) => task::Poll::Ready(Err(e)),
        }
    }
}
