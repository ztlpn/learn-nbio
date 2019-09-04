use std::{
    io::{ self, prelude::* },
    sync::Mutex,
};

use crossbeam::{
    channel,
    thread,
};

use nbio::{ RequestBuf, process_request };

enum Readiness {
    Ready,
    Blocked,
    Finished,
}

trait Task {
    fn register(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()>;

    fn reregister(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()>;

    fn make_progress(&mut self, tasks: &Tasks) -> io::Result<Readiness>;
}

type TaskBox = Box<dyn Task + Send>;

struct Tasks {
    slab: Mutex<slab::Slab<Option<TaskBox>>>,
    poll: mio::Poll,
    ready_sender: channel::Sender<(TaskBox, mio::Token)>,
    ready_receiver: channel::Receiver<(TaskBox, mio::Token)>,
    // Invariant: if is_ready() is false then it is in the slab and registered with poll,
    // else it is either in the ready_queue, or checked out in the task holder and not registered with poll.
}

impl Tasks {
    fn with_capacity(cap: usize, poll: mio::Poll) -> Tasks {
        let (ready_sender, ready_receiver) = channel::unbounded();
        Tasks {
            slab: Mutex::new(slab::Slab::with_capacity(cap)),
            poll,
            ready_sender, ready_receiver,
        }
    }

    fn spawn(&self, task: TaskBox) -> io::Result<()> {
        let mut slab = self.slab.lock().unwrap();

        let entry = slab.vacant_entry();
        let token = mio::Token(entry.key());
        task.register(&self.poll, token)?;
        entry.insert(Some(task));

        Ok(())
    }

    fn dequeue(&self) -> TaskHolder {
        let (task, token) = self.ready_receiver.recv().unwrap();
        TaskHolder {
            parent: self,
            task: Some(task),
            token,
        }
    }
}


struct TaskHolder<'a> {
    parent: &'a Tasks,
    task: Option<TaskBox>,
    token: mio::Token,
}

impl<'a> TaskHolder<'a> {
    fn make_progress(mut self) -> io::Result<()> {
        let mut task = self.task.take().unwrap();

        match task.make_progress(self.parent) {
            Ok(Readiness::Finished) => {
                // eprintln!("token {} finished, removing", self.token.0);
                self.parent.slab.lock().unwrap().remove(self.token.0);
            }

            Ok(Readiness::Ready) => {
                // eprintln!("token {}: still ready, adding back to ready queue", self.token.0);
                self.parent.ready_sender.send((task, self.token)).unwrap();
            }

            Ok(Readiness::Blocked) => {
                let mut slab = self.parent.slab.lock().unwrap();
                // eprintln!("token {}: blocked, reregistering with poll", self.token.0);
                task.reregister(&self.parent.poll, self.token)?;
                slab.get_mut(self.token.0).unwrap().replace(task);
            }

            Err(e) => {
                eprintln!("error while running task with token {}: {}", self.token.0, e);
                self.parent.slab.lock().unwrap().remove(self.token.0);
            }
        }

        Ok(())
    }
}

impl<'a> Drop for TaskHolder<'a> {
    fn drop(&mut self) {
        if let Some(_) = self.task.take() {
            panic!("no progress was made on the task!");
        }
    }
}

struct Listener {
    inner: mio::net::TcpListener,
}

impl Task for Listener {
    fn register(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()> {
        poll.register(&self.inner, token, mio::Ready::all(), mio::PollOpt::edge() | mio::PollOpt::oneshot())
    }

    fn reregister(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()> {
        poll.reregister(&self.inner, token, mio::Ready::all(), mio::PollOpt::edge() | mio::PollOpt::oneshot())
    }

    fn make_progress(&mut self, tasks: &Tasks) -> io::Result<Readiness> {
        match self.inner.accept() {
            Ok((stream, addr)) => {
                // println!("got connection from {}", addr);
                let conn_task = Box::new(Connection::new(addr, stream));
                tasks.spawn(conn_task)?;
                Ok(Readiness::Ready)
            }

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                Ok(Readiness::Blocked)
            }

            Err(e) => {
                eprintln!("error while accepting connection: {}", &e);
                Err(e.into())
            }
        }
    }
}

#[derive(Debug)]
enum State {
    ReadingRequests,
    Processing,
    SendingResponse,
    Closing,
}

struct Connection {
    state: State,

