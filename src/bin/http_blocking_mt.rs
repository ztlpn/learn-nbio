use std::{
    result::Result,
    error::Error,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    thread,
};

use rust_http::{ RequestBuf, process_request };

fn process_conn(conn: TcpStream) -> Result<(), Box<dyn Error>> {
    let peer_addr = conn.peer_addr()?.to_string();
    eprintln!("new connection from {}!", peer_addr);

    let mut in_buf = RequestBuf::new();
    let mut resp = Vec::new();

    loop {
        in_buf.rewind()?;
        let nread = (&conn).read(in_buf.as_mut())?;

        in_buf.advance(nread)?;
        if nread == 0 {
            eprintln!("client closed the connection");
            return Ok(());
        }

        resp.clear();
        if process_request(&mut in_buf, &peer_addr, &mut resp)? {
            (&conn).write_all(&resp)?;
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
