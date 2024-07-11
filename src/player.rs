use anyhow::{anyhow, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Sample, SampleFormat, SizedSample, StreamConfig,
};
use rand::seq::SliceRandom;
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};
use std::{
    collections::VecDeque,
    fmt::Display,
    fs,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex, RwLock},
    thread,
    time::Duration,
};
use symphonia::core::{
    audio::{AudioBuffer, Signal},
    codecs::{CodecParameters, Decoder},
    formats::{FormatOptions, FormatReader},
    io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions},
    meta::MetadataOptions,
    probe::Hint,
    units,
};
use thiserror::Error;

type SampleType = i16;

/// Represents a mode of operation for `Queue`s.
///
/// Iteration on a `Queue` is done with `Queue::next()`!
#[derive(PartialEq, Eq)]
pub enum RepeatMode {
    /// Iterate over the elements continously, looping back to the first one when the Queue reaches the end.
    All,
    /// Repeat the current element continously.
    Single,
    /// Iterate like a Vec, returning None after reaching the end.
    /// The `Queue` can still return elements after the end by skipping, jumping or changing the repeat mode.
    Off,
}

impl RepeatMode {
    pub fn next(&self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::Single,
            Self::Single => Self::Off,
        }
    }
}

// A new queue is created everytime a different set of songs is loaded for the player,
// and the queue does not change if the playlist it was created from updates
// because sharing that update across threads would be a headache

/// A queue, used for "iterating" over items, with different modes of iteration (see `RepeatMode` for modes).
/// The queue behaves like a spotify queue does.
///
/// Iteration of the queue is done via the `next` method because the Queue is designed to be mutated while "iterating".
/// This allows for changing the queue's repeat mode, which might change what the next item is.
pub struct Queue<T> {
    pub items: Vec<T>,
    index: usize,
    pub repeat_mode: RepeatMode,
    /// Used for proper iteration with `RepeatMode::All and Off`, because  without it, the first element given by `next` would be off by one place.
    /// This is set to false on creation, or after any operation that changes the index such as `skip`.
    has_advanced: bool,
}

impl<T> Default for Queue<T> {
    fn default() -> Self {
        Self::new(Vec::new(), 0, RepeatMode::Off)
    }
}

impl<T> Queue<T> {
    pub fn new(items: Vec<T>, start_index: usize, repeat_mode: RepeatMode) -> Self {
        Self {
            items,
            index: start_index,
            repeat_mode,
            has_advanced: false,
        }
    }

    /// Return a reference to the next element in the queue, and advance it.
    /// The element given after `skip`, `jump` or after Queue creation is always the same, regardless of mode.
    ///
    /// This is not an implementation of Iterator.
    pub fn next(&mut self) -> Option<&T> {
        if self.repeat_mode == RepeatMode::Single {
            return Some(&self.items[self.index]);
        }
        if self.has_advanced {
            self.index += 1;
        }
        if self.index >= self.items.len() {
            if self.repeat_mode == RepeatMode::Off {
                return None;
            } else if self.repeat_mode == RepeatMode::All {
                self.index %= self.items.len();
            }
        }
        self.has_advanced = true;
        Some(&self.items[self.index])
    }

    /// Return a mutable reference to the next element in the queue, and advance it.
    /// The element given after `skip`, `jump` or after Queue creation is always the same, regardless of mode.
    ///
    /// This is not an implementation of Iterator.
    pub fn next_mut(&mut self) -> Option<&mut T> {
        if self.repeat_mode == RepeatMode::Single {
            return Some(&mut self.items[self.index]);
        }
        if self.has_advanced {
            self.index += 1;
        }
        if self.index >= self.items.len() {
            if self.repeat_mode == RepeatMode::Off {
                return None;
            } else if self.repeat_mode == RepeatMode::All {
                self.index %= self.items.len();
            }
        }
        self.has_advanced = true;
        Some(&mut self.items[self.index])
    }

