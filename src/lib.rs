use std::io;

pub fn process_request(bytes: &[u8], peer_addr: &str, out_buf: &mut impl io::Write)
        -> Result<Option<usize>, io::Error> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Request::new(&mut headers);
    let res = parsed.parse(bytes);
    match res {
        Ok(httparse::Status::Complete(nparsed)) => {
            // eprintln!("processing request: {} {}", parsed.method.unwrap(), parsed.path.unwrap());

            // // simulate cpu-intensive work
            // std::thread::sleep(std::time::Duration::from_millis(100));

            let now = chrono::Local::now().format("%a, %d %b %Y %T %Z");

            use std::fmt::Write;
            // TODO remove allocations from the hot path
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

            write!(out_buf, "HTTP/1.1 200 OK\r\n\
                             Server: MyHTTP\r\n\
                             Content-Type: text/html\r\n\
                             Content-Length: {}\r\n\
                             Date: {}\r\n\
                             \r\n\
                             {}\r\n",
                   payload.len(), now, payload).unwrap();

            Ok(Some(nparsed))
        }

        Ok(httparse::Status::Partial) => Ok(None),

        Err(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "could not parse request")),
    }
}
