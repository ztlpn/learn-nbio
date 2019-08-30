use std::io;

use tokio::{
    prelude::*,
};

struct RequestBuf {
    buf: Vec<u8>,
    pos: usize,
    end: usize,
}

impl RequestBuf {
    fn new() -> RequestBuf {
        RequestBuf {
            buf: vec![0; 4096],
            pos: 0,
            end: 0,
        }
    }

    fn rewind(&mut self) -> io::Result<()> {
        if self.pos == self.buf.len() {
            // we've read the full request and it ended exactly at the end of the buffer.
            self.pos = 0;
            self.end = 0;
        } else if self.end == self.buf.len() {
            if self.pos == 0 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "request too big"));
            } else {
                // we've read part of the request and need to copy it to the beginning of the buffer to read the rest
                self.buf.rotate_left(self.pos);
                self.end = self.end - self.pos;
                self.pos = 0;
            }
        }
        Ok(())
    }
}

impl AsMut<[u8]> for RequestBuf {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.end..]
    }
}

fn process_conn(conn: tokio::net::TcpStream) -> impl Future<Item = (), Error = ()> {
    let peer_addr = conn.peer_addr().unwrap(); // XXX: unwrap
    let (reader, writer) = conn.split();
    let reader = std::io::BufReader::new(reader);

    future::loop_fn((peer_addr, reader, RequestBuf::new(), writer), |(peer_addr, reader, mut in_buf, writer)| {
        in_buf.rewind().into_future().and_then(move |_| {
            tokio::io::read(reader, in_buf).and_then(move |(reader, mut in_buf, nread)| {
                // returning Either to "harmonize" two Futures of different types.
                // Either::A - immediate Result
                // Either::B - proceed with processing the connection

                if nread == 0 {
                    let res = if in_buf.pos != in_buf.end {
                        Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete request"))
                    } else {
                        Ok(future::Loop::Break(()))
                    };

                    return future::Either::A(res.into_future());
                }

                in_buf.end += nread;

                let mut headers = [httparse::EMPTY_HEADER; 16];
                let mut request = httparse::Request::new(&mut headers);
                let parsed = request.parse(&in_buf.buf[in_buf.pos..in_buf.end]);
                let response = match parsed {
                    Ok(httparse::Status::Complete(nparsed)) => {
                        in_buf.pos += nparsed;

                        eprintln!("processing request: {} {}", request.method.unwrap(), request.path.unwrap());

                        // // simulate cpu-intensive work
                        // std::thread::sleep(std::time::Duration::from_millis(100));

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
                               peer_addr, now).unwrap();

                        let mut response = Vec::new();
                        write!(response, "HTTP/1.1 200 OK\r\n\
                                          Server: MyHTTP\r\n\
                                          Content-Type: text/html\r\n\
                                          Content-Length: {}\r\n\
                                          Date: {}\r\n\
                                          \r\n\
                                          {}\r\n",
                               payload.len(), now, payload).unwrap();
                        response
                    }

                    Ok(httparse::Status::Partial) => {
                        return future::Either::A(
                            future::ok(
                                future::Loop::Continue((peer_addr, reader, in_buf, writer))));
                    }

                    Err(_) =>  {
                        return future::Either::A(
                            future::err(
                                io::Error::new(io::ErrorKind::InvalidData, "could not parse request")));
                    }
                };

                future::Either::B(
                    tokio::io::write_all(writer, response)
                        .and_then(move |(writer, _)| {
                            Ok(future::Loop::Continue((peer_addr, reader, in_buf, writer)))
                        }))
            })
        })
    }).map_err(|e| {
        eprintln!("error while processing connection: {}", e);
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string()).parse()?;
    let listener = tokio::net::TcpListener::bind(&addr)?;
    println!("listening on {}", addr);
    let server = listener.incoming()
        .map_err(|e| {
            eprintln!("error while listening: {}", e);
        }).for_each(|conn| {
            tokio::spawn(process_conn(conn))
        });

    tokio::run(server);
    Ok(())
}
