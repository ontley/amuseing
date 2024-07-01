use amuseing::player::{Queue, RepeatMode};
use rand::prelude::SliceRandom;
use rand::SeedableRng;

#[test]
fn test_iteration_single() {
    let items: Vec<u8> = vec![7, 1, 3];
    let mode = RepeatMode::Single;
    let mut q = Queue::new(items, 0, mode);
    assert_eq!(Some(&7), q.next());
    assert_eq!(Some(&7), q.next());
    assert_eq!(Some(&7), q.next());
}

#[test]
fn test_iteration_all() {
    let items: Vec<u8> = vec![7, 1, 3];
    let mode = RepeatMode::All;
    let mut q = Queue::new(items, 0, mode);
    assert_eq!(Some(&7), q.next());
    assert_eq!(Some(&1), q.next());
    assert_eq!(Some(&3), q.next());
    assert_eq!(Some(&7), q.next());
    assert_eq!(Some(&1), q.next());
}

#[test]
fn test_iteration_off() {
    let items: Vec<u8> = vec![7, 1, 3];
    let mode = RepeatMode::Off;
    let mut q = Queue::new(items, 0, mode);
    assert_eq!(Some(&7), q.next());
    assert_eq!(Some(&1), q.next());
    assert_eq!(Some(&3), q.next());
    assert_eq!(None, q.next());
    assert_eq!(None, q.next());
}

#[test]
fn test_changing_mode() {
    let items: Vec<u8> = vec![7, 1, 3, 4];
    let mut q = Queue::new(items, 0, RepeatMode::All);
    assert_eq!(Some(&7), q.next());
    assert_eq!(Some(&1), q.next());

    q.repeat_mode = RepeatMode::Single;
    assert_eq!(Some(&1), q.next());
    assert_eq!(Some(&1), q.next());

    q.repeat_mode = RepeatMode::Off;
    assert_eq!(Some(&3), q.next());
    assert_eq!(Some(&4), q.next());
    assert_eq!(None, q.next());
}

#[test]
fn test_peek() {
    let items: Vec<u8> = vec![7, 1, 3, 4];
    let mut q = Queue::new(items, 2, RepeatMode::All);
    assert_eq!(Some(&3), q.peek());

    q.next();
    assert_eq!(Some(&4), q.peek());
}

#[test]
fn test_skip() {
    let items: Vec<u8> = vec![7, 1, 3, 4];
    let mut q = Queue::new(items.clone(), 0, RepeatMode::All);
    q.skip(2);
    assert_eq!(Some(&3), q.peek());

    let mut q = Queue::new(items.clone(), 0, RepeatMode::All);
    q.next();
    q.skip(2);

    assert_eq!(Some(&4), q.peek());
    let mut q = Queue::new(items.clone(), 0, RepeatMode::All);
    q.next();
    q.skip(1);
    q.skip(1);
    assert_eq!(Some(&4), q.peek());
}

#[test]
fn test_jump() {
    let items: Vec<u8> = vec![7, 1, 3, 4];
    let mut q = Queue::new(items, 0, RepeatMode::All);

    q.next();
    q.jump(2);
    assert_eq!(Some(&3), q.peek());
}

#[test]
fn test_shuffle() {
    let items: Vec<u8> = vec![7, 1, 3, 4];
    let mut q = Queue::new(items, 0, RepeatMode::All);
    q.jump(1);
    let seed = [3; 32];
    let mut rng = rand::rngs::StdRng::from_seed(seed);
    let remaining = &mut [7, 3, 4];
    remaining.shuffle(&mut rng);

    q.shuffle(&mut rng);
    assert_eq!(Some(&1), q.next());
    assert_eq!(Some(&remaining[0]), q.next());
}