    /// Return a reference to what the next item **could be**, if the queue isn't changed until then.
    /// This method does not advance the queue, so repeated calls without changing the repeat mode will give the same item.
    pub fn peek(&self) -> Option<&T> {
        if self.repeat_mode == RepeatMode::Single {
            return Some(&self.items[self.index]);
        }
        let mut i = self.index;
        if self.has_advanced {
            i += 1;
        }
        if self.index >= self.items.len() {
            if self.repeat_mode == RepeatMode::Off {
                return None;
            } else if self.repeat_mode == RepeatMode::All {
                i %= self.items.len();
            }
        }
        Some(&self.items[i])
    }

    /// Return a mutable reference to what the next item *could* be, if the repeat mode doesn't change until then.
    /// This method does not advance the queue, so repeated calls without changing the repeat mode will give the same item.
    pub fn peek_mut(&mut self) -> Option<&mut T> {
        if self.repeat_mode == RepeatMode::Single {
            return Some(&mut self.items[self.index]);
        }
        let mut i = self.index;
        if self.has_advanced {
            i += 1;
        }
        if self.index >= self.items.len() {
            if self.repeat_mode == RepeatMode::Off {
                return None;
            } else if self.repeat_mode == RepeatMode::All {
                i %= self.items.len();
            }
        }
        Some(&mut self.items[i])
    }

    /// Return a refence to the index of the item that was returned previously.
    pub fn index(&self) -> &usize {
        &self.index
    }

    /// Return a mutable reference to the index.
    /// NOTE: If you want iteration to work as expected, use `jump`.
    pub fn index_mut(&mut self) -> &mut usize {
        &mut self.index
    }

    /// Shuffle the queue in place.
    ///
    /// NOTE: this moves the previously returned element to the front.
    // this is not an implementation of SliceRandom because I can't be arsed.
    pub fn shuffle(&mut self, rng: &mut impl rand::Rng) {
        self.items.swap(0, self.index);
        let slice = &mut self.items[1..];
        slice.shuffle(rng);
    }

    /// Skip n items.
    ///
    /// The queue guarantees the next item will be `n` away from the one returned previously, regardless of repeat mode.
    /// If `next` wasn't called, this is equivalent to `jump`.
    /// Skipping multiple times in a row is equivalent to skipping the sum.
    pub fn skip(&mut self, mut n: usize) {
        if self.has_advanced {
            n += 1;
        }
        self.jump(self.index + n);
    }

    /// Jump to the `n`th item.
    ///
    /// The queue guarantees the next item will be at index `n`, regardless of repeat mode.
    pub fn jump(&mut self, n: usize) {
        self.index = if self.items.is_empty() {
            0
        } else {
            n % self.items.len()
        };
        self.has_advanced = false;
    }
}

/// A representation of a song that can be played from `Player`.
#[derive(Clone, Debug)]
pub struct Song {
    id: u32,
    title: String,
    path: PathBuf,
    params: CodecParameters,
}

impl Song {
    pub fn new(id: u32, name: String, path: impl Into<PathBuf>, params: CodecParameters) -> Self {
        Self {
            id,
            title: name,
            path: path.into(),
            params,
        }
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub fn duration(&self) -> Duration {
        let time_base = self.params.time_base.unwrap();
        let n_frames = self.params.n_frames.unwrap();
        time_base.calc_time(n_frames).into()
    }

    fn get_decoder(&self) -> Box<dyn Decoder> {
        // TODO: this should return a result instead of unwrapping
        let codec_registry = symphonia::default::get_codecs();
        codec_registry
            .make(&self.params, &Default::default())
            .unwrap()
    }

    fn time_base(&self) -> units::TimeBase {
        // TODO: maybe check if the timebase is Some when creating the song
        self.params
            .time_base
            .expect("Every song should have a timebase")
    }
}

impl Display for Song {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Song({}, {:?})", self.title, self.path)
    }
}

#[derive(Debug, Error)]
#[error("Can not create a playlist with a path that does not exist")]
pub struct InvalidPath(PathBuf);

pub struct Playlist {
    name: String,
    path: PathBuf,
}

impl Playlist {
    pub fn new(name: String, path: PathBuf) -> Result<Self, InvalidPath> {
        // TODO: Conver to Result if the path doesn't exist
        if !path.exists() {
            return Err(InvalidPath(path));
        }
        Ok(Self { name, path })
    }

