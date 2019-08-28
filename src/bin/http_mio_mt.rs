use std::{
    io::{ self, prelude::* },
    sync::Mutex,
};
use crossbeam::{
    channel,
    thread,
};

trait Task {
    fn register(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()>;

    fn reregister(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()>;

    fn is_ready(&self) -> bool;

    fn add_readiness(&mut self, ready: mio::Ready);

    fn is_finished(&self) -> bool;

    fn make_progress(&mut self, tasks: &Tasks) -> io::Result<()>;
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

        if task.is_ready() {
            // insert a placeholder into the slab and the task into the ready_queue
            entry.insert(None);
            self.ready_sender.send((task, token)).unwrap();
        } else {
            // register the task with poll for later wakeup
            task.register(&self.poll, token)?;
            entry.insert(Some(task));
        }

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
    fn make_progress(&mut self) -> io::Result<()> {
        let mut task = self.task.take().expect("make_progress() called twice!");
        if let Err(e) = task.make_progress(self.parent) {
            eprintln!("error while running task with token {}: {}", self.token.0, e);
            self.parent.slab.lock().unwrap().remove(self.token.0);
        } else if task.is_finished() {
            // eprintln!("token {} finished, removing", self.token.0);
            self.parent.slab.lock().unwrap().remove(self.token.0);
        } else if task.is_ready() {
            // eprintln!("token {}: still ready, adding back to ready queue", self.token.0);
            self.parent.ready_sender.send((task, self.token)).unwrap();
        } else {
            let mut slab = self.parent.slab.lock().unwrap();
            // eprintln!("token {}: blocked, reregistering with poll", self.token.0);
            task.reregister(&self.parent.poll, self.token)?;
            slab.get_mut(self.token.0).unwrap().replace(task);
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
    ready: mio::Ready,
    inner: mio::net::TcpListener,
}

impl Task for Listener {
    fn register(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()> {
        poll.register(&self.inner, token, mio::Ready::all(), mio::PollOpt::edge() | mio::PollOpt::oneshot())
    }

    fn reregister(&self, poll: &mio::Poll, token: mio::Token) -> io::Result<()> {
        poll.reregister(&self.inner, token, mio::Ready::all(), mio::PollOpt::edge() | mio::PollOpt::oneshot())
    }

    fn is_ready(&self) -> bool {
        self.ready.contains(mio::Ready::readable())
    }

    fn add_readiness(&mut self, ready: mio::Ready) {
        self.ready |= ready;
    }

    fn is_finished(&self) -> bool {
        false
    }

    fn make_progress(&mut self, tasks: &Tasks) -> io::Result<()> {
        match self.inner.accept() {
            Ok((stream, addr)) => {
                // println!("got connection from {}", addr);
                let conn_task = Box::new(Connection::new(addr, stream));
                tasks.spawn(conn_task)?;
                Ok(())
            }

            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.ready &= !mio::Ready::readable();
                Ok(())
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
    ready: mio::Ready,

    peer_addr: std::net::SocketAddr,
    stream: mio::net::TcpStream,

    in_buf: Vec<u8>,
    in_pos: usize,
    in_end: usize,

    out_buf: Vec<u8>,
    out_pos: usize,
}

impl Connection {
    fn new(peer_addr: std::net::SocketAddr, stream: mio::net::TcpStream) -> Connection {
        Connection {
            state: State::ReadingRequests,
            ready: mio::Ready::readable() | mio::Ready::writable(),

            peer_addr,
            stream,

            in_buf: vec![0u8; 4096],
            in_pos: 0,
            in_end: 0,

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

    fn add_readiness(&mut self, ready: mio::Ready) {
        self.ready |= ready;
    }

    fn is_ready(&self) -> bool {
        match self.state {
            State::ReadingRequests => {
                self.ready.contains(mio::Ready::readable())
            }

            State::Processing => true,

            State::SendingResponse {..} => {
                self.ready.contains(mio::Ready::writable())
            }

            State::Closing => true,
        }
    }

    fn is_finished(&self) -> bool {
        if let State::Closing = self.state {
            true
        } else {
            false
        }
    }

    fn make_progress(&mut self, _: &Tasks) -> io::Result<()> {
        loop {
            // println!("connection to {}: do work, state: {:?} ready: {:?}", self.peer_addr, self.state, self.ready);
            match self.state {
                State::ReadingRequests => {
                    if self.in_pos == self.in_buf.len() {
                        // we've read the full request and it ended exactly at the end of the buffer.
                        self.in_pos = 0;
                        self.in_end = 0;
                    } else if self.in_end == self.in_buf.len() {
                        if self.in_pos == 0 {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "request too big"));
                        } else {
                            // we've read part of the request and need to copy it to the beginning of the buffer to read the rest
                            self.in_buf.rotate_left(self.in_pos);
                            self.in_end = self.in_end - self.in_pos;
                            self.in_pos = 0;
                        }
                    }

                    match self.stream.read(&mut self.in_buf[self.in_end..]) {
                        Ok(nread) => {
                            if nread == 0 {
                                if self.in_pos != self.in_end {
                                    return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete request"));
                                }

                                // println!("conn to {}: read EOF", self.peer_addr);
                                self.state = State::Closing;
                                return Ok(())
                            }

                            self.in_end += nread;
                            self.state = State::Processing;
                            continue;
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            self.ready &= !mio::Ready::readable();
                            return Ok(())
                        }
                        Err(e) => {
                            eprintln!("error while reading: {}", &e);
                            return Err(e)
                        }
                    }
                }

                State::Processing => {
                    if self.in_pos == self.in_end {
                        self.state = State::ReadingRequests;
                        return Ok(());
                    }

                    let mut headers = [httparse::EMPTY_HEADER; 16];
                    let mut parsed = httparse::Request::new(&mut headers);
                    let res = parsed.parse(&self.in_buf[self.in_pos..self.in_end]);
                    match res {
                        Ok(httparse::Status::Complete(nparsed)) => {
                            self.in_pos += nparsed;

                            eprintln!("processing request: {} {}", parsed.method.unwrap(), parsed.path.unwrap());
                            let now = chrono::Local::now().format("%a, %d %b %Y %T %Z");

                            use std::fmt::Write;

                            // TODO remove allocation from the hot path
                            let mut payload = String::new();
                            write!(payload, "<html>\n\
                                             <head>\n\
                                             <title>Test page</title>\n\
                                             </head>\n\
                                             <body>\n\
                                             <p>Hello, your address is {}, current time is {}.</p>\n\
                                             </body>\n\
                                             </html>",
                                   self.peer_addr, now).unwrap();

                            self.out_buf.clear();
                            self.out_pos = 0;

                            write!(self.out_buf, "HTTP/1.1 200 OK\r\n\
                                                  Server: MyHTTP\r\n\
                                                  Content-Type: text/html\r\n\
                                                  Content-Length: {}\r\n\
                                                  Date: {}\r\n\
                                                  \r\n\
                                                  {}\r\n",
                                   payload.len(), now, payload).unwrap();

                            self.state = State::SendingResponse;
                            continue;
                        }

                        Ok(httparse::Status::Partial) => {
                            self.state = State::ReadingRequests;
                            return Ok(())
                        }

                        Err(_) => {
                            return Err(io::Error::new(io::ErrorKind::InvalidData, "could not parse request"));
                        }
                    }
                }

                State::SendingResponse => {
                    match self.stream.write(&self.out_buf[self.out_pos..]) {
                        Ok(nwritten) => {
                            self.out_pos += nwritten;
                            if self.out_pos == self.out_buf.len() {
                                self.state = State::Processing;
                            }

                            continue;
                        }

                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            self.ready &= !mio::Ready::writable();
                            return Ok(())
                        }

                        Err(e) => {
                            eprintln!("error while sending response: {}", &e);
                            return Err(e)
                        }
                    }
                }

                State::Closing => {
                    return Ok(());
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
            ready: mio::Ready::readable(),
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
                    let mut task = task_slot.take().unwrap();
                    task.add_readiness(event.readiness());
                    if task.is_ready() {
                        tasks.ready_sender.send((task, event.token())).unwrap();
                    } else {
                        task.reregister(&tasks.poll, event.token()).unwrap();
                        task_slot.replace(task);
                    }
                }
            }
        });

        for _ in 0..4 {
            scope.spawn(|_| {
                loop {
                    let mut task_holder = tasks.dequeue();
                    task_holder.make_progress().unwrap();
                }
            });
        }
    }).unwrap();

    Ok(())
}
