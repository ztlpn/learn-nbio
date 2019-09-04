use tokio02 as tokio;

use tokio::{
    prelude::*,
};

use nbio::{ RequestBuf, process_request };

async fn process_conn(mut conn: tokio::net::TcpStream, peer_addr: String)
                      -> Result<(), Box<dyn std::error::Error>> {
    let mut in_buf = RequestBuf::new();
    let mut resp = Vec::new();

    loop {
        in_buf.rewind()?;
        let nread = conn.read(in_buf.as_mut()).await?;

        in_buf.advance(nread)?;
        if nread == 0 {
            eprintln!("client closed the connection from {}", peer_addr);
            return Ok(());
        }

        resp.clear();
        if process_request(&mut in_buf, &peer_addr, &mut resp)? {
            conn.write_all(&resp).await?;
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());
    let mut listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("listening on {}", addr);

    loop {
        let (conn, peer_addr) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(e) = process_conn(conn, peer_addr.to_string()).await {
                eprintln!("error while processing connection from {}: {}", peer_addr, e);
            }
        });
    }
}
