#![feature(try_blocks)]

use {
    std::io::self,

    nbio::minitokio::{
        Runtime,
        spawn,
        TcpListener,
    }
};

fn main() -> io::Result<()> {
    let mut runtime = Runtime::new()?;
    runtime.run(async move {
        let res: Result<(), Box<dyn std::error::Error>> = try {
            let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string());
            let mut listener = TcpListener::bind(&addr.parse()?)?;
            println!("listening on {}", addr);

            loop {
                let (mut stream, addr) = listener.accept().await?;
                println!("accepted connection from {}!", addr);

                spawn(async move {
                    let res: Result<(), Box<dyn std::error::Error>> = try {
                        let mut buf = [0u8; 4096];
                        loop {
                            let nread = stream.read(&mut buf).await?;
                            if nread == 0 {
                                println!("client ended the connection");
                                break;
                            } else {
                                let text = String::from_utf8(buf[..nread].to_vec())?;
                                println!("read: {}", text);
                                let reply = "Hello, ".to_owned() + &text;
                                let mut pos = 0;
                                while pos < reply.as_bytes().len() {
                                    pos += stream.write(&reply.as_bytes()[pos..]).await?;
                                }
                            }
                        }
                    };

                    if let Err(e) = res {
                        eprintln!("error on connection from {}: {}", addr, e);
                    }
                });
            }
        };

        if let Err(e) = res {
            eprintln!("error: {}", e);
        }
    })
}
