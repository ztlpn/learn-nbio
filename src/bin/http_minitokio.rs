#![feature(try_blocks)]

use std::{
    sync::{Arc, Weak},
    cell::{RefCell, RefMut},
    pin::Pin,
    future::Future,
    task,
    io::{ self, prelude::* },
    collections::VecDeque,
};

use slab::Slab;

// that's the toplevel future
type TaskBox = Pin<Box<dyn Future<Output = ()>>>;

struct RuntimeInner {
    poll: mio::Poll,
    io_resources: Slab<Vec<task::Waker>>,
    // toplevel futures go here. the task is either in tasks or in ready_queue.
    tasks: Slab<Option<TaskBox>>,
    ready_queue: VecDeque<(TaskBox, usize)>,
}

impl RuntimeInner {
    fn spawn<T>(&mut self, task: T) where T: Future<Output = ()> + 'static {
        let pinned = Box::pin(task);
        let key = self.tasks.insert(None);
        self.ready_queue.push_back((pinned, key));
    }
}

type RuntimeInnerHandle = Arc<RefCell<RuntimeInner>>;

struct Runtime {
    inner: RuntimeInnerHandle,
}

thread_local! {
    static RUNTIME: RefCell<Option<RuntimeInnerHandle>> = RefCell::default();
}

fn current_runtime() -> RuntimeInnerHandle {
    RUNTIME.with(|r| {
        r.borrow().as_ref().expect("no current runtime!").clone()
    })
}

struct RunGuard<'a> {
    _runtime: &'a Runtime,
}

impl<'a> RunGuard<'a> {
    fn new(runtime: &Runtime) -> RunGuard {
        RUNTIME.with(|r| {
            *r.borrow_mut() = Some(runtime.inner.clone());
        });

        RunGuard { _runtime: runtime }
    }
}

impl<'a> Drop for RunGuard<'a> {
    fn drop(&mut self) {
        RUNTIME.with(|r| {
            *r.borrow_mut() = None;
        });
    }
}

static WAKER_VTABLE: task::RawWakerVTable = {
    unsafe fn clone_fn(task_idx: *const ()) -> task::RawWaker {
        task::RawWaker::new(task_idx, &WAKER_VTABLE)
    }

    unsafe fn wake_fn(task_idx: *const ()) {
        // consumes the Box
        let task_idx: usize = std::mem::transmute(task_idx);
        let runtime_handle = current_runtime();
        let mut runtime = runtime_handle.borrow_mut();
        let task = runtime.tasks.get_mut(task_idx).expect("can't wake nonexistent task");
        if let Some(task) = task.take() {
            runtime.ready_queue.push_back((task, task_idx))
        }
    }

    unsafe fn wake_by_ref_fn(task_idx: *const ()) {
        wake_fn(task_idx);
    }

    unsafe fn drop_fn(_task_idx: *const ()) {
    }

    task::RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn)
};

fn new_waker(task_idx: usize) -> task::Waker {
    unsafe {
        task::Waker::from_raw(task::RawWaker::new(std::mem::transmute(task_idx), &WAKER_VTABLE))
    }
}

pub fn spawn<T>(task: T)
where T: Future<Output = ()> + 'static {
    current_runtime().borrow_mut().spawn(task);
}

impl Runtime {
    pub fn new() -> io::Result<Runtime> {
        let inner = RuntimeInner {
            poll: mio::Poll::new()?,
            io_resources: Slab::new(),
            tasks: Slab::new(),
            ready_queue: VecDeque::new(),
        };

        Ok(Runtime { inner: Arc::new(RefCell::new(inner)), })
    }

    pub fn run<T>(&mut self, task: T) -> io::Result<()>
    where T: Future<Output = ()> + 'static {
        let _run_guard = RunGuard::new(self);

        let runtime = &self.inner;
        runtime.borrow_mut().spawn(task);

        let mut events = mio::Events::with_capacity(1024);

