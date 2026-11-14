//! Many keep-alive connections exercise a server across all available workers.
//! Connection setup stays outside the clock; latency covers JSON construction through decoding.

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
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Barrier, watch},
    task::JoinSet,
    time::{sleep, timeout},
};

const CONNECTIONS: usize = 256;

/// How long a pass may take before it counts as hung. A client that fails before the rendezvous
/// would otherwise leave every other task waiting at the barrier for good.
const PASS_LIMIT: Duration = Duration::from_secs(300);

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
                // A client that gave up on a connection still queued makes accept fail with
                // ECONNABORTED. The client retries, so the server keeps listening.
                let Ok((stream, _)) = accepted else { continue };
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
    let lines = (0..rng.between(4, 64))
        .map(|_| Line {
            name: rng.pick(WORDS).repeat(8),
            quantity: rng.between(1, 9) as u32,
            cents: rng.between(50, 20_000) as u32,
        })
        .collect();
    Order { id, customer: format!("{} {}", rng.pick(WORDS), rng.pick(WORDS)), lines }
}

/// One keep-alive connection that sends `requests` orders and checks every receipt.
async fn client_task(
    address: SocketAddr,
    index: usize,
    requests: usize,
    ready: Arc<Barrier>,
) -> Vec<u64> {
    let stream = connect(address).await;
    stream.set_nodelay(true).expect("nodelay");
    let (mut sender, connection) =
        client::handshake(TokioIo::new(stream)).await.expect("handshake");
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    let mut samples = Vec::with_capacity(requests);
    ready.wait().await;
    // The second rendezvous lets the coordinator start the clock before requests begin.
    ready.wait().await;
    let mut rng = Rng::new(0x4854_5450 + index as u64);
    for sequence in 0..requests {
        let start = Instant::now();
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
        samples.push(start.elapsed().as_micros() as u64);
    }
    // Dropping the sender closes the connection, which lets the server's task and ours end.
    drop(sender);
    driver.await.expect("the driver ends");
    samples
}

/// Connects to the server, retrying for a while: 256 connections at once overflow a listen backlog
/// of 128, and a machine that has just run many passes may be short of ephemeral ports.
async fn connect(address: SocketAddr) -> TcpStream {
    let mut attempts = 0;
    loop {
        match TcpStream::connect(address).await {
            Ok(stream) => return stream,
            Err(_) if attempts < 100 => {
                attempts += 1;
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("cannot connect after {attempts} retries: {error}"),
        }
    }
}

fn pass(requests: usize) -> Pass {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map_or(1, usize::from).min(16))
        .enable_io()
        .enable_time()
        .build()
        .expect("a runtime");
    let pass = runtime.block_on(async { timeout(PASS_LIMIT, run_pass(requests)).await });
    drop(runtime);
    pass.expect("a pass of http many finishes in time")
}

/// One pass: the server, the clients and the clock.
async fn run_pass(requests: usize) -> Pass {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("an address");
    let (stop, stopped) = watch::channel(false);
    let server = tokio::spawn(serve(listener, stopped));

    let ready = Arc::new(Barrier::new(CONNECTIONS + 1));
    let clients: Vec<_> = (0..CONNECTIONS)
        .map(|index| tokio::spawn(client_task(address, index, requests, ready.clone())))
        .collect();
    ready.wait().await;
    let start = Instant::now();
    ready.wait().await;
    let mut samples = Vec::with_capacity(requests * CONNECTIONS);
    for client in clients {
        samples.extend(client.await.expect("the client ends"));
    }
    let mut pass = Pass::since(start, (requests * CONNECTIONS) as f64);
    samples.sort_unstable();
    pass.p99_us = Some(samples[(samples.len() * 99).div_ceil(100) - 1] as f64);

    stop.send(true).expect("the server listens");
    server.await.expect("the server ends");
    pass
}

/// One unit is one completed request.
pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let requests = scale.count(256);
    twice(|| pass(requests))
}
