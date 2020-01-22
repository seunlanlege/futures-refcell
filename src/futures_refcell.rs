//! Heavily modified from atomic_refcell

#![allow(unsafe_code, dead_code)]
#![deny(missing_docs)]

use crossbeam_queue::SegQueue as Queue;
use futures_task::{Poll, Context};
use futures_core::task::__internal::AtomicWaker;
use std::cell::UnsafeCell;
use std::fmt;
use std::fmt::Debug;
use std::ops::{Deref, DerefMut};
use std::sync::{atomic, Arc};
use std::sync::atomic::{AtomicUsize, AtomicBool};
use std::result::Result::Ok;

/// A threadsafe analogue to RefCell. Uses futures for synchronization
/// It allows for 9223372036854775808 concurrent immutable borrows.
/// Only one mutable borrow can exist and without concurrent borrows.
/// Uses an UnsafeCell internally
pub struct RefCell<T: ?Sized> {
	/// borrow tracker
	borrow: AtomicUsize,
	/// flag to prevent new borrows from being created
	blocked_writer: AtomicBool,
	/// reader wakers
	reader_waker: Arc<Queue<AtomicWaker>>,
	/// writer waker
	writer_waker: Queue<AtomicWaker>,
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
			reader_waker: Arc::new(Queue::new()),
			writer_waker: Queue::new(),
			value: UnsafeCell::new(value),
		}
	}

	/// Consumes the `AtomicRefCell`, returning the wrapped value.
	#[inline]
	pub fn into_inner(self) -> T {
		debug_assert!(self.borrow.load(atomic::Ordering::Acquire) == 0);
		self.value.into_inner()
	}
}

impl<T: ?Sized> RefCell<T> {
	/// Immutably borrows the wrapped value.
	#[inline]
	pub fn borrow(&self, context: &Context<'_>) -> Poll<AtomicRef<T>> {
		// register the waker.
		let waker = AtomicWaker::new();
		waker.register(context.waker());
		self.reader_waker.push(waker);

		// check if there's no writer waiting for immutable borrows to drop to 0.
		if !self.blocked_writer.load(atomic::Ordering::Acquire) {
			if let Some(borrow) = AtomicBorrowRef::new(&self.borrow, &self.writer_waker) {
				return Poll::Ready(AtomicRef {
					value: unsafe { &*self.value.get() },
					borrow,
				})
			}
		}

		Poll::Pending
	}

	/// registers interest in borrowing the value mutably.
	/// this prevents new borrows from happening. if this returns Poll::Ready(Ok(()))
	/// the value is now available to be borrowed mutably. call `borrow_mut` next.
	pub fn can_borrow_mut(&self, context: &Context<'_>) -> Poll<Result<(), ()>> {
		// Use compare-and-swap to avoid corrupting the immutable borrow count
		// on illegal mutable borrows.
		let old = match self.borrow.compare_exchange(0, HIGH_BIT, atomic::Ordering::Acquire, atomic::Ordering::Relaxed) {
			Ok(x) => x,
			Err(x) => x,
		};

		// looks like there's still immutable borrows still happening
		// we register interest in it waiting for the data to be available.
		if old != 0 {
			let waker = AtomicWaker::new();
			waker.register(context.waker());
			self.writer_waker.push(waker);
			self.blocked_writer.store(true, atomic::Ordering::Release);
			return Poll::Pending
		}

		Poll::Ready(Ok(()))
	}

	/// Mutably borrows the wrapped value, call `can_borrow_mut` first
	#[inline]
	pub fn borrow_mut(&self) -> AtomicRefMut<T> {
		self.blocked_writer.store(false, atomic::Ordering::Release);
		let borrow = AtomicBorrowRefMut::new(&self.borrow, &self.reader_waker);
		AtomicRefMut {
			value: unsafe { &mut *self.value.get() },
			borrow
		}
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
		debug_assert!(self.borrow.load(atomic::Ordering::Acquire) == 0);
		unsafe { &mut *self.value.get() }
	}

	/// wakes all readers.
	pub fn wake_readers(&self) {
		while let Ok(waker) = self.reader_waker.pop() {
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
	) -> Option<Self> {
		// add to the number of immutable borrows.
		let new = borrow.fetch_add(1, atomic::Ordering::Acquire) + 1;

		// whoops, there's a mutable borrow in progress.
		if new & HIGH_BIT != 0 {
			return None;
		}

		Some(AtomicBorrowRef { borrow, writer_waker })
	}
}

impl<'b> Drop for AtomicBorrowRef<'b> {
	#[inline]
	fn drop(&mut self) {
		let old = self.borrow.fetch_sub(1, atomic::Ordering::Release);
		// so this is the last object holding a borrow,
		// if there's a writer_waker in the queue, wake it.
		if old == 0 {
			if let Ok(waker) = self.writer_waker.pop() {
				waker.wake()
			}
		}
	}
}

struct AtomicBorrowRefMut<'b> {
	read_wakers: &'b Queue<AtomicWaker>,
	borrow: &'b AtomicUsize,
}

impl<'b> Drop for AtomicBorrowRefMut<'b> {
	#[inline]
	fn drop(&mut self) {
		self.borrow.store(0, atomic::Ordering::Release);
		// we're done mutating the value, wake up all readers.
		while let Ok(waker) = self.read_wakers.pop() {
			waker.wake()
		}
	}
}

impl<'b> AtomicBorrowRefMut<'b> {
	#[inline]
	fn new(
		borrow: &'b AtomicUsize,
		read_wakers: &'b Queue<AtomicWaker>,
	) -> Self {
		AtomicBorrowRefMut { read_wakers, borrow }
	}
}

unsafe impl<T: ?Sized + Send + Sync> Send for RefCell<T> {}

unsafe impl<T: ?Sized + Send + Sync> Sync for RefCell<T> {}

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
pub struct AtomicRef<'b, T: ?Sized + 'b> {
	value: &'b T,
	borrow: AtomicBorrowRef<'b>,
}


impl<'b, T: ?Sized> Deref for AtomicRef<'b, T> {
	type Target = T;

	#[inline]
	fn deref(&self) -> &T {
		self.value
	}
}

/// A wrapper type for a mutably borrowed value from an `AtomicRefCell<T>`.
pub struct AtomicRefMut<'b, T: ?Sized + 'b> {
	value: &'b mut T,
	borrow: AtomicBorrowRefMut<'b>,
}

impl<'b, T: ?Sized> Deref for AtomicRefMut<'b, T> {
	type Target = T;

	#[inline]
	fn deref(&self) -> &T {
		self.value
	}
}

impl<'b, T: ?Sized> DerefMut for AtomicRefMut<'b, T> {
	#[inline]
	fn deref_mut(&mut self) -> &mut T {
		self.value
	}
}

impl<'b, T: ?Sized + Debug + 'b> Debug for AtomicRef<'b, T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		self.value.fmt(f)
	}
}

impl<'b, T: ?Sized + Debug + 'b> Debug for AtomicRefMut<'b, T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		self.value.fmt(f)
	}
}

impl<T: ?Sized + Debug> Debug for RefCell<T> {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		write!(f, "AtomicRefCell {{ ... }}")
	}
}