    pub fn songs(&self) -> Vec<Song> {
        let paths = fs::read_dir(&self.path).expect("Playlist path invalid");
        let songs = paths
            .filter_map(|entry| {
                let entry = entry.ok()?;
                if entry.path().extension()? != "mp3" {
                    return None;
                }
                let Ok(song_file) = fs::File::open(entry.path()) else {
                    return None;
                };
                let mss = MediaSourceStream::new(Box::new(song_file), Default::default());
                let format_reader = get_format_reader(mss);
                let track = format_reader.default_track().unwrap();
                let params = &track.codec_params;

                // Figure out a way to collect title from metadata
                // Metadata appears to be empty for test tracks
                let song_title = entry
                    .file_name()
                    .into_string()
                    .expect("Could not convert filename to string");
                Some(Song::new(
                    track.id,
                    song_title,
                    entry.path(),
                    params.clone(),
                ))
            })
            .collect();
        songs
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn play_from_index(&self, player: &mut Player, index: usize) {
        let queue = Queue::new(self.songs(), index, RepeatMode::Off);
        player.run_with_queue(queue);
    }
}

/// Sent to the thread playing the audio from `Player`.
///
/// Use with `Player::send_message`, or with wrappers like `Player::stop`.
pub enum PlayerMessage {
    /// Stop is used anywhere where we need to skip the current song.
    ///
    /// In most cases, using `Player::skip` or `Player::jump` is preffered.
    Stop,
    /// Pause the playback until we send `Resume`, does nothing if already paused.
    Pause,
    /// Resume the player if Pause was sent previously, does nothing if already playing.
    Resume,
    /// Seek to this duration and recreate the decoder
    Seek(Duration),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlayerState {
    /// Indicates the `Player`s run method hasn't been called.
    NotStarted,
    /// Indicates the `Player` thread has finished.
    Finished,
    Paused,
    Playing,
}

#[derive(Debug, Error)]
pub enum SeekError {
    #[error("Invalid seek Duration (expected maximum {max:?}, got {to:?})")]
    OutOfRange { to: Duration, max: Duration },
    // Returned when the Player tries to seek but there is no song playing;
    #[error("The player does not have a song which can be skipped")]
    NoCurrentSong,
}

#[derive(Debug, Error)]
#[error("The player is already running")]
pub struct PlayerRunningError;

pub struct Player {
    queue: Arc<Mutex<Queue<Song>>>,
    /// None if the player hasn't started yet.
    sender: Option<mpsc::Sender<PlayerMessage>>,
    state: Arc<RwLock<PlayerState>>,
    /// non-zero if the player is playing, this isn't an indicator of state
    time_playing: Arc<RwLock<Duration>>,
    // TODO: Change this to be a &Song
    current: Arc<RwLock<Option<Song>>>,
    volume: Arc<RwLock<f32>>,
}

impl Player {
    pub fn new() -> Self {
        let queue = Queue::new(vec![], 0, RepeatMode::Off);
        Self::with_queue(queue)
    }

    pub fn with_queue(queue: Queue<Song>) -> Self {
        Self {
            queue: Arc::new(Mutex::new(queue)),
            sender: None,
            state: Arc::new(RwLock::new(PlayerState::NotStarted)),
            time_playing: Arc::new(RwLock::new(Default::default())),
            current: Arc::new(RwLock::new(None)),
            volume: Arc::new(RwLock::new(1.)),
        }
    }

    /// Return a reference to the Player's queue
    pub fn queue(&self) -> &Mutex<Queue<Song>> {
        self.queue.as_ref()
    }

    /// Returns at what point the player is in the Song
    ///
    /// NOTE: This is not the duration of the song, use `current` and `Song::duration` for  that
    pub fn duration(&self) -> Duration {
        *self.time_playing.read().unwrap()
    }

    /// Return a cloned version of the song that is currently playing.
    ///
    /// NOTE: This returns Some(Song) even when paused
    pub fn current(&self) -> Option<Song> {
        // TODO: Make this return a reference to the Song if possible
        self.current.read().unwrap().clone()
    }

    /// Send a message to the thread playing the audio
    /// See `PlayerMessage` for more info
    /// Returns a bool to signal message success
    /// `true` means the message was sent successfully
    pub fn send_message(&self, message: PlayerMessage) -> bool {
        let Some(sender) = &self.sender else {
            return false;
        };
        sender.send(message).is_ok()
    }

