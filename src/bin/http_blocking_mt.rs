use std::{
    result::Result,
    error::Error,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

fn process_conn(conn: TcpStream) -> Result<(), Box<dyn Error>> {
    let peer_addr = conn.peer_addr()?.to_string();
    eprintln!("new connection from {}!", peer_addr);

    let mut buf = [0u8; 4096];
    let mut pos = 0;
    let mut resp = Vec::new();

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

        resp.clear();
        if let Some(_) = rust_http::process_request(&buf[..pos], &peer_addr, &mut resp)? {
            (&conn).write_all(&resp)?;
            // read and process the next request in this connection.
            resp.clear();
            pos = 0;
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8080".to_string());
    let listener = TcpListener::bind(&addr)?;
    println!("listening on {}", addr);

    for maybe_conn in listener.incoming() {
        let conn = maybe_conn?;
        thread::spawn(move || { process_conn(conn).unwrap(); });
    }

    Ok(())
}
