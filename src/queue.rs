use std::fmt::Display;

use crate::errors::OutOfBoundsError;

/// Controls the behaviour of the [`Queue::next_item`] method.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RepeatMode {
    /// The queue does not repeat. Calls to [`next_item`] return None until the RepeatMode is changed.
    ///
    /// [`next_item`]: Queue::next_item
    Off,
    /// The queue returns the same item on repeat, this includes None if the current index is larger than the length of the items.
    Single,
    /// The queue loops back to the start after reaching the end.
    All,
}

impl RepeatMode {
    /// Returns the next repeat mode in the cycle: All => Single => Off => All.
    pub fn next(&self) -> Self {
        match *self {
            Self::All => Self::Single,
            Self::Single => Self::Off,
            Self::Off => Self::All,
        }
    }
}

impl Display for RepeatMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match *self {
            Self::Off => "off",
            Self::All => "all",
            Self::Single => "one",
        })
    }
}

// TODO: turn into builder pattern

/// A queue of items with variable iteration rules, depending on the repeat_mode field.
///
/// Queues like this are commonly found in music players, such as spotify or youtube music.
///
/// See [`next_item`] for an explanation on how the repeat mode changes iteration.
///
/// [`next_item`]: Self::next_item
#[derive(Clone, Debug)]
pub struct Queue<T> {
    items: Vec<T>,
    index: usize,
    /// The repeat mode of the queue.
    ///
    /// The value is used in `next` to determine which item to return.
    pub repeat_mode: RepeatMode,
    /// Used for proper iteration after skipping/jumping, and initial `next` call
    has_advanced: bool,
}

impl<T> Queue<T> {
    /// Create a new queue with the given repeat mode.
    pub fn new(repeat_mode: RepeatMode) -> Self {
        Self {
            items: Vec::new(),
            index: 0,
            repeat_mode,
            has_advanced: false,
        }
    }

    /// Return the next item in the queue, depending on the [`RepeatMode`].
    ///
    /// If the queue is not empty, the first call always gives the first item, regardless of the repeat mode.
    /// Calling `next_item` after [`jump`], [`skip`] or [`rewind`] also guarantees the next item's position, regardless of the repeat mode.
    ///
    /// This means that [`jump`]ing to index 5, and calling [`next_item`] will return the 6th item, or None if that's outside the queue.
    ///
    /// This method updates the index on call.
    /// The index is not updated if the repeat mode is [`Off`] and the method returns None.
    ///
    /// [`jump`]: Self::jump
    /// [`skip`]: Self::skip
    /// [`rewind`]: Self::rewind
    /// [`next_item`]: Self::next_item
    ///
    /// # Iteration rules
    ///
    /// If the repeat mode is [`All`], the queue will wrap around to the beginning when it reached the end.
    /// If it is [`Single`], the same value will be returned every time, even if it is None.
    /// If it is [`Off`], the queue will return None after reaching the end.
    ///
    /// [`All`]: RepeatMode::All
    /// [`Single`]: RepeatMode::Single
    /// [`Off`]: RepeatMode::Off
    pub fn next_item(&mut self) -> Option<&T> {
        if self.items.is_empty() {
            return None;
        }
        if self.repeat_mode != RepeatMode::Single
            && self.has_advanced
            && self.index < self.items.len()
        {
            self.index += 1;
        }
        if self.repeat_mode != RepeatMode::Off {
            self.index %= self.items.len();
        }
        self.has_advanced = true;
        self.items.get(self.index)
    }

    /// Return a slice of the items in the queue.
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Return a mutable slice of the items in the queue.
    pub fn items_mut(&mut self) -> &mut [T] {
        &mut self.items
    }

    /// Return true if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The index of the current (last returned) item. This index can be outside the queue if the last returned item was None.
    pub fn index(&self) -> usize {
        self.index
    }