    /// Send a signal to stop playing, which effectively skips the current song
    ///
    /// This method is a wrapper around `Player::send_message()`, so it returns a bool to signal message success
    pub fn stop(&self) -> bool {
        self.send_message(PlayerMessage::Stop)
    }

    /// Pause playback, does nothing if already paused
    ///
    /// This method is a wrapper around `Player::send_message()`, so it returns a bool to signal message success
    pub fn pause(&self) -> bool {
        self.send_message(PlayerMessage::Pause)
    }

    /// Resume playback, does nothing if already resumed
    ///
    /// This method is a wrapper around `Player::send_message()`, so it returns a bool to signal message success
    pub fn resume(&self) -> bool {
        self.send_message(PlayerMessage::Resume)
    }

    /// Skips in the Player's `Queue` and stops playback of the current song
    ///
    /// Returns `true` on successful message
    pub fn skip(&mut self, n: usize) -> bool {
        let mut queue_lock = self.queue.lock().unwrap();
        queue_lock.skip(n);
        self.stop()
    }

    /// Rewing to the previous song
    ///
    /// Returns `true` on successful message
    pub fn rewind(&mut self) -> bool {
        let mut queue_lock = self.queue.lock().unwrap();
        // TODO: braing isn't braining idk if there's a better way of getting pos
        let mut pos = queue_lock.index;
        if self.duration().as_secs() <= 3 {
            if pos == 0 {
                pos = queue_lock.items.len() - 1;
            } else {
                pos -= 1;
            }
        }
        queue_lock.jump(pos as usize);
        self.stop()
    }

    /// Same as `stop`
    ///
    /// Returns `true` on successful message
    pub fn fast_forward(&mut self) -> bool {
        self.stop()
    }

    /// Jumps in the Player's `Queue` and stops playback of the current song
    ///
    /// Returns `true` on successfull message
    pub fn jump(&mut self, n: usize) -> bool {
        let mut queue_lock = self.queue.lock().unwrap();
        queue_lock.jump(n);
        self.stop()
    }

    pub fn state(&self) -> PlayerState {
        *self.state.read().unwrap()
    }

    pub fn is_playing(&self) -> bool {
        matches!(self.state(), PlayerState::Playing)
    }

    pub fn is_paused(&self) -> bool {
        matches!(self.state(), PlayerState::Paused)
    }

    pub fn is_running(&self) -> bool {
        !matches!(
            self.state(),
            PlayerState::NotStarted | PlayerState::Finished
        )
    }

    /// Clamp the value to 0..1 and set the volume
    pub fn set_volume(&mut self, volume: &f32) {
        let mut volume_lock = self.volume.write().unwrap();
        const B: f32 = 6.9;
        *volume_lock = if volume <= &0. {
            0.
        } else if volume >= &1. {
            1.
        } else {
            ((volume * B).exp() - 1.) / (B.exp() - 1.)
        };
    }

    /// Convenience function for changing the queue and starting the player immediately
    pub fn run_with_queue(&mut self, queue: Queue<Song>) {
        {
            let mut queue_lock = self.queue.lock().unwrap();
            *queue_lock = queue;
        }
        self.stop();
        let _ = self.run();
    }

