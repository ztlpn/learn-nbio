use tokio::{
    prelude::*,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::args().nth(1).unwrap_or("127.0.0.1:8000".to_string()).parse()?;
    let listener = tokio::net::TcpListener::bind(&addr)?;
    println!("listening on {}", addr);
    let server = listener.incoming()
        .map_err(|e| {
            eprintln!("error while listening: {}", e);
        }).for_each(|conn| {
            println!("accepted conn: {}", conn.peer_addr().unwrap());

            let (reader, writer) = conn.split();
            let reader = std::io::BufReader::new(reader);

            tokio::spawn(
                tokio::io::lines(reader)
                    .fold(writer, |writer, line| {
                        println!("got line {}", line);
                        let response = format!("hello, {}\n", line);
                        tokio::io::write_all(writer, response.into_bytes())
                            .map(|(writer, _)| writer)
                    }).map_err(|e| {
                        eprintln!("error while processing connection: {}", e);
                    }).map(|_| ()))
        });

    tokio::run(server);
    Ok(())
}