    /// Push an item onto the internal Vec.
    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }

    /// Clears the internal Vec, and sets the index to 0.
    pub fn clear(&mut self) {
        self.items.clear();
        self.index = 0;
        self.has_advanced = false;
    }

    /// Remove the value at position `index`, calling [`Vec::remove`] internally.
    ///
    /// Rewinds the queue by 1 if the given index is less than the internal one.
    pub fn remove(&mut self, index: usize) {
        self.items.remove(index);
        if index < self.index {
            self.index -= 1;
        }
    }

    /// Inserts an item at position `index`, calling [`Vec::remove`] internally.
    ///
    /// Advanced the queue by 1 if `index` is less or equal to the internal one, for consistent iteration.
    pub fn insert(&mut self, index: usize, item: T) {
        self.items.insert(index, item);
        if index <= self.index {
            self.index += 1;
        }
    }

    /// Jump to index `n` in the queue.
    ///
    /// This method guarantees the next item is at index `n`.
    pub fn jump(&mut self, new_index: usize) -> Result<(), OutOfBoundsError<usize>> {
        if new_index > self.items.len() {
            return Err(OutOfBoundsError::High {
                value: new_index,
                max: self.items.len(),
            });
        }
        self.has_advanced = false;
        self.index = new_index;
        Ok(())
    }

    /// Skip n items forward.
    ///
    /// This method guarantees the next item is `n` ahead.
    ///
    /// If the repeat mode is [`Off`], skipping beyond the end of the queue will set the index to the length of the queue, otherwise wrap around to the beginning.
    ///
    /// [`Off`]: RepeatMode::Off
    pub fn skip(&mut self, mut n: usize) {
        if self.has_advanced {
            n += 1;
        }
        let new_index = if self.items.is_empty() {
            0
        } else if self.repeat_mode == RepeatMode::Off {
            (self.index + n).clamp(0, self.items.len())
        } else {
            (self.index + n) % self.items.len()
        };
        self.jump(new_index)
            .expect("Calculated jump from skip shouldn't fail");
    }

    /// Rewind n items.
    ///
    /// This method guarantees the next item is `n` behind the current item.
    pub fn rewind(&mut self, n: usize) {
        let new_index = if self.items.is_empty() {
            0
        } else if n <= self.index {
            self.index - n
        } else {
            self.items.len() - (n - self.index)
        };
        self.jump(new_index)
            .expect("Calculated jump from rewind shouldn't fail");
    }

    /// Return a reference to the current element (the element that was last returned).
    pub fn current(&self) -> Option<&T> {
        self.items.get(self.index)
    }
}

impl<T> Extend<T> for Queue<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        self.items.extend(iter);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_iteration_all_test() {
        let mut queue: Queue<u32> = Queue::new(RepeatMode::All);
        queue.items = vec![1, 2, 3];
        assert_eq!(queue.next_item(), Some(&1));
        assert_eq!(queue.next_item(), Some(&2));
        assert_eq!(queue.next_item(), Some(&3));
        assert_eq!(queue.next_item(), Some(&1));
        assert_eq!(queue.next_item(), Some(&2));
    }

    #[test]
    fn queue_iteration_off_test() {
        let mut queue: Queue<u32> = Queue::new(RepeatMode::Off);
        queue.items = vec![1, 2, 3];
        assert_eq!(queue.next_item(), Some(&1));
        assert_eq!(queue.next_item(), Some(&2));
        assert_eq!(queue.next_item(), Some(&3));
        assert_eq!(queue.next_item(), None);
        assert_eq!(queue.next_item(), None);
    }

    #[test]
    fn queue_iteration_single_test() {
        let mut queue: Queue<u32> = Queue::new(RepeatMode::Single);
        queue.items = vec![1, 2, 3];
        assert_eq!(queue.next_item(), Some(&1));
        assert_eq!(queue.next_item(), Some(&1));
        assert_eq!(queue.next_item(), Some(&1));
    }

    #[test]
    fn test_skip() {
        let items: Vec<u32> = vec![1, 5, 3, 7, 8, 6, 9, 4];
        let mut queue = Queue::new(RepeatMode::Off);
        queue.items = items;
        queue.skip(2);
        assert_eq!(queue.next_item(), Some(&3));
        queue.skip(1);
        assert_eq!(queue.next_item(), Some(&8));
    }

    #[test]
    fn test_push() {
        let mut queue = Queue::new(RepeatMode::Off);
        queue.items = vec![];
        queue.push(6);
        queue.push(4);
        assert_eq!(&queue.items, &[6, 4]);
    }

    #[test]
    fn test_remove() {
        let mut queue = Queue::new(RepeatMode::Off);
        queue.items = vec![1, 6, 3, 9, 2];
        // It's set to 0 anyway but I like this being explicit
        queue.index = 0;
        queue.remove(3);
        assert_eq!(&queue.items, &[1, 6, 3, 2]);
        queue.index = 2;
        queue.remove(0);
        assert_eq!(&queue.items, &[6, 3, 2]);
        assert_eq!(queue.index, 1);
    }
}