    // TODO: add result if already playing, currently we just panic
    // unfortunately I can't make it move self, since I need the Player for the ui
    pub fn run(&mut self) -> Result<(), PlayerRunningError> {
        {
            let mut state_lock = self.state.write().unwrap();
            match *state_lock {
                PlayerState::Playing | PlayerState::Paused => return Err(PlayerRunningError),
                _ => {}
            }
            *state_lock = PlayerState::Paused;
        }
        println!("Running");

        let queue = self.queue.clone();
        let state = self.state.clone();
        let current = self.current.clone();
        let duration = self.time_playing.clone();
        let volume = self.volume.clone();

        let (tx, rx) = mpsc::channel::<PlayerMessage>();
        self.sender = Some(tx);

        thread::spawn(move || -> Result<()> {
            // Idk if 32KiB is too much or too little
            let buffer = HeapRb::<SampleType>::new(1024 * 32);
            let (mut producer, consumer) = buffer.split();

            let (device, stream_config) = init_cpal();
            let stream_channels = stream_config.channels() as usize;
            // TODO: This might not work because of different channel amounts, idk how mp3 works
            let audio_stream = match stream_config.sample_format() {
                SampleFormat::I16 => {
                    create_stream::<i16>(device, &stream_config.into(), consumer, volume)
                }
                SampleFormat::I32 => {
                    create_stream::<i32>(device, &stream_config.into(), consumer, volume)
                }
                SampleFormat::I64 => {
                    create_stream::<i64>(device, &stream_config.into(), consumer, volume)
                }
                SampleFormat::F32 => {
                    create_stream::<f32>(device, &stream_config.into(), consumer, volume)
                }
                SampleFormat::F64 => {
                    create_stream::<f64>(device, &stream_config.into(), consumer, volume)
                }
                sample_format => panic!("Unsupported sample format: '{sample_format}'"),
            }
            .unwrap();
            audio_stream.play().unwrap();
            println!("Created audio stream");

            loop {
                let song = {
                    let mut queue_lock = queue.lock().unwrap();
                    if let Some(song) = queue_lock.next() {
                        println!("Song found: {:?}", song.title);
                        let mut current_lock = current.write().unwrap();
                        *current_lock = Some(song.clone());
                        // FIXME: Cloning is annoying
                        song.clone()
                    } else {
                        println!("Got none from Queue, exiting");
                        let mut state_lock = state.write().unwrap();
                        *state_lock = PlayerState::Finished;

                        let mut current_lock = current.write().unwrap();
                        *current_lock = None;
                        break;
                    }
                };
                let channel_factor = stream_channels / song.params.channels.unwrap().count();
                let mss = {
                    let Ok(f) = fs::File::open(&song.path) else {
                        println!("Coudln't find the file for song: {}", song);
                        continue;
                    };
                    let media_source_options = MediaSourceStreamOptions::default();
                    MediaSourceStream::new(Box::new(f), media_source_options)
                };
                println!("Created mss");
                let seekable = mss.is_seekable();
                let time_base = song.time_base();
                let track_id = song.id;
                // These use unwrap, so I'll need to refactor this
                let mut format_reader = get_format_reader(mss);
                let mut decoder = song.get_decoder();
                println!("Created reader and decoder");

                {
                    let mut duration_lock = duration.write().unwrap();
                    *duration_lock = Default::default();

                    let mut state_lock = state.write().unwrap();
                    *state_lock = PlayerState::Playing;
                }

                let mut playing = true;
                let mut source_exhausted = false;
                let mut sample_deque: VecDeque<SampleType> = VecDeque::new();
                while !source_exhausted || !producer.is_empty() {
                    if let Ok(message) = rx.try_recv() {
                        match message {
                            PlayerMessage::Stop => {
                                break;
                            }
                            PlayerMessage::Pause => {
                                let mut state_lock = state.write().unwrap();
                                *state_lock = PlayerState::Paused;
                                playing = false;
                            }
                            PlayerMessage::Resume => {
                                let mut state_lock = state.write().unwrap();
                                *state_lock = PlayerState::Playing;
                                playing = true;
                            }
                            PlayerMessage::Seek(dur) => {
                                use symphonia::core::formats::{SeekMode, SeekTo};
                                let time: units::Time = dur.into();
                                // FormatReader is seekable depending on the MediaSourceStream.is_seekable() method
                                // I'm fairly certain this should always be true for mp3 files
                                // TODO: The bool `seekable` should be used to check if we can seek, I don't know how to handle that yet
                                let seeked_to = format_reader
                                    .seek(
                                        SeekMode::Accurate,
                                        SeekTo::Time {
                                            time,
                                            track_id: Some(track_id),
                                        },
                                    )
                                    .expect("Mp3 readers should always be seekable");
                                let mut dur_lock = duration.write().unwrap();
                                let time = time_base.calc_time(seeked_to.actual_ts);
                                *dur_lock = time.into();
                                // Reset the decoder after seeking, the docs say this is a necessary step
                                decoder = song.get_decoder();
                            }
                        }
                    }
                    if !playing {
                        continue;
                    }
                    if !sample_deque.is_empty() {
                        // If there is a buffer available, write data to the producer if there is space
                        while producer.vacant_len() >= channel_factor {
                            let Some(sample) = sample_deque.pop_front() else {
                                break;
                            };
                            for _ in 0..channel_factor {
                                producer.try_push(sample).unwrap();
                            }
                        }
                    } else {
                        // Push samples for the sample deque if none are available

                        // TODO: figure out resampling

                        if let Ok(packet) = format_reader.next_packet() {
                            {
                                let mut duration_lock = duration.write().unwrap();
                                *duration_lock = time_base.calc_time(packet.ts()).into();
                            }
                            source_exhausted = false;
                            let audio_buf = decoder.decode(&packet).unwrap();
                            let mut audio_buf_type: AudioBuffer<SampleType> =
                                audio_buf.make_equivalent();
                            audio_buf.convert(&mut audio_buf_type);
                            for (l, r) in audio_buf_type
                                .chan(0)
                                .iter()
                                .zip(audio_buf_type.chan(1).iter())
                            {
                                sample_deque.push_back(*l);
                                sample_deque.push_back(*r);
                            }
                        } else {
                            source_exhausted = true;
                        }
                    }
                }
            }
            Ok(())
        });
        Ok(())
    }