    peer_addr: String,
    stream: mio::net::TcpStream,

    in_buf: RequestBuf,

    out_buf: Vec<u8>,
    out_pos: usize,
}

impl Connection {
    fn new(peer_addr: std::net::SocketAddr, stream: mio::net::TcpStream) -> Connection {
        Connection {
            state: State::ReadingRequests,

            peer_addr: peer_addr.to_string(),
            stream,

            in_buf: RequestBuf::new(),

            out_buf: Vec::new(),
            out_pos: 0,
        }
    }
}

impl Task for Connection {
    fn register(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()> {
        poll.register(&self.stream, token, mio::Ready::all(), mio::PollOpt::edge() | mio::PollOpt::oneshot())
    }

    fn reregister(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()> {
        poll.reregister(&self.stream, token, mio::Ready::all(), mio::PollOpt::edge() | mio::PollOpt::oneshot())
    }

    fn make_progress(&mut self, _: &Tasks) -> io::Result<Readiness> {
        loop {
            // println!("connection to {}: do work, state: {:?} ready: {:?}", self.peer_addr, self.state, self.ready);
            match self.state {
                State::ReadingRequests => {
                    self.in_buf.rewind()?;

                    match self.stream.read(self.in_buf.as_mut()) {
                        Ok(nread) => {
                            self.in_buf.advance(nread)?;
                            if nread == 0 {
                                // println!("conn to {}: read EOF", self.peer_addr);
                                self.state = State::Closing;
                                return Ok(Readiness::Finished)
                            }

                            self.state = State::Processing;
                            continue;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Ok(Readiness::Blocked)
                        }
                        Err(e) => {
                            eprintln!("error while reading: {}", &e);
                            return Err(e)
                        }
                    }
                }

                State::Processing => {
                    if process_request(&mut self.in_buf, &self.peer_addr, &mut self.out_buf)? {
                        self.state = State::SendingResponse;
                        continue;
                    } else {
                        self.state = State::ReadingRequests;
                        return Ok(Readiness::Ready)
                    }
                }

                State::SendingResponse => {
                    match self.stream.write(&self.out_buf[self.out_pos..]) {
                        Ok(nwritten) => {
                            self.out_pos += nwritten;
                            if self.out_pos == self.out_buf.len() {
                                self.out_buf.clear();
                                self.out_pos = 0;
                                self.state = State::Processing;
                            }

                            continue;
                        }

                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            return Ok(Readiness::Blocked)
                        }

                        Err(e) => {
                            eprintln!("error while sending response: {}", &e);
                            return Err(e)
                        }
                    }
                }

                State::Closing => {
                    return Ok(Readiness::Finished);
                }
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tasks = Tasks::with_capacity(1024, mio::Poll::new()?);

    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());
    let listener = Box::new(
        Listener {
            inner: mio::net::TcpListener::bind(&addr.parse()?)?,
        });
    tasks.spawn(listener)?;
    println!("listening on {}", addr);

    thread::scope(|scope| {
        scope.spawn(|_| {
            loop {
                let mut events = mio::Events::with_capacity(1024);
                let _n = tasks.poll.poll(&mut events, None).unwrap();
                // eprintln!("{} events: {:?}", _n, events);
                let mut slab = tasks.slab.lock().unwrap();
                for event in events.iter() {
                    // eprintln!("got event {:?}", &event);
                    let key = event.token().0;
                    let task_slot = slab.get_mut(key).expect("got token for nonexistent task");
                    if let Some(task) = task_slot.take() {
                        tasks.ready_sender.send((task, event.token())).unwrap();
                    }
                }
            }
        });

        for _ in 0..3 {
            scope.spawn(|_| {
                loop {
                    let task_holder = tasks.dequeue();
                    task_holder.make_progress().unwrap();
                }
            });
        }
    }).unwrap();

    Ok(())
}
