use anyhow::{anyhow, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Sample, SampleFormat, SizedSample, StreamConfig,
};
use egui::mutex::RwLock;
use rand::seq::SliceRandom;
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};
use std::{
    fmt::Display,
    fs::{self, File},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Duration,
};
use symphonia::core::{
    audio::{AudioBuffer, Signal},
    codecs::{CodecParameters, Decoder},
    formats::{FormatOptions, FormatReader},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::MetadataOptions,
    probe::Hint,
    units::{Time, TimeBase},
};

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

// A new queue is created everytime a different set of songs is loaded,
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

    /// Return a reference to what the next item **could be**, if the repeat mode doesn't change until then.
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

    pub fn push(&mut self, value: T) {
        self.items.push(value)
    }

    pub fn insert(&mut self, index: usize, value: T) {
        self.items.insert(index, value)
    }

    pub fn swap(&mut self, a: usize, b: usize) {
        self.items.swap(a, b)
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

    fn decoder(&self) -> Box<dyn Decoder> {
        // TODO: this should return a result instead of unwrapping
        let codec_registry = symphonia::default::get_codecs();
        codec_registry
            .make(&self.params, &Default::default())
            .unwrap()
    }

    fn time_base(&self) -> TimeBase {
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

pub struct Playlist {
    name: String,
    path: PathBuf,
}

impl Playlist {
    pub fn new(name: String, path: PathBuf) -> Self {
        // TODO: Conver to Result if the path doesn't exist
        Self { name, path }
    }

    pub fn songs(&self) -> Vec<Song> {
        let probe = symphonia::default::get_probe();
        let paths = fs::read_dir(&self.path).expect("Playlist path invalid");
        let songs = paths
            .filter_map(|path| {
                let path = path.ok()?;
                let mss = if let Ok(f) = File::open(path.path()) {
                    let media_source_options = MediaSourceStreamOptions::default();
                    MediaSourceStream::new(Box::new(f), media_source_options)
                } else {
                    return None;
                };
                let format_reader = {
                    let mut hint = Hint::new();
                    hint.with_extension("mp3");
                    let mut format_opts = FormatOptions::default();
                    format_opts.enable_gapless = true;
                    let metadata_opts = MetadataOptions::default();
                    probe
                        .format(&hint, mss, &format_opts, &metadata_opts)
                        .unwrap()
                        .format
                };
                let track = format_reader.default_track().unwrap();
                let params = &track.codec_params;

                // Figure out a way to collect title from metadata
                // Metadata appears to be empty for test tracks
                let song_title = path
                    .file_name()
                    .into_string()
                    .expect("Could not convert filename to string");
                Some(Song::new(track.id, song_title, path.path(), params.clone()))
            })
            .collect();
        songs
    }

    pub fn name(&self) -> &str {
        &self.name
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
    /// Indicates the `Player` thread has finished at least once.
    Finished,
    Paused,
    Playing,
}

#[derive(Copy, Clone, Debug)]
pub enum SeekError {
    OutOfRange { to: Duration, max: Duration },
    NoCurrentSong,
}

pub struct Player {
    queue: Arc<Mutex<Queue<Song>>>,
    /// None if the player hasn't started yet.
    sender: Option<mpsc::Sender<PlayerMessage>>,
    state: Arc<RwLock<PlayerState>>,
    /// non-zero if the player is playing, this isn't an indicator of state
    time_playing: Arc<RwLock<Duration>>,
    current: Arc<RwLock<Option<Song>>>,
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
        }
    }

    /// Return a reference to the Player's queue
    pub fn queue(&self) -> &Mutex<Queue<Song>> {
        self.queue.as_ref()
    }

    pub fn duration(&self) -> Duration {
        *self.time_playing.read()
    }

    pub fn current(&self) -> Option<Song> {
        // TODO: Make this return a reference to the Song if possible
        self.current.read().clone()
    }

    /// Send a message to the thread playing the audio
    /// See `PlayerMessage` for more info
    /// Returns a bool to signal message success
    /// `true` means the message was sent successfully
    pub fn send_message(&self, message: PlayerMessage) -> bool {
        if let Some(sender) = &self.sender {
            return sender.send(message).is_ok();
        }
        false
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
    /// Returns `true` on successfull message
    pub fn skip(&mut self, n: usize) -> bool {
        let mut queue_lock = self.queue.lock().unwrap();
        queue_lock.skip(n);
        self.stop()
    }

    /// Rewing to the previous song
    ///
    /// Returns `true` on successfull message
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
        *self.state.read()
    }

    // TODO: add result if already playing, currently we just panic
    pub fn run(&mut self) {
        println!("Running");
        {
            let mut state_lock = self.state.write();
            match *state_lock {
                PlayerState::Playing | PlayerState::Paused => panic!("Player already started"),
                _ => {}
            }
            *state_lock = PlayerState::Paused;
        }

        let queue = self.queue.clone();
        let state = self.state.clone();
        let current = self.current.clone();
        let duration = self.time_playing.clone();

        let (tx, rx) = mpsc::channel::<PlayerMessage>();
        self.sender = Some(tx);

        println!("Pre thread");
        thread::spawn(move || -> Result<()> {
            // Idk if 32KiB is too much or too little
            let buffer = HeapRb::<SampleType>::new(1024 * 32);
            let (mut producer, consumer) = buffer.split();

            let (device, stream_config) = init_cpal();
            let stream_channels = stream_config.channels() as usize;
            // TODO: This might not work because of different channel amounts, idk how mp3 works
            let audio_stream = match stream_config.sample_format() {
                SampleFormat::I16 => create_stream::<i16>(device, &stream_config.into(), consumer),
                SampleFormat::I32 => create_stream::<i32>(device, &stream_config.into(), consumer),
                SampleFormat::I64 => create_stream::<i64>(device, &stream_config.into(), consumer),
                SampleFormat::F32 => create_stream::<f32>(device, &stream_config.into(), consumer),
                SampleFormat::F64 => create_stream::<f64>(device, &stream_config.into(), consumer),
                sample_format => panic!("Unsupported sample format: '{sample_format}'"),
            }
            .unwrap();
            audio_stream.play().unwrap();
            println!("Created audio stream");

            loop {
                let song = {
                    let mut queue_lock = queue.lock().map_err(|e| anyhow!("{e}"))?;
                    if let Some(song) = queue_lock.next() {
                        println!("Song found: {:?}", song.title);
                        let mut current_lock = current.write();
                        *current_lock = Some(song.clone());
                        // FIXME: Cloning is annoying
                        song.clone()
                    } else {
                        println!("Got none from Queue, exiting");
                        let mut state_lock = state.write();
                        *state_lock = PlayerState::Finished;
                        break;
                    }
                };
                let mss = {
                    if let Ok(f) = File::open(&song.path) {
                        let media_source_options = MediaSourceStreamOptions::default();
                        MediaSourceStream::new(Box::new(f), media_source_options)
                    } else {
                        println!("Coudln't find the file for song: {}", song);
                        continue;
                    }
                };
                println!("Created mss");
                // These use unwrap, so I'll need to refactor this
                let mut format_reader = get_format_reader(mss);
                let mut decoder = song.decoder();
                println!("Created reader and decoder");
                let time_base = song.params.time_base.unwrap();

                {
                    let mut duration_lock = duration.write();
                    *duration_lock = Default::default();

                    let mut state_lock = state.write();
                    *state_lock = PlayerState::Playing;
                }

                let mut playing = true;
                let mut source_exhausted = false;
                let mut sample_vec: Vec<SampleType> = vec![];
                while !source_exhausted || !producer.is_empty() {
                    if let Ok(message) = rx.try_recv() {
                        match message {
                            PlayerMessage::Stop => {
                                break;
                            }
                            PlayerMessage::Pause => {
                                let mut state_lock = state.write();
                                *state_lock = PlayerState::Paused;
                                playing = false;
                            }
                            PlayerMessage::Resume => {
                                let mut state_lock = state.write();
                                *state_lock = PlayerState::Playing;
                                playing = true;
                            }
                            PlayerMessage::Seek(dur) => {
                                use symphonia::core::formats::{SeekMode, SeekTo};
                                let time: Time = dur.into();
                                let track_id = song.id;
                                let seeked_to = format_reader
                                    .seek(
                                        SeekMode::Accurate,
                                        SeekTo::Time {
                                            time,
                                            track_id: Some(track_id),
                                        },
                                    )
                                    // idk why this would fail yet and I can't be bothered to look it up
                                    .expect("Temporary, why did you fail");
                                let mut dur_lock = duration.write();
                                let time_base = song.time_base();
                                let time = time_base.calc_time(seeked_to.actual_ts);
                                *dur_lock = time.into();
                                // Reset the decoder, the docs say this should be done
                                decoder = song.decoder();
                            }
                        }
                    }
                    if !playing {
                        continue;
                    }
                    if !sample_vec.is_empty() {
                        // If there is a buffer available, write data to the producer if there is space
                        let n = producer.vacant_len().min(sample_vec.len());
                        if n > 0 {
                            producer.push_iter(sample_vec.drain(0..n));
                        } else {
                            thread::sleep(Duration::from_millis(10));
                        }
                    } else {
                        // Generate the sample buffer if we fully used the last one

                        // TODO: figure out resampling

                        if let Ok(packet) = format_reader.next_packet() {
                            {
                                let mut duration_lock = duration.write();
                                *duration_lock = time_base.calc_time(packet.ts()).into();
                            }
                            source_exhausted = false;
                            let audio_buf = decoder.decode(&packet).unwrap();
                            let mut audio_buf_type: AudioBuffer<SampleType> =
                                audio_buf.make_equivalent();
                            audio_buf.convert(&mut audio_buf_type);
                            let channels = audio_buf.spec().channels.count();
                            let channel_factor = stream_channels / channels;
                            let num_samples = audio_buf.frames() * stream_channels;
                            sample_vec = vec![SampleType::EQUILIBRIUM; num_samples];
                            // TODO: It's probably a better idea to copy only the audio buffer, and adapt the channel layout while writing
                            for ch in 0..channels {
                                let channel_slice = audio_buf_type.chan(ch);
                                for (chunk, source) in sample_vec
                                    .chunks_mut(channel_factor)
                                    .step_by(channels)
                                    .zip(channel_slice)
                                {
                                    for dst in chunk {
                                        *dst = source.to_sample();
                                    }
                                }
                            }
                        } else {
                            source_exhausted = true;
                        }
                    }
                }
            }
            Ok(())
        });
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
    // TODO: This should return a return instead of unwrapping
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
    _cbinfo: &cpal::OutputCallbackInfo,
) where
    T: cpal::FromSample<SampleType>,
{
    for d in data.iter_mut() {
        // copy as many samples as we have.
        // if we run out, write silence
        match samples.try_pop() {
            Some(sample) => *d = T::from_sample(sample),
            None => *d = T::from_sample(SampleType::EQUILIBRIUM),
        }
    }
}

/// Create a stream to the `device`, reading data from the `consumer`
fn create_stream<T>(
    device: cpal::Device,
    stream_config: &StreamConfig,
    mut consumer: (impl Consumer<Item = SampleType> + std::marker::Send + 'static),
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + cpal::FromSample<SampleType>,
{
    let callback = move |data: &mut [T], cbinfo: &cpal::OutputCallbackInfo| {
        write_audio(data, &mut consumer, cbinfo)
    };
    let err_fn = |e| eprintln!("Stream error: {e}");
    let stream = device.build_output_stream(stream_config, callback, err_fn, None)?;
    Ok(stream)
}
