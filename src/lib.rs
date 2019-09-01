use std::io;

pub struct RequestBuf {
    buf: Vec<u8>,
    pos: usize,
    end: usize,
}

impl RequestBuf {
    pub fn new() -> RequestBuf {
        RequestBuf {
            buf: vec![0; 4096],
            pos: 0,
            end: 0,
        }
    }

    pub fn advance(&mut self, nread: usize) -> io::Result<()> {
        if nread == 0 && self.pos != self.end {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "incomplete request"))
        }

        self.end += nread;
        Ok(())
    }

    pub fn rewind(&mut self) -> io::Result<()> {
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

/// return true if the request was processed
pub fn process_request(in_buf: &mut RequestBuf, peer_addr: &str, out_buf: &mut impl io::Write)
        -> Result<bool, io::Error> {
    let mut headers = [httparse::EMPTY_HEADER; 16];
    let mut parsed = httparse::Request::new(&mut headers);
    let res = parsed.parse(&in_buf.buf[in_buf.pos..in_buf.end]);
    match res {
        Ok(httparse::Status::Complete(nparsed)) => {
            in_buf.pos += nparsed;

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

            Ok(true)
        }

        Ok(httparse::Status::Partial) => Ok(false),

        Err(_) => Err(io::Error::new(io::ErrorKind::InvalidData, "could not parse request")),
    }
}
