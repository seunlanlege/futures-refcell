//! Heavily modified from atomic_refcell

#![allow(unsafe_code, dead_code)]
#![deny(missing_docs)]

use crossbeam_queue::SegQueue as Queue;
use futures_task::{Poll, Context};
use futures_util::future::poll_fn;
use futures_core::task::__internal::AtomicWaker;
use std::cell::UnsafeCell;
use std::fmt;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::{
	atomic::{AtomicUsize, AtomicBool, Ordering}, Arc,
};

/// A threadsafe analogue to RefCell. Uses futures for synchronization
/// It allows for 9223372036854775808 concurrent immutable borrows.
/// Only one mutable borrow can exist and without concurrent borrows.
/// Uses an UnsafeCell internally
pub struct RefCell<T> {
	/// borrow tracker
	borrow: AtomicUsize,
	/// flag to prevent new borrows from being created
	blocked_writer: AtomicBool,
	/// reader wakers
	reader_wakers: Arc<Queue<AtomicWaker>>,
	/// writer waker
	writer_wakers: Queue<AtomicWaker>,
	/// inner value.
	value: UnsafeCell<T>,
}

impl<T> RefCell<T> {
	/// Creates a new `AtomicRefCell` containing `value`.
	#[inline]
	pub fn new(value: T) -> RefCell<T> {
		RefCell {
			borrow: AtomicUsize::new(0),
			blocked_writer: AtomicBool::new(false),
			reader_wakers: Arc::new(Queue::new()),
			writer_wakers: Queue::new(),
			value: UnsafeCell::new(value),
		}
	}

	/// Consumes the `AtomicRefCell`, returning the wrapped value.
	#[inline]
	pub fn into_inner(self) -> T {
		debug_assert!(self.borrow.load(Ordering::Acquire) == 0);
		self.value.into_inner()
	}
}

impl<T> RefCell<T> {
	/// Immutably borrows the wrapped value.
	pub async fn borrow(&self) -> AtomicRef<'_, T> {
		poll_fn(|ctx| {
			self.poll_borrow(ctx)
		}).await
	}

	/// Mutably borrows the wrapped value.
	pub async fn borrow_mut(&self) -> AtomicRefMut<'_, T> {
		poll_fn(|ctx| {
			self.poll_borrow_mut(ctx)
		}).await
	}

	/// Immutably borrows the wrapped value.
	#[inline]
	fn poll_borrow(&self, context: &Context<'_>) -> Poll<AtomicRef<T>> {
		// register the waker.
		self.register_waker(context, &self.reader_wakers);

		if self.blocked_writer.load(Ordering::Acquire) {
			return Poll::Pending
		}

		// add to the number of immutable borrows.
		let new = self.borrow.fetch_add(1, Ordering::Acquire) + 1;

		// whoops, there's a mutable borrow in progress.
		if new & HIGH_BIT != 0 {
			return Poll::Pending;
		}

		let borrow = AtomicBorrowRef::new(&self.borrow, &self.writer_wakers);

		return Poll::Ready(AtomicRef {
			value: unsafe { &*self.value.get() },
			borrow
		})
	}

	/// Mutably borrows the wrapped value.
	fn poll_borrow_mut(&self, context: &Context<'_>) -> Poll<AtomicRefMut<T>> {
		// Use compare-and-swap to avoid corrupting the immutable borrow count
		// on illegal mutable borrows.
		if self.borrow.compare_exchange(0, HIGH_BIT, Ordering::Acquire, Ordering::Relaxed).is_ok() {
			// looks like there's still immutable borrows still happening
			// we register interest in it waiting for the data to be available.
			self.register_waker(context, &self.writer_wakers);
			self.blocked_writer.store(true, Ordering::Release);
			return Poll::Pending
		}

		let borrow = AtomicBorrowRefMut::new(&self);
		Poll::Ready(AtomicRefMut {
			value: unsafe { &mut *self.value.get() },
			borrow
		})
	}

	fn register_waker(&self, context: &Context<'_>, queue: &Queue<AtomicWaker>) {
		let waker = AtomicWaker::new();
		waker.register(context.waker());
		queue.push(waker);
	}

	/// Returns a raw pointer to the underlying data in this cell.
	///
	/// External synchronization is needed to avoid data races when dereferencing
	/// the pointer.
	#[inline]
	pub fn as_ptr(&self) -> *mut T {
		self.value.get()
	}

	/// Returns a mutable reference to the wrapped value.
	///
	/// No runtime checks take place (unless debug assertions are enabled)
	/// because this call borrows `AtomicRefCell` mutably at compile-time.
	#[inline]
	pub fn get_mut(&mut self) -> &mut T {
		debug_assert!(self.borrow.load(Ordering::Acquire) == 0);
		unsafe { &mut *self.value.get() }
	}

	/// wakes all readers.
	fn wake_readers(&self) {
		while let Ok(waker) = self.reader_wakers.pop() {
			waker.wake()
		}
	}
}