    /// Seek to a specific duration of the song.
    /// If the duration is longer than the maximum duration returns an error
    pub fn seek_duration(&self, duration: Duration) -> Result<bool, SeekError> {
        let dur_max = self.current().ok_or(SeekError::NoCurrentSong)?.duration();
        if duration > dur_max {
            return Err(SeekError::OutOfRange {
                to: duration,
                max: dur_max,
            });
        }
        println!("Seeking to: {:.2}", duration.as_secs_f32());
        Ok(self.send_message(PlayerMessage::Seek(duration)))
    }
}

fn get_format_reader(mss: MediaSourceStream) -> Box<dyn FormatReader> {
    // TODO: This should return a result instead of unwrapping
    let probe = symphonia::default::get_probe();
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format_opts = FormatOptions::default();
    format_opts.enable_gapless = true;
    let metadata_opts = MetadataOptions::default();
    probe
        .format(&hint, mss, &format_opts, &metadata_opts)
        .unwrap()
        .format
}

fn init_cpal() -> (cpal::Device, cpal::SupportedStreamConfig) {
    let device = cpal::default_host()
        .default_output_device()
        .expect("no output device available");

    // Create an output stream for the audio so we can play it
    // NOTE: If system doesn't support the file's sample rate, the program will panic when we try to play,
    // so we'll need to resample the audio to a supported config
    let supported_config_range = device
        .supported_output_configs()
        .expect("error querying audio output configs")
        .next()
        .expect("no supported audio config found");

    // Pick the best (highest) sample rate
    (device, supported_config_range.with_max_sample_rate())
}

fn write_audio<T: Sample>(
    data: &mut [T],
    samples: &mut impl Consumer<Item = SampleType>,
    volume: &RwLock<f32>,
    _cbinfo: &cpal::OutputCallbackInfo,
) where
    T: cpal::FromSample<SampleType>,
{
    // Channel remapping might be done here, to lower the load on the Player thread
    let volume = *volume.read().unwrap();
    for d in data.iter_mut() {
        // copy as many samples as we have.
        // if we run out, write silence
        // TODO: volume controls here
        match samples.try_pop() {
            Some(sample) => *d = T::from_sample(((sample as f32) * volume) as SampleType),
            None => *d = T::from_sample(SampleType::EQUILIBRIUM),
        }
    }
}

/// Create a stream to the `device`, reading data from the `consumer`
fn create_stream<T>(
    device: cpal::Device,
    stream_config: &StreamConfig,
    mut consumer: (impl Consumer<Item = SampleType> + std::marker::Send + 'static),
    volume: Arc<RwLock<f32>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + cpal::FromSample<SampleType>,
{
    let callback = move |data: &mut [T], cbinfo: &cpal::OutputCallbackInfo| {
        write_audio(data, &mut consumer, &volume, cbinfo)
    };
    let err_fn = |e| eprintln!("Stream error: {e}");
    device.build_output_stream(stream_config, callback, err_fn, None)
}
