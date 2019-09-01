use std::{
    result::Result,
    error::Error,
    io::{Read, Write},
    net::TcpListener,
};

fn main() -> Result<(), Box<dyn Error>> {
    let address = std::env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let listener = TcpListener::bind(address)?;

    for maybe_conn in listener.incoming() {
        let conn = maybe_conn?;
        let peer_addr = conn.peer_addr()?;
        eprintln!("new connection from {}!", peer_addr);

        let mut buf = [0u8; 4096];
        let mut pos = 0;
        while pos < buf.len() {
            let nread = (&conn).read(&mut buf[pos..])?;
            eprintln!("read {} bytes, pos: {}", nread, pos);
            if nread == 0 {
                break;
            }
            pos += nread;

            let mut headers = [httparse::EMPTY_HEADER; 16];
            let mut req = httparse::Request::new(&mut headers);
            if req.parse(&buf[..pos])?.is_complete() {
                eprintln!("got req: {} {}", req.method.unwrap(), req.path.unwrap());
                let now = chrono::Local::now().format("%a, %d %b %Y %T %Z");

                use std::fmt::Write;

                let mut payload = String::new();
                write!(payload, "<html>\n\
                                     <head>\n\
                                         <title>Test page</title>\n\
                                     </head>\n\
                                     <body>\n\
                                         <p>Hello, your address is {}, current time is {}.</p>\n\
                                     </body>\n\
                                 </html>",
                       peer_addr, now)?;


                let mut resp = String::new();
                write!(resp, "HTTP/1.1 200 OK\r\n\
                              Server: MyHTTP\r\n\
                              Content-Type: text/html\r\n\
                              Content-Length: {}\r\n\
                              Date: {}\r\n\
                              Connection: close\r\n\
                              \r\n\
                              {}\r\n",
                       payload.len() + 2, now, payload)?;

                (&conn).write_all(resp.as_bytes())?;
                eprintln!("written all");

                break;
            }
        };

    }

    Ok(())
}