//
// Core synchronization logic. Keep this section small and easy to audit.
//

const HIGH_BIT: usize = !(std::usize::MAX >> 1);
const MAX_FAILED_BORROWS: usize = HIGH_BIT + (HIGH_BIT >> 1);

struct AtomicBorrowRef<'b> {
	borrow: &'b AtomicUsize,
	writer_waker: &'b Queue<AtomicWaker>,
}

impl<'b> AtomicBorrowRef<'b> {
	#[inline]
	fn new(
		borrow: &'b AtomicUsize,
		writer_waker: &'b Queue<AtomicWaker>
	) -> Self {
		AtomicBorrowRef { borrow, writer_waker }
	}
}

impl<'b> Drop for AtomicBorrowRef<'b> {
	#[inline]
	fn drop(&mut self) {
		let old = self.borrow.fetch_sub(1, Ordering::Release);
		// so this is the last object holding a borrow,
		// if there's a writer_waker in the queue, wake it.
		if old == 0 {
			if let Ok(waker) = self.writer_waker.pop() {
				waker.wake()
			}
		}
	}
}

struct AtomicBorrowRefMut<'b, T> {
	cell: &'b RefCell<T>,
}

impl<'b, T> Drop for AtomicBorrowRefMut<'b, T> {
	#[inline]
	fn drop(&mut self) {
		self.cell.borrow.store(0, Ordering::Release);
		if let Ok(waker) = self.cell.writer_wakers.pop() {
			waker.wake()
		} else {
			self.cell.blocked_writer.store(false, Ordering::Release);
			// we're done mutating the value, wake up all readers.
			self.cell.wake_readers()
		}
	}
}

impl<'b, T> AtomicBorrowRefMut<'b, T> {
	#[inline]
	fn new(cell: &'b RefCell<T>) -> Self {
		Self { cell }
	}
}

unsafe impl<T: Send + Sync> Send for RefCell<T> {}

unsafe impl<T: Send + Sync> Sync for RefCell<T> {}

//
// End of core synchronization logic. No tricky thread stuff allowed below
// this point.
//

impl<T: Default> Default for RefCell<T> {
	#[inline]
	fn default() -> RefCell<T> {
		RefCell::new(Default::default())
	}
}


impl<T> From<T> for RefCell<T> {
	fn from(t: T) -> RefCell<T> {
		RefCell::new(t)
	}
}

/// A wrapper type for an immutably borrowed value from an `AtomicRefCell<T>`.
pub struct AtomicRef<'b, T> {
	value: &'b T,
	borrow: AtomicBorrowRef<'b>,
}


impl<'b, T> Deref for AtomicRef<'b, T> {
	type Target = T;

	#[inline]
	fn deref(&self) -> &T {
		self.value
	}
}

/// A wrapper type for a mutably borrowed value from an `AtomicRefCell<T>`.
pub struct AtomicRefMut<'b, T> {
	value: &'b mut T,
	borrow: AtomicBorrowRefMut<'b, T>,
}

impl<'b, T> Deref for AtomicRefMut<'b, T> {
	type Target = T;

	#[inline]
	fn deref(&self) -> &T {
		self.value
	}
}

impl<'b, T> DerefMut for AtomicRefMut<'b, T> {
	#[inline]
	fn deref_mut(&mut self) -> &mut T {
		self.value
	}
}

impl<'b, T: Debug + 'b> Debug for AtomicRef<'b, T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		self.value.fmt(f)
	}
}

impl<'b, T: Debug + 'b> Debug for AtomicRefMut<'b, T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		self.value.fmt(f)
	}
}

impl<T: Debug> Debug for RefCell<T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "AtomicRefCell {{ ... }}")
	}
}
