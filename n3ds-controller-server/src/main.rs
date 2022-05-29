use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_stream::StreamExt;

#[tokio::main]
async fn main() {
    println!("Starting n3ds-controller server on 0.0.0.0:3535");
    let tcp_listener = TcpListener::bind(("0.0.0.0", 3535))
        .await
        .expect("Failed to bind address");
    let mut tcp_stream = TcpListenerStream::new(tcp_listener);

    while let Some(connection) = tcp_stream.next().await {
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

        let mut connection_lines = connection.lines();
        while let Some(line) = connection_lines.next_line().await.transpose() {
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
