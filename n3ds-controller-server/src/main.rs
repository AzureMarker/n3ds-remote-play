use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;

fn main() {
    println!("Starting n3ds-controller server on 0.0.0.0:3535");
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535)).expect("Failed to bind address");

    for connection in tcp_listener.incoming() {
        let connection = match connection {
            Ok(connection) => BufReader::new(connection),
            Err(e) => {
                eprintln!("New connection error: {e}");
                continue;
            }
        };
        let peer_addr = match connection.get_ref().peer_addr() {
            Ok(peer_addr) => peer_addr,
            Err(e) => {
                eprintln!("Error while getting peer address: {e}");
                continue;
            }
        };

        println!("New connection from {peer_addr}");

        for line in connection.lines() {
            match line {
                Ok(line) => println!("[{peer_addr}] {line}"),
                Err(e) => {
                    eprintln!("Error while reading stream: {e}");
                    if e.kind() == std::io::ErrorKind::ConnectionReset {
                        break;
                    }
                }
            }
        }

        println!("Closing connection with [{peer_addr}]");
    }
}
