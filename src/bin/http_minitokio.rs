#![feature(try_blocks)]

use std::{
    sync::Arc,
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

struct Runtime {
    poll: mio::Poll,
    io_resources: Slab<Vec<task::Waker>>,
     // toplevel futures go here. the task is either in tasks or in ready_queue.
    tasks: Slab<Option<TaskBox>>,
    ready_queue: VecDeque<(TaskBox, usize)>,
}

type RuntimePtr = Arc<RefCell<Runtime>>;

impl Runtime {
    fn new() -> io::Result<RuntimePtr> {
        let res = Runtime {
            poll: mio::Poll::new()?,
            io_resources: Slab::new(),
            tasks: Slab::new(),
            ready_queue: VecDeque::new(),
        };

        Ok(Arc::new(RefCell::new(res)))
    }

    fn spawn<T>(&mut self, task: T)
    where T: Future<Output = ()> + 'static {
        let pinned = Box::pin(task);
        let key = self.tasks.insert(None);
        self.ready_queue.push_back((pinned, key));
    }

    fn run(runtime: &RuntimePtr) -> io::Result<()> {
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
                // TODO: why do I get BorroMutError if I use while let? lifetime of temp objects is extended??
                let res = runtime.borrow_mut().ready_queue.pop_front();
                match res {
                    Some((mut ready_task, key)) => {
                        let waker = Waker::new(runtime.clone(), key);
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

#[derive(Clone)]
struct Waker {
    runtime: RuntimePtr,
    task_idx: usize,
}

impl Waker {
    fn new(runtime: RuntimePtr, task_idx: usize) -> task::Waker {
        let waker = Box::new(Waker { runtime, task_idx });
        unsafe {
            let raw_waker = task::RawWaker::new(std::mem::transmute(Box::into_raw(waker)), &WAKER_VTABLE);
            task::Waker::from_raw(raw_waker)
        }
    }

    fn wake(&self) {
        // println!("wake() called for task {}!", self.task_idx);
        let mut runtime = self.runtime.borrow_mut();
        let task = runtime.tasks.get_mut(self.task_idx).expect("can't wake nonexistent task");
        if let Some(task) = task.take() {
            runtime.ready_queue.push_back((task, self.task_idx))
        }
    }
}

static WAKER_VTABLE: task::RawWakerVTable = {
    unsafe fn from_ptr(ptr: *const ()) -> Box<Waker> {
        Box::from_raw(std::mem::transmute(ptr))
    }

    unsafe fn clone_fn(waker: *const ()) -> task::RawWaker {
        let waker: *const Waker = std::mem::transmute(waker);
        let cloned_ptr = std::mem::transmute(Box::into_raw(Box::new((*waker).clone())));
        task::RawWaker::new(cloned_ptr, &WAKER_VTABLE)
    }

    unsafe fn wake_fn(waker: *const ()) {
        // consumes the Box
        from_ptr(waker).wake();
    }

    unsafe fn wake_by_ref_fn(waker: *const ()) {
        let waker: *const Waker = std::mem::transmute(waker);
        (&*waker).wake();
    }

    unsafe fn drop_fn(waker: *const ()) {
        drop(from_ptr(waker));
    }

    task::RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn)
};

struct ReadFuture<'a, 'b> {
    stream: &'a mio::net::TcpStream,
    buf: &'b mut [u8],

    runtime: RuntimePtr,
    registered_as: Option<usize>,
}

impl ReadFuture<'_, '_> {
    fn new<'a, 'b>(stream: &'a mio::net::TcpStream, buf: &'b mut [u8], runtime: RuntimePtr) -> ReadFuture<'a, 'b> {
        ReadFuture {
            stream, buf, runtime,
            registered_as: None,
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
                let mut runtime = this.runtime.borrow_mut();
                match this.registered_as {
                    Some(key) => {
                        runtime.io_resources.get_mut(key).expect("unknown io resource").push(ctx.waker().clone());
                    }

                    None => {
                        // register the waker with poll.
                        // waker is associated with toplevel future
                        // , so we must save the association io_resource -> waker somewhere.
                        let (mut io_resources, poll) = RefMut::map_split(
                            runtime,
                            |r| (&mut r.io_resources, &mut r.poll));
                        let entry = io_resources.vacant_entry();
                        let key = entry.key();
                        if let Err(e) = poll.register(
                            this.stream, mio::Token(key), mio::Ready::readable(), mio::PollOpt::edge()) {
                            return task::Poll::Ready(Err(e))
                        }
                        entry.insert(vec![ctx.waker().clone()]);
                        this.registered_as = Some(key);
                    }
                }

                task::Poll::Pending
            }

            Err(e) => task::Poll::Ready(Err(e)),
        }
    }
}

fn main() -> io::Result<()> {
    let runtime = Runtime::new()?;

    let task = {
        let runtime = runtime.clone();
        async move {
            let res: Result<(), Box<dyn std::error::Error>> = try {
                let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());
                println!("connecting to {}", addr);
                let stream = mio::net::TcpStream::connect(&addr.parse()?)?;
                let mut buf = [0u8; 4096];
                loop {
                    let nread = ReadFuture::new(&stream, &mut buf, runtime.clone()).await?;
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
        }
    };


    runtime.borrow_mut().spawn(task);
    Runtime::run(&runtime)
}
