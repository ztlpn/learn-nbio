#![feature(try_blocks)]

use {
    std::io::self,

    nbio::{
        minitokio::{
            self,
            Runtime,
            TcpListener,
            TcpStream,
        },

        RequestBuf,
        process_request,
    }
};

async fn process_conn(mut conn: TcpStream, peer_addr: String)
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
            let mut pos = 0;
            while pos < resp.len() {
                pos += conn.write(&resp[pos..]).await?;
            }
        }
    }
}

fn main() -> io::Result<()> {
    let mut runtime = Runtime::new(3)?;
    runtime.run(async move {
        let res: Result<(), Box<dyn std::error::Error>> = try {
            let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());
            let mut listener = TcpListener::bind(&addr.parse()?)?;
            println!("listening on {}", addr);

            loop {
                let (conn, peer_addr) = listener.accept().await?;
                minitokio::spawn(async move {
                    if let Err(e) = process_conn(conn, peer_addr.to_string()).await {
                        eprintln!("error while processing connection from {}: {}", peer_addr, e);
                    }
                });
            }
        };

        if let Err(e) = res {
            eprintln!("error while listening: {}", e);
        }
    });

    Ok(())
}
