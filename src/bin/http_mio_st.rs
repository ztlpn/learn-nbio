use std::collections::VecDeque;
use std::io::{ self, prelude::* };

#[derive(Debug)]
enum State {
    ReadingRequests,
    Processing,
    SendingResponse,
    Closing,
}

struct Listener {
    ready: mio::Ready,
    inner: mio::net::TcpListener,
}

impl Listener {
    fn add_readiness(&mut self, ready: mio::Ready) {
        self.ready |= ready;
    }

    fn is_ready(&self) -> bool {
        self.ready.contains(mio::Ready::readable())
    }
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

    fn make_progress(&mut self) -> io::Result<()> {
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
                        Err(e) => { return Err(e) }
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

                        Err(e) => { return Err(e) }
                    }
                }

                State::Closing => {
                    return Ok(());
                }
            }
        }
    }
}

fn token_to_slab_key(t: mio::Token) -> usize {
    t.0 - 1
}

fn slab_key_to_token(k: usize) -> mio::Token {
    mio::Token(k + 1)
}

const LISTENER_TOKEN: mio::Token = mio::Token(0);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());

    let poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(1024);
    let mut ready_tokens = VecDeque::new();
    // Invariant: if is_ready() is true for the object represented by token, it is on the ready_tokens queue

    let mut listener = Listener {
        inner: mio::net::TcpListener::bind(&addr.parse()?)?,
        ready: mio::Ready::readable(),
    };
    poll.register(&listener.inner, LISTENER_TOKEN, mio::Ready::all(), mio::PollOpt::edge())?;
    if listener.is_ready() {
        ready_tokens.push_back(LISTENER_TOKEN);
    }
    println!("listening on {}", addr);

    let mut connections = slab::Slab::<Connection>::with_capacity(1024);

    loop {
        if ready_tokens.is_empty() {
            let _n = poll.poll(&mut events, None)?;
            // println!("{} events: {:?}", _n, events);

            for event in events.iter() {
                // println!("got event {:?}", &event);
                if event.token() == LISTENER_TOKEN {
                    let old_is_ready = listener.is_ready();
                    listener.add_readiness(event.readiness());
                    if !old_is_ready && listener.is_ready() {
                        ready_tokens.push_back(event.token());
                    }
                } else {
                    let key = token_to_slab_key(event.token());
                    let conn = connections.get_mut(key).expect("got token for nonexistent connection");

                    let old_is_ready = conn.is_ready();
                    conn.add_readiness(event.readiness());
                    // println!("conn to {}: old is_ready: {}, new is_ready: {}", conn.peer_addr, old_is_ready, conn.is_ready());
                    if !old_is_ready && conn.is_ready() {
                        ready_tokens.push_back(event.token());
                    }
                }
            }
        }

        while let Some(token) = ready_tokens.pop_front() {
            if token == LISTENER_TOKEN {
                match listener.inner.accept() {
                    Ok((stream, addr)) => {
                        // println!("got connection from {}", addr);

                        let entry = connections.vacant_entry();

                        let token = slab_key_to_token(entry.key());
                        poll.register(&stream, token, mio::Ready::all(), mio::PollOpt::edge())?;

                        let conn = Connection::new(addr, stream);
                        if conn.is_ready() {
                            ready_tokens.push_back(token);
                        }
                        entry.insert(conn);
                    }

                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        listener.ready &= !mio::Ready::readable();
                        continue;
                    }

                    Err(e) => {
                        eprintln!("error while accepting connection: {}", &e);
                        return Err(e.into());
                    }
                }

                if listener.is_ready() {
                    ready_tokens.push_back(token);
                }
            } else {
                let key = token_to_slab_key(token);
                let conn = connections.get_mut(key).unwrap();
                if let Err(e) = conn.make_progress() {
                    eprintln!("conn to {}: error: {}", conn.peer_addr, e);
                    connections.remove(key);
                    continue;
                }

                if conn.is_finished() {
                    // println!("conn to {}: finished, removing", conn.peer_addr);
                    connections.remove(key);
                } else if conn.is_ready() {
                    // println!("conn to {}: still ready, adding back to ready queue", conn.peer_addr);
                    ready_tokens.push_back(token);
                }
            }
        }
    }
}
