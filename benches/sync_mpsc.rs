#![feature(test)]
extern crate test;
use crate::test::Bencher;

use {
    futures::{
        ready,
        stream::{Stream, StreamExt},
        sink::Sink,
        task::{Context, Poll},
    },
    futures_state_channel::{Sender, Receiver, channel},
    futures_test::task::noop_context,
    std::{pin::Pin, sync::Arc},
};
use futures_executor::block_on;
use futures::SinkExt;

/// Single producer, single consumer
#[bench]
fn unbounded_1_tx(b: &mut Bencher) {
    let mut cx = noop_context();
    b.iter(|| {
        let (mut tx, mut rx) = channel();

        // 1000 iterations to avoid measuring overhead of initialization
        // Result should be divided by 1000
        for i in 0..1000 {

            // Poll, not ready, park
            assert_eq!(Poll::Pending, rx.poll_next_unpin(&mut cx));

           block_on(tx.send(i));

            // Now poll ready
            assert_eq!(Poll::Ready(Some(Arc::new(i))), rx.poll_next_unpin(&mut cx));
        }
    })
}

/// 100 producers, single consumer
#[bench]
fn unbounded_100_tx(b: &mut Bencher) {
    let mut cx = noop_context();
    b.iter(|| {
        let (tx, mut rx) = channel();

        let mut tx: Vec<_> = (0..100).map(|_| tx.clone()).collect();

        // 1000 send/recv operations total, result should be divided by 1000
        for _ in 0..10 {
            for (i, x) in tx.iter_mut().enumerate() {
                assert_eq!(Poll::Pending, rx.poll_next_unpin(&mut cx));

                block_on(x.send(i));

                assert_eq!(Poll::Ready(Some(Arc::new(i))), rx.poll_next_unpin(&mut cx));
            }
        }
    })
}

#[bench]
fn unbounded_uncontended(b: &mut Bencher) {
    let mut cx = noop_context();
    b.iter(|| {
        let (mut tx, mut rx) = channel();

        for i in 0..1000 {
            block_on(tx.send(i));
            // No need to create a task, because poll is not going to park.
            assert_eq!(Poll::Ready(Some(Arc::new(i))), rx.poll_next_unpin(&mut cx));
        }
    })
}


/// A Stream that continuously sends incrementing number of the queue
struct TestSender {
    tx: Sender<u32>,
    last: u32, // Last number sent
}

// Could be a Future, it doesn't matter
impl Stream for TestSender {
    type Item = u32;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>)
        -> Poll<Option<Self::Item>>
    {
        let this = &mut *self;
        let mut tx = Pin::new(&mut this.tx);

        ready!(tx.as_mut().poll_ready(cx)).unwrap();
        tx.as_mut().start_send(this.last + 1).unwrap();
        this.last += 1;
        assert_eq!(Poll::Ready(Ok(())), tx.as_mut().poll_flush(cx));
        Poll::Ready(Some(this.last))
    }
}

/// Single producers, single consumer
#[bench]
fn bounded_1_tx(b: &mut Bencher) {
    let mut cx = noop_context();
    b.iter(|| {
        let (tx, mut rx) = channel();

        let mut tx = TestSender { tx, last: 0 };

        for i in 0..1000 {
            assert_eq!(Poll::Ready(Some(i + 1)), tx.poll_next_unpin(&mut cx));
            assert_eq!(Poll::Ready(Some(Arc::new(i + 1))), rx.poll_next_unpin(&mut cx));
        }
    })
}

