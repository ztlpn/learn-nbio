use std::{
    result::Result,
    error::Error,
    sync::Arc,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

const MAX_REQUESTS_PER_CONN: u32 = 100;

fn process_conn(conn: TcpStream, worker: usize) -> Result<(), Box<dyn Error>> {
    let peer_addr = conn.peer_addr()?;
    eprintln!("new connection from {}!", peer_addr);

    let mut payload = String::new();
    let mut resp = String::new();

    let mut num_processed = 0;

    let mut buf = [0u8; 4096];
    let mut pos = 0;
    loop {
        let nread = (&conn).read(&mut buf[pos..])?;
        if nread == 0 {
            eprintln!("client closed the connection");
            if pos == 0 {
                return Ok(());
            } else {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "client sent incomplete request").into());
            }
        }
        pos += nread;

        if pos == buf.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request too big").into());
        }

        let mut headers = [httparse::EMPTY_HEADER; 16];
        let mut req = httparse::Request::new(&mut headers);
        if req.parse(&buf[..pos])?.is_complete() {
            eprintln!("got req: {} {}", req.method.unwrap(), req.path.unwrap());
            let now = chrono::Local::now().format("%a, %d %b %Y %T %Z");
            num_processed += 1;

            use std::fmt::Write;

            payload.clear();
            write!(payload, "<html>\n\
                                 <head>\n\
                                     <title>Test page</title>\n\
                                 </head>\n\
                                 <body>\n\
                                     <p>Hello from worker #{}, your address is {}, current time is {}.</p>\n\
                                 </body>\n\
                             </html>",
                   worker, peer_addr, now)?;

            resp.clear();
            write!(resp, "HTTP/1.1 200 OK\r\n\
                          Server: MyHTTP\r\n\
                          Content-Type: text/html\r\n\
                          {}\
                          Content-Length: {}\r\n\
                          Date: {}\r\n\
                          \r\n\
                          {}\r\n",
                   if num_processed < MAX_REQUESTS_PER_CONN { "" } else { "Connection: close\r\n" },
                   payload.len() + 2, now, payload)?;

            (&conn).write_all(resp.as_bytes())?;

            if num_processed < MAX_REQUESTS_PER_CONN {
                // read and process the next request in this connection.
                pos = 0;
            } else {
                return Ok(());
            }
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let address = std::env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let listener = Arc::new(TcpListener::bind(address)?);

    let mut threads = Vec::new();
    for i in 0..4 {
        threads.push(thread::spawn({
            let listener = listener.clone();
            move || {
                for maybe_conn in listener.incoming() {
                    let conn = maybe_conn.unwrap();
                    process_conn(conn, i).unwrap();
                }
            }
        }));
    }

    for thread in threads.into_iter() {
        thread.join().unwrap();
    }

    Ok(())
}
