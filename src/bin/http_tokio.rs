use tokio01 as tokio;

use tokio::{
    prelude::*,
};

use rust_http::{ RequestBuf, process_request };

fn process_conn(conn: tokio::net::TcpStream) -> impl Future<Item = (), Error = ()> {
    let peer_addr = conn.peer_addr().unwrap().to_string(); // XXX: unwrap
    let (reader, writer) = conn.split();
    let reader = std::io::BufReader::new(reader);

    future::loop_fn((peer_addr, reader, RequestBuf::new(), writer), |(peer_addr, reader, mut in_buf, writer)| {
        in_buf.rewind().into_future().and_then(move |_| {
            tokio::io::read(reader, in_buf).and_then(move |(reader, mut in_buf, nread)| {
                in_buf.advance(nread).into_future().and_then(move |_| {
                    // returning Either to "harmonize" two Futures of different types.
                    // Either::A - immediate Result
                    // Either::B - proceed with processing the connection

                    if nread == 0 {
                        eprintln!("client closed the connection");
                        return future::Either::A(
                            future::ok(future::Loop::Break(())));
                    }

                    let mut response = Vec::new();
                    match process_request(&mut in_buf, &peer_addr, &mut response) {
                        Ok(true) => future::Either::B(
                            tokio::io::write_all(writer, response)
                                .and_then(move |(writer, _)| {
                                    Ok(future::Loop::Continue((peer_addr, reader, in_buf, writer)))
                                })),

                        Ok(false) => future::Either::A(
                            future::ok(
                                future::Loop::Continue((peer_addr, reader, in_buf, writer)))),

                        Err(e) => future::Either::A(future::err(e)),
                    }
                })
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