        while !runtime.borrow().tasks.is_empty() {
            if runtime.borrow().ready_queue.is_empty() {
                runtime.borrow().poll.poll(&mut events, None)?;

                for event in &events {
                    let mut wakers = vec![];
                    std::mem::swap(
                        runtime.borrow_mut().io_resources.get_mut(event.token().0)
                            .expect("got token for nonexistent io resource"),
                        &mut wakers);
                    for waker in wakers.into_iter() {
                        waker.wake();
                    }
                }
            }

            loop {
                // Work around https://github.com/rust-lang/rust/issues/38355
                let res = runtime.borrow_mut().ready_queue.pop_front();
                match res {
                    Some((mut ready_task, key)) => {
                        let waker = new_waker(key);
                        let mut ctx = task::Context::from_waker(&waker);
                        match ready_task.as_mut().poll(&mut ctx) {
                            task::Poll::Ready(()) => {
                                eprintln!("task {} done", key);
                                runtime.borrow_mut().tasks.remove(key);
                            }

                            task::Poll::Pending => {
                                runtime.borrow_mut().tasks.get_mut(key)
                                    .expect("got key for nonexistent task from ready queue")
                                    .replace(ready_task);
                            }
                        }
                    }

                    None => break,
                }
            }
        }

        Ok(())
    }
}

struct Registration {
    runtime: Weak<RefCell<RuntimeInner>>, // storing Weak to runtime to avoid cycles during cancellation.
    key: usize,
}

struct ReadFuture<'a, 'b> {
    stream: &'a mio::net::TcpStream,
    registration: Option<Registration>,

    buf: &'b mut [u8],
}

impl ReadFuture<'_, '_> {
    fn new<'a, 'b>(stream: &'a mio::net::TcpStream, buf: &'b mut [u8]) -> ReadFuture<'a, 'b> {
        ReadFuture {
            stream,
            registration: None,
            buf,
        }
    }
}

impl Future for ReadFuture<'_, '_> {
    type Output = io::Result<usize>;

    fn poll(self: Pin<&mut Self>, ctx: &mut task::Context) -> task::Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream.read(this.buf) {
            Ok(nread) => task::Poll::Ready(Ok(nread)),

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                match &this.registration {
                    Some(Registration { runtime, key }) => {
                        match Weak::upgrade(runtime) {
                            Some(runtime) => {
                                runtime.borrow_mut()
                                    .io_resources.get_mut(*key).expect("unknown io resource")
                                    .push(ctx.waker().clone());
                            }

                            None => return task::Poll::Ready(Err(
                                io::Error::new(
                                    io::ErrorKind::Other, "runtime already dropped"))),
                        }
                    }

                    None => {
                        // register the waker with poll.
                        // waker is associated with toplevel future
                        // , so we must save the association io_resource -> waker somewhere.
                        let runtime = current_runtime();
                        let (mut io_resources, poll) = RefMut::map_split(
                            runtime.borrow_mut(),
                            |r| (&mut r.io_resources, &mut r.poll));
                        let entry = io_resources.vacant_entry();
                        let key = entry.key();
                        if let Err(e) = poll.register(
                            this.stream, mio::Token(key), mio::Ready::readable(), mio::PollOpt::edge()) {
                            return task::Poll::Ready(Err(e))
                        }
                        entry.insert(vec![ctx.waker().clone()]);
                        this.registration = Some(Registration { runtime: Arc::downgrade(&runtime), key });
                    }
                }

                task::Poll::Pending
            }

            Err(e) => task::Poll::Ready(Err(e)),
        }
    }
}

fn main() -> io::Result<()> {
    let mut runtime = Runtime::new()?;
    runtime.run(async move {
        let res: Result<(), Box<dyn std::error::Error>> = try {
            let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());
            println!("connecting to {}", addr);
            let stream = mio::net::TcpStream::connect(&addr.parse()?)?;
            let mut buf = [0u8; 4096];
            loop {
                let nread = ReadFuture::new(&stream, &mut buf).await?;
                if nread == 0 {
                    println!("server ended the connection");
                    break;
                } else {
                    println!("read: {}", String::from_utf8(buf[..nread].to_vec())?);
                }
            }
        };

        if let Err(e) = res {
            eprintln!("error: {}", e);
        }
    })
}
