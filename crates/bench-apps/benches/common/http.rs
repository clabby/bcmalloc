//! A hyper server and its clients in one process.
//!
//! The server listens on a loopback port that the system picks, inside a multi-threaded tokio
//! runtime. Several clients, each on its own keep-alive connection, send small JSON requests one
//! after the other. The server parses every body, builds an answer and sends it back, and the
//! client parses that too. Every pass starts its own runtime and server and shuts both down, and
//! the clock covers only the requests.

use super::{
    Pass, Scale,
    text::{Rng, WORDS},
    twice,
};
use http_body_util::{BodyExt, Full};
use hyper::{
    Request, Response,
    body::{Bytes, Incoming},
    client::conn::http1 as client,
    server::conn::http1 as server,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, net::SocketAddr, time::Instant};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    task::JoinSet,
};

const WORKERS: usize = 4;
const CONNECTIONS: usize = 8;

#[derive(Serialize, Deserialize)]
struct Line {
    name: String,
    quantity: u32,
    cents: u32,
}

#[derive(Serialize, Deserialize)]
struct Order {
    id: u64,
    customer: String,
    lines: Vec<Line>,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    id: u64,
    total: u64,
    names: Vec<String>,
    note: String,
}

fn total(order: &Order) -> u64 {
    order.lines.iter().map(|line| u64::from(line.quantity) * u64::from(line.cents)).sum()
}

async fn handle(request: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    let body = request.into_body().collect().await.expect("a complete body").to_bytes();
    let order: Order = serde_json::from_slice(&body).expect("a valid order");
    let receipt = Receipt {
        id: order.id,
        total: total(&order),
        names: order.lines.iter().map(|line| line.name.to_uppercase()).collect(),
        note: format!("order {} for {}", order.id, order.customer),
    };
    let bytes = serde_json::to_vec(&receipt).expect("serializable");
    let response = Response::builder()
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(bytes)))
        .expect("a valid response");
    Ok(response)
}

/// Serves connections until told to stop, then waits for them to finish. They finish when their
/// clients hang up.
async fn serve(listener: TcpListener, mut stop: watch::Receiver<bool>) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.expect("accept");
                stream.set_nodelay(true).expect("nodelay");
                connections.spawn(async move {
                    let io = TokioIo::new(stream);
                    let _ = server::Builder::new().serve_connection(io, service_fn(handle)).await;
                });
            }
            _ = stop.changed() => break,
        }
    }
    while connections.join_next().await.is_some() {}
}

fn order(rng: &mut Rng, id: u64) -> Order {
    let lines = (0..rng.between(1, 6))
        .map(|_| Line {
            name: rng.pick(WORDS).to_string(),
            quantity: rng.between(1, 9) as u32,
            cents: rng.between(50, 20_000) as u32,
        })
        .collect();
    Order { id, customer: format!("{} {}", rng.pick(WORDS), rng.pick(WORDS)), lines }
}

/// One keep-alive connection that sends `requests` orders and checks every receipt.
async fn client_task(address: SocketAddr, index: usize, requests: usize) {
    let stream = TcpStream::connect(address).await.expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    let (mut sender, connection) =
        client::handshake(TokioIo::new(stream)).await.expect("handshake");
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut rng = Rng::new(0x4854_5450 + index as u64);
    for sequence in 0..requests {
        let id = (index * requests + sequence) as u64;
        let order = order(&mut rng, id);
        let request = Request::post("/orders")
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(serde_json::to_vec(&order).expect("serializable"))))
            .expect("a valid request");
        let response = sender.send_request(request).await.expect("a response");
        let body = response.into_body().collect().await.expect("a complete body").to_bytes();
        let receipt: Receipt = serde_json::from_slice(&body).expect("a valid receipt");
        assert!(receipt.id == id && receipt.total == total(&order));
    }
    // Dropping the sender closes the connection, which lets the server's task and ours end.
    drop(sender);
    driver.await.expect("the driver ends");
}

fn pass(requests: usize) -> Pass {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .enable_io()
        .build()
        .expect("a runtime");
    let pass = runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("an address");
        let (stop, stopped) = watch::channel(false);
        let server = tokio::spawn(serve(listener, stopped));

        let start = Instant::now();
        let clients: Vec<_> = (0..CONNECTIONS)
            .map(|index| tokio::spawn(client_task(address, index, requests)))
            .collect();
        for client in clients {
            client.await.expect("the client ends");
        }
        let pass = Pass::since(start, (requests * CONNECTIONS) as f64 / 1e3);

        stop.send(true).expect("the server listens");
        server.await.expect("the server ends");
        pass
    });
    drop(runtime);
    pass
}

/// One unit is a thousand requests.
pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let requests = scale.count(12_000);
    twice(|| pass(requests))
}
