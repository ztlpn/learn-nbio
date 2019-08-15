use std::{
    result::Result,
    error::Error,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

fn process_conn(conn: TcpStream) -> Result<(), Box<dyn Error>> {
    let peer_addr = conn.peer_addr()?;
    eprintln!("new connection from {}!", peer_addr);

    let mut payload = String::new();
    let mut resp = String::new();

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

            use std::fmt::Write;

            payload.clear();
            write!(payload, "<html>\n\
                                 <head>\n\
                                     <title>Test page</title>\n\
                                 </head>\n\
                                 <body>\n\
                                     <p>Hello, your address is {}, current time is {}.</p>\n\
                                 </body>\n\
                             </html>",
                   peer_addr, now)?;

            resp.clear();
            write!(resp, "HTTP/1.1 200 OK\r\n\
                          Server: MyHTTP\r\n\
                          Content-Type: text/html\r\n\
                          Content-Length: {}\r\n\
                          Date: {}\r\n\
                          \r\n\
                          {}\r\n",
                   payload.len(), now, payload)?;

            (&conn).write_all(resp.as_bytes())?;

            // read and process the next request in this connection.
            pos = 0;
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let address = std::env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let listener = TcpListener::bind(address)?;

    for maybe_conn in listener.incoming() {
        let conn = maybe_conn?;
        thread::spawn(move || { process_conn(conn).unwrap(); });
    }

    Ok(())
}
