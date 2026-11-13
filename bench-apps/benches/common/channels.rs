//! A graph of tokio tasks that pass boxed messages through channels.
//!
//! Sources create messages and send them to mappers, which build a new message from each and send
//! it into one shared channel. A router reads that channel and deals the messages out to sinks. The
//! runtime moves tasks between its worker threads, so most messages are freed on a different thread
//! than the one that allocated them, as they are in a pipeline of async services.

use std::time::Instant;

use tokio::{sync::mpsc, task::JoinSet};

use super::{Pass, Scale, text::Rng, twice};

const WORKERS: usize = 4;
const SOURCES: usize = 4;
const SINKS: usize = 4;
const CAPACITY: usize = 256;

struct Message {
    id: u64,
    tag: String,
    payload: Vec<u8>,
}

/// A message whose payload size is skewed towards small.
fn message(rng: &mut Rng, id: u64) -> Box<Message> {
    let size = *rng.pick(&[16, 16, 32, 48, 64, 128, 256, 512, 1500]);
    let payload = (0..size).map(|i| (id as usize + i) as u8).collect();
    Box::new(Message { id, tag: format!("src-{}", id % 97), payload })
}

async fn source(index: usize, count: usize, out: mpsc::Sender<Box<Message>>) {
    let mut rng = Rng::new(0x4348_414e + index as u64);
    for sequence in 0..count {
        let id = (index * count + sequence) as u64;
        if out.send(message(&mut rng, id)).await.is_err() {
            return;
        }
    }
}

async fn mapper(mut input: mpsc::Receiver<Box<Message>>, out: mpsc::Sender<Box<Message>>) {
    while let Some(old) = input.recv().await {
        let new = Box::new(Message {
            id: old.id,
            tag: format!("{}-mapped", old.tag),
            payload: old.payload.iter().map(|byte| byte.wrapping_add(1)).collect(),
        });
        if out.send(new).await.is_err() {
            return;
        }
    }
}

async fn router(mut input: mpsc::Receiver<Box<Message>>, sinks: Vec<mpsc::Sender<Box<Message>>>) {
    while let Some(message) = input.recv().await {
        let at = message.id as usize % sinks.len();
        if sinks[at].send(message).await.is_err() {
            return;
        }
    }
}

/// Counts what arrives and adds up a figure that depends on the contents.
async fn sink(mut input: mpsc::Receiver<Box<Message>>) -> (usize, u64) {
    let (mut count, mut sum) = (0, 0);
    while let Some(message) = input.recv().await {
        count += 1;
        sum += u64::from(message.payload[0]) + message.tag.len() as u64;
    }
    (count, sum)
}

fn pass(per_source: usize) -> Pass {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKERS)
        .build()
        .expect("a runtime");
    let pass = runtime.block_on(async {
        let start = Instant::now();
        let (merged_tx, merged_rx) = mpsc::channel(CAPACITY);
        let mut tasks = JoinSet::new();
        for index in 0..SOURCES {
            let (tx, rx) = mpsc::channel(CAPACITY);
            tasks.spawn(source(index, per_source, tx));
            tasks.spawn(mapper(rx, merged_tx.clone()));
        }
        // The router sees the end of the input when the last mapper drops its sender.
        drop(merged_tx);

        let (mut senders, mut sinks) = (Vec::new(), Vec::new());
        for _ in 0..SINKS {
            let (tx, rx) = mpsc::channel(CAPACITY);
            senders.push(tx);
            sinks.push(tokio::spawn(sink(rx)));
        }
        tasks.spawn(router(merged_rx, senders));

        let mut delivered = 0;
        for sink in sinks {
            delivered += sink.await.expect("the sink ends").0;
        }
        assert_eq!(delivered, per_source * SOURCES);
        while tasks.join_next().await.is_some() {}
        Pass::since(start, delivered as f64 / 1e6)
    });
    drop(runtime);
    pass
}

/// One unit is a million messages that reach a sink.
pub(crate) fn run(scale: Scale) -> [Pass; 2] {
    let per_source = scale.count(500_000);
    twice(|| pass(per_source))
}
