//! Shared memory synchronized using futures.
//!
//! The "channel" holds memory of the last sent item and makes it available to new consumers.
//! Its basically shared state that uses futures.
use futures_task::{Context, Poll};
use futures_core::Stream;
use futures_sink::Sink;
use std::{pin::Pin, sync::Arc};
use std::sync::atomic::{Ordering, AtomicBool};

mod futures_refcell;
use self::futures_refcell::RefCell;

// shared state held by the producer and consumers.
struct StateInner<T> {
	// we use Option<T> as a way to signal
	// new consumers to wait for data to be available.
	state: RefCell<Option<Arc<T>>>,
	// flag for closing the stream
	sender_closed: AtomicBool,
}

/// The consuming end of the channel, it can be cloned across as many threads/tasks
/// as required.
pub struct Receiver<T> {
	inner: Arc<StateInner<T>>,
	last_value: Option<Arc<T>>,
}

/// The producing end of the channel, can be cloned, but should only be called from
/// one thread at any time.
pub struct Sender<T> {
	inner: Arc<StateInner<T>>
}

impl<T: Unpin> Stream for Receiver<T> {
	type Item = Arc<T>;

	fn poll_next(
		self: Pin<&mut Self>,
		ctx: &mut Context<'_>,
	) -> Poll<Option<Self::Item>> {
		if self.inner.sender_closed.load(Ordering::SeqCst) {
			return Poll::Ready(None);
		}

		let this = std::pin::Pin::into_inner(self);
		let value = match this.inner.state.borrow(ctx) {
			Poll::Ready(data) => data,
			_ => return Poll::Pending
		};

		let data = match (value.as_ref(), this.last_value.as_ref()) {
			// the executor calls poll_next just after a previous call that succeeds
			// we would like to not return the same value we returned last time
			(Some(msg), last) => {
				if let Some(last) = last {
					if Arc::ptr_eq(msg, last) {
						return Poll::Pending;
					}
				}

				this.last_value = Some(Arc::clone(msg));
				Arc::clone(msg)
			}
			_ => {
				return Poll::Pending;
			}
		};

		Poll::Ready(Some(data))
	}
}

impl<T> Sink<T> for Sender<T> {
	type Error = ();

	fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.state.can_borrow_mut(cx)
	}

	fn start_send(mut self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
		*self.inner.state.borrow_mut() = Some(Arc::new(item));
		Ok(())
	}

	fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.sender_closed.store(true, Ordering::SeqCst);
		self.inner.state.wake_readers();
		Poll::Ready(Ok(()))
	}
}

impl<T> Clone for Receiver<T> {
	fn clone(&self) -> Self {
		Receiver {
			inner: self.inner.clone(),
			last_value: None,
		}
	}
}

impl<T> Clone for Sender<T> {
	fn clone(&self) -> Self {
		Sender {
			inner: self.inner.clone(),
		}
	}
}

/// creates a state channel
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
	let inner = Arc::new(StateInner {
		state: RefCell::new(None),
		sender_closed: AtomicBool::new(false),
	});

	let sender = Sender {
		inner: inner.clone()
	};

	let receiver = Receiver {
		inner,
		last_value: None,
	};

	(sender, receiver)
}

#[cfg(test)]
mod test {
	use super::*;
	use futures_executor::block_on;
	use futures::{
		stream::FuturesUnordered,
		StreamExt,
		SinkExt,
	};

	#[test]
	fn test_channel() {
		let (mut s, r) = channel();

		let handle = std::thread::spawn(move || {
			let futures = FuturesUnordered::new();
			for i in 0..10 {
				let mut rec = r.clone();
				futures.push(
					rec.for_each(move |item| {
						futures::future::ready(())
					})
				)
			}

			block_on(futures.for_each(|val| {
				futures::future::ready(())
			}));
		});


		block_on(async {
			for value in 10..=15 {
				s.send(value).await;
				std::thread::sleep(std::time::Duration::from_secs(1));
			}
			s.close().await;
		});

		handle.join();
	}
}
