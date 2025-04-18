use cpal::{
    Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use ringbuf::{
    HeapRb,
    traits::{Consumer, Observer, Producer, Split},
};
use rubato::{FftFixedIn, Resampler};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fmt::Debug,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering}, mpsc::{self, Receiver}, Arc, Mutex, MutexGuard
    },
    thread,
    time::Duration,
};
use symphonia::core::{
    audio::Signal,
    codecs::Decoder,
    errors::{Error, Result as SymphoniaResult},
    formats::{FormatOptions, FormatReader},
    io::MediaSourceStream,
    units,
};
use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use triple_buffer::{Output, triple_buffer};

type SampleType = f64;
/// The buffer stores `[f64; 2]` so the number of samples is double.
const CHUNK_SIZE: usize = 512;

use crate::errors::{OutOfBoundsError, PlayerRunningError, SeekError};
use crate::queue::{Queue, RepeatMode};

/// Represents a song from a [`Player`]s queue.
///
/// Songs are played from a [`Player`], which uses a Symphonia reader and decoder read the samples from the file.
///
/// Songs should be created with [`from_path`].
///
/// [`from_path`]: Self::from_path
///
/// The duration of the song is automatically calculated when created.
#[derive(Clone, Debug)]
pub struct Song {
    id: u32,
    title: String,
    path: PathBuf,
    duration: Duration,
}

impl Song {
    fn new(id: u32, title: String, path: PathBuf, duration: Duration) -> Self {
        Self {
            id,
            title,
            path,
            duration,
        }
    }

    pub fn id(&self) -> &u32 {
        &self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn duration(&self) -> &Duration {
        &self.duration
    }

    /// Create a new Song from a mp3 file at `path`, and automatically calculate the duration from it.
    pub fn from_path(title: String, path: PathBuf) -> SymphoniaResult<Song> {
        let path = path.canonicalize()?;
        let reader = Self::reader(&path)?;
        let track = reader
            .default_track()
            .expect("Found mp3 file without a track, abort");
        let params = &track.codec_params;
        let time_base = params
            .time_base
            .expect("Every mp3 track should have a time base");
        let n_frames = params.n_frames.expect("Every mp3 track should have frames");
        let duration = time_base.calc_time(n_frames).into();
        Ok(Self::new(track.id, title, path, duration))
    }

    // Feels kinda dumb to have to get a reader for duration, and later for actually reading the data
    fn reader(path: &PathBuf) -> SymphoniaResult<MpaReader> {
        let file = fs::File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let reader_options = FormatOptions {
            enable_gapless: true,
            ..Default::default()
        };
        MpaReader::try_new(mss, &reader_options)
    }

    /// Try to get a reader and decoder for use in player to get audio samples
    fn reader_decoder(&self) -> SymphoniaResult<(MpaReader, MpaDecoder)> {
        let reader = Self::reader(&self.path)?;
        let track = reader
            .default_track()
            .expect("Every mp3 file should have a track");
        let decoder = MpaDecoder::try_new(&track.codec_params, &Default::default())?;
        Ok((reader, decoder))
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Playlist {
    name: String,
    path: PathBuf,
    icon_path: Option<PathBuf>,
}

impl Playlist {
    pub fn new(path: PathBuf, name: String, icon_path: Option<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            path: path.canonicalize()?,
            name,
            icon_path,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn icon_path(&self) -> Option<&Path> {
        self.icon_path.as_ref().map(|v| &**v)
    }

    pub fn songs(&self) -> std::io::Result<Vec<Song>> {
        Ok(self
            .path
            .read_dir()?
            .filter_map(|f| {
                let path = f.ok()?.path();
                if path.extension()? == "mp3" {
                    let title = path.file_name().unwrap().to_str().unwrap();
                    return Song::from_path(title.into(), path).ok();
                }
                None
            })
            .collect())
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PlayerState {
    Paused,
    Playing,
    Finished,
    NotStarted,
}

pub enum PlayerMessage {
    Stop,
    // Pause,
    // Resume,
    Seek(Duration),
    Quit,
}

#[derive(Debug)]
pub struct AtomicVolume {
    percent: AtomicU64,
    multiplier: AtomicU64,
}

impl AtomicVolume {
    pub fn percent(&self) -> f64 {
        let as_u64 = self.percent.load(Ordering::Relaxed);
        f64::from_bits(as_u64)
    }

    pub fn multiplier(&self) -> f64 {
        let as_u64 = self.multiplier.load(Ordering::Relaxed);
        f64::from_bits(as_u64)
    }

    fn set_volume(&self, other: &Self) {
        let percent = other.percent.load(Ordering::Acquire);
        let multiplier = other.multiplier.load(Ordering::Acquire);
        self.percent.store(percent, Ordering::Relaxed);
        self.multiplier.store(multiplier, Ordering::Relaxed);
    }

    /// Same as [`from_percent`], but fails when percentage is outside of the f32 range `0..1`.
    ///
    /// [`from_percent`]: Self::from_percent
    pub fn from_percent_checked(percent: f64) -> Result<Self, OutOfBoundsError<f64>> {
        if !(0. ..=1.).contains(&percent) {
            return Err(OutOfBoundsError::range(percent, 0., 1.));
        }
        Ok(Self::from_percent(percent))
    }

    /// Calculates the sample multiplier depending on a percentage to adjust for human hearing being logarithmic.
    ///
    /// The curve is: `10^(Ar/20)`, where `Ar` is the relative attenuation interpolated between -60 and 0 decibels, based on the percentage.
    /// If the percentage is 0. the attenuation is -infinity.
    /// Why? Because I think this is how loudness works please help.
    pub fn from_percent(percent: f64) -> Self {
        assert!((0. ..=1.).contains(&percent));
        let multiplier = if percent == 0. || percent == 1. {
            percent
        } else {
            const MIN_AR: f64 = -60f64;
            const MAX_AR: f64 = 0f64;
            let ar_interpolated = MIN_AR + (MAX_AR - MIN_AR) * percent;
            10f64.powf(ar_interpolated / 20.)
        };
        Self {
            percent: AtomicU64::new(percent.to_bits()),
            multiplier: AtomicU64::new(multiplier.to_bits()),
        }
    }
}

// idk if this is dumb dumb
/// A wrapper around AtomicU64, storing a number of milliseconds as a duration
#[derive(Debug, Default)]
pub struct AtomicMilliseconds(AtomicU64);

impl AtomicMilliseconds {
    pub fn new(millis: u64) -> Self {
        Self(AtomicU64::new(millis))
    }

    pub fn as_secs_f64(&self) -> f64 {
        let millis = self.0.load(Ordering::Relaxed);
        millis as f64 / 1000.
    }

    pub fn set_millis(&self, millis: u64) {
        self.0.store(millis, Ordering::Relaxed)
    }
}

pub enum PlayerUpdate {
    SongChange { song: Option<Song>, index: usize },
    DeviceDisconnect,
    // DeviceChange(),
    StateChange,
}
impl PlayerUpdate {
    fn song_change(index: usize, song: Option<Song>) -> PlayerUpdate {
        Self::SongChange { song, index }
    }
}

pub struct Player {
    queue: Arc<Mutex<Queue<Song>>>,
    state: Arc<Mutex<PlayerState>>,
    /// None if the player hasn't started yes, the player's state is `PlayerState::NotStarted` in this case
    sender: Option<mpsc::Sender<PlayerMessage>>,

    time_playing: Arc<AtomicMilliseconds>,
    volume: Arc<AtomicVolume>,
}

// TODO: turn into builder pattern
impl Player {
    /// Create a new player with the given volume.
    pub fn new(volume: f64) -> Self {
        Self {
            queue: Mutex::new(Queue::new(RepeatMode::All)).into(),
            state: Mutex::new(PlayerState::NotStarted).into(),
            sender: None,
            time_playing: AtomicMilliseconds::default().into(),
            volume: AtomicVolume::from_percent(volume).into(),
        }
    }

    pub fn with_queue(queue: Queue<Song>, volume: f64) -> Self {
        Self {
            queue: Arc::new(Mutex::new(queue)),
            ..Player::new(volume)
        }
    }

    pub fn queue_mut(&self) -> MutexGuard<Queue<Song>> {
        self.queue.lock().unwrap()
    }

    /// Set the player's volume.
    pub fn set_volume(&mut self, volume: &AtomicVolume) {
        self.volume.set_volume(volume);
    }

    /// Get the player's volume
    pub fn volume(&self) -> &AtomicVolume {
        self.volume.as_ref()
    }

    /// If available, return a cloned version of the [`Song`] that's currently playing.
    pub fn current(&self) -> Option<Song> {
        self.queue.lock().unwrap().current().cloned()
    }

    pub fn time_playing(&self) -> &AtomicMilliseconds {
        self.time_playing.as_ref()
    }

    /// Return a bool if the player is currently paused.
    ///
    /// The player might not be paused immediately after [`pause`].
    ///
    /// [`pause`]: Self::pause
    pub fn is_paused(&self) -> bool {
        let state = *self.state.lock().unwrap();
        matches!(state, PlayerState::Paused)
    }

    /// Return the state of the audio decoding thread.
    ///
    /// NOTE: the metods `pause`, `resume`, `quit`, and `stop` don't update the state automatically so using this method may result in a race condition.
    pub fn state(&self) -> PlayerState {
        *self.state.lock().unwrap()
    }

    /// Send a message to the audio playing thread.
    ///
    /// Returns true on successful messages.
    pub fn send_message(&mut self, message: PlayerMessage) -> bool {
        let Some(tx) = &self.sender else { return false };
        tx.send(message).is_ok()
    }

    /// Send a message to the audio thread to quit playing entirely.
    ///
    /// NOTE: the player's state is NOT updated to Finished by this call, but by the audio thread.
    /// This means you should not depend on the method [`Player::state`] for data, as it can result in a race condition.
    ///
    /// [`Player::state`]: Self::state
    pub fn quit(&mut self) -> bool {
        self.send_message(PlayerMessage::Quit)
    }

    /// Send a message to the audio thread to stop the current song.
    ///
    /// If there are more songs available, they will be played.
    ///
    /// NOTE: the player's state is NOT updated to Finished (if there are no new songs) by this call, but by the audio thread.
    /// This means you should not depend on the method [`Player::state`] for data, as it can result in a race condition.
    ///
    /// [`Player::state`]: Self::state
    pub fn stop(&mut self) -> bool {
        self.send_message(PlayerMessage::Stop)
    }

    /// Send a message to the audio thread to pause playback.
    ///
    /// NOTE: the player's state is NOT updated to Paused by this call, but by the audio thread
    /// This means you should not depend on the method [`Player::state`] for data, as it can result in a race condition.
    ///
    /// [`Player::state`]: Self::state
    pub fn pause(&mut self) {
        let mut state = self.state.lock().unwrap();
        *state = PlayerState::Paused;

        // self.send_message(PlayerMessage::Pause)
    }

    /// Send a message to the audio thread to resume playback.
    ///
    /// NOTE: the player's state is NOT updated to Playing by this call, but by the audio thread
    /// This means you should not depend on the method [`Player::state`] for data, as it can result in a race condition.
    ///
    /// [`Player::state`]: Self::state
    pub fn resume(&mut self) {
        let mut state = self.state.lock().unwrap();
        *state = PlayerState::Playing;

        // self.send_message(PlayerMessage::Resume)
    }

    /// Start the player.
    ///
    /// This method spawns a seperate thread which continously decodes audio for the current song, and pushes it to a consumer for the cpal library to use
    pub fn run(
        &mut self,
        buffer_size: usize,
    ) -> Result<Receiver<PlayerUpdate>, PlayerRunningError> {
        {
            let mut state_lock = self.state.lock().unwrap();
            match *state_lock {
                PlayerState::Playing | PlayerState::Paused => return Err(PlayerRunningError),
                _ => {}
            }
            *state_lock = PlayerState::Paused;
        }
        let queue = self.queue.clone();
        let player_state = self.state.clone();
        let time_playing = self.time_playing.clone();
        let volume = self.volume.clone();

        let (control_tx, control_rx) = mpsc::channel::<PlayerMessage>();
        self.sender = Some(control_tx.clone());

        let (player_update_tx, player_update_rx) = mpsc::channel::<PlayerUpdate>();

        // DECODER THREAD
        thread::spawn(move || {
            // Assume we start at 44.1k so the resampler doesn't break;
            let mut last_song_sample_rate = 44100;
            let (mut sample_rate_update_input, mut sample_rate_update_output) =
                triple_buffer(&last_song_sample_rate);
            let (mut stream, mut stream_rx, mut producer) =
                stream_setup(sample_rate_update_output, buffer_size, volume.clone());
            stream.play().unwrap();
            'main_loop: loop {
                let song = {
                    let mut queue_lock = queue.lock().unwrap();
                    let index = queue_lock.index();
                    let next_song = queue_lock.next_item();
                    let _ =
                        player_update_tx.send(PlayerUpdate::song_change(index, next_song.cloned()));
                    let Some(song) = next_song else {
                        break;
                    };
                    song.clone()
                };
                let (mut reader, mut decoder) = song.reader_decoder().unwrap();
                let track = reader.default_track().unwrap();
                let track_id = track.id;
                let time_base = track.codec_params.time_base.unwrap();
                time_playing.set_millis(0);
                {
                    let mut state_lock = player_state.lock().unwrap();
                    *state_lock = PlayerState::Playing;
                }

                let song_sample_rate = track.codec_params.sample_rate.unwrap();
                if last_song_sample_rate != song_sample_rate {
                    sample_rate_update_input.write(song_sample_rate);
                    last_song_sample_rate = song_sample_rate;
                }

                let mut sample_deque = VecDeque::new();

                let mut playing = true;
                'song_loop: loop {
                    match stream_rx.try_recv() {
                        // Currently we recreate the device and audio stream for any error, but I'm not sure if that's stupid
                        Ok(e) => {
                            {
                                let mut state = player_state.lock().unwrap();
                                *state = PlayerState::Paused;
                            }
                            (sample_rate_update_input, sample_rate_update_output) =
                                triple_buffer(&last_song_sample_rate);
                            (stream, stream_rx, producer) = stream_setup(
                                sample_rate_update_output,
                                buffer_size,
                                volume.clone(),
                            );
                            let _ = player_update_tx.send(PlayerUpdate::DeviceDisconnect);
                            println!("Got stream error: {e}");
                        }
                        Err(mpsc::TryRecvError::Disconnected) => break 'main_loop,
                        _ => (),
                    }
                    for message in control_rx.try_iter() {
                        match message {
                            PlayerMessage::Quit => break 'main_loop,
                            PlayerMessage::Stop => break 'song_loop,
                            // PlayerMessage::Pause => {
                            //     {
                            //         let mut state_lock = player_state.lock().unwrap();
                            //         *state_lock = PlayerState::Paused;
                            //     }
                            //     playing = false;
                            //     stream.pause().unwrap();
                            //     // We can slow down the thread a bit if the player is paused
                            //     thread::sleep(Duration::from_millis(100));
                            // }
                            // PlayerMessage::Resume => {
                            //     {
                            //         let mut state_lock = player_state.lock().unwrap();
                            //         *state_lock = PlayerState::Playing;
                            //     }
                            //     stream.play().unwrap();
                            //     playing = true;
                            // }
                            PlayerMessage::Seek(dur) => {
                                use symphonia::core::formats::{SeekMode, SeekTo};
                                let time: units::Time = dur.into();
                                // FormatReader is seekable depending on the MediaSourceStream.is_seekable() method
                                // I'm fairly certain this should always be true for mp3 files
                                // TODO: The bool `seekable` should be used to check if we can seek, I don't know how to handle that yet
                                let millis = match reader.seek(
                                    SeekMode::Coarse,
                                    SeekTo::Time {
                                        time,
                                        track_id: Some(track_id),
                                    },
                                ) {
                                    Ok(seeked_to) => {
                                        let duration: Duration =
                                            time_base.calc_time(seeked_to.actual_ts).into();
                                        duration.as_millis() as u64
                                    }
                                    Err(e) => match e {
                                        // IoError from seeking (I think) only happens when the format reader reaches EOF, at which point we can skip to the next song
                                        Error::IoError(_) => continue 'main_loop,
                                        e => panic!("{}", e),
                                    },
                                };
                                time_playing.set_millis(millis);
                                // Reset the decoder after seeking, the docs say this is a necessary step after seeking
                                decoder.reset();
                            }
                        }
                    }
                    {
                        let state = player_state.lock().unwrap();
                        if playing {
                            if *state == PlayerState::Paused {
                                playing = false;
                                stream.pause().unwrap();
                            }
                        } else {
                            if *state == PlayerState::Playing {
                                playing = true;
                                stream.play().unwrap();
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    if !playing {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }

                    if !sample_deque.is_empty() {
                        while !producer.is_full() {
                            let Some(sample) = sample_deque.pop_front() else {
                                break;
                            };
                            producer.try_push(sample).unwrap();
                        }
                    }
                    if sample_deque.is_empty() {
                        let Ok(packet) = reader.next_packet() else {
                            break 'song_loop;
                        };
                        let audio_buf_ref = decoder.decode(&packet).unwrap();
                        let mut audio_buf = audio_buf_ref.make_equivalent();
                        audio_buf_ref.convert(&mut audio_buf);
                        let mut sample_iter = audio_buf
                            .chan(0)
                            .iter()
                            .zip(audio_buf.chan(1))
                            .map(|t| [*t.0, *t.1]);
                        while producer.vacant_len() > 2 {
                            let Some(pair) = sample_iter.next() else {
                                break;
                            };
                            producer.try_push(pair).unwrap();
                        }
                        sample_deque.extend(sample_iter);
                        let dur: Duration = time_base.calc_time(packet.ts()).into();
                        time_playing.set_millis(dur.as_millis() as u64);
                    }
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        });
        Ok(player_update_rx)
    }

    /// Seek to the given duration in the song, if one is currently playing.
    ///
    /// If the duration is longer than the maximum duration returns an error.
    pub fn seek_duration(&mut self, duration: Duration) -> Result<bool, SeekError> {
        let duration_max = self.current().ok_or(SeekError::NoCurrentSong)?.duration;
        if duration > duration_max {
            return Err(SeekError::out_of_range(duration, duration_max));
        }
        Ok(self.send_message(PlayerMessage::Seek(duration)))
    }

    /// Skip to the next song.
    pub fn fast_forward(&mut self) {
        self.stop();
    }

    /// Rewind to the beginning of the track if it has been playing long enough, otherwise the previous track.
    pub fn rewind(&mut self) {
        let time_playing = self.time_playing.as_secs_f64();
        /// If the current song has been playing for longer than this constant, go back to the beginning of it
        const REWIND_TOLERANCE: f64 = 3.0;
        if time_playing > REWIND_TOLERANCE && self.current().is_some() {
            self.seek_duration(Duration::from_secs(0))
                .expect("Rewinding to 0 with a song playing should not fail");
        } else {
            self.queue_mut().rewind(1);
            self.stop();
        }
    }

    pub fn is_running(&self) -> bool {
        let state = self.state.lock().unwrap();
        matches!(*state, PlayerState::Paused | PlayerState::Playing)
    }
}

fn init_cpal() -> (cpal::Device, cpal::SupportedStreamConfig) {
    let device = cpal::default_host()
        .default_output_device()
        .expect("no output device available");

    let stream_config = device
        .default_output_config()
        .expect("A device should have a default config");

    (device, stream_config)
}

fn write_audio<T>(
    data: &mut [T],
    samples: &mut VecDeque<SampleType>,
    channel_factor: u16,
    volume: &AtomicVolume,
    _cbinfo: &cpal::OutputCallbackInfo,
) where
    T: Sample + cpal::FromSample<SampleType>,
{
    for chunk in data.chunks_mut(channel_factor.into()) {
        let sample_scaled = if let Some(sample) = samples.pop_front() {
            (sample * volume.multiplier()).to_sample()
        } else {
            T::EQUILIBRIUM
        };
        for d in chunk.iter_mut() {
            *d = sample_scaled;
        }
    }
}

/// Create a stream to the `device`, reading data from the `consumer`
fn create_stream<T>(
    device: cpal::Device,
    stream_config: &cpal::StreamConfig,
    mut sample_rate_update: Output<u32>,
    stream_tx: mpsc::Sender<cpal::StreamError>,
    mut consumer: (impl Consumer<Item = [SampleType; 2]> + std::marker::Send + 'static),
    volume: Arc<AtomicVolume>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + cpal::FromSample<SampleType>,
{
    // FIXME: This could possibly break if the mp3 file is mono.
    // This can probably be fixed by pushing the same sample twice in the decoder thread if it is.
    let channel_factor = stream_config.channels / 2;

    let sample_rate_in = *sample_rate_update.read() as usize;
    let sample_rate_out = stream_config.sample_rate.0 as usize;
    // If the input and output sample rates are the same, we can bypass resampling and write the samples as they are
    let mut bypass_resampler = sample_rate_in == sample_rate_out;
    let mut resampler: FftFixedIn<SampleType> =
        FftFixedIn::new(sample_rate_in, sample_rate_out, CHUNK_SIZE, 1, 2).unwrap();

    let mut samples_in: Vec<Vec<f64>> = vec![Vec::new(), Vec::new()];
    let mut sample_deque = VecDeque::new();
    let mut samples_out = resampler.output_buffer_allocate(true);

    let callback = move |data: &mut [T], cbinfo: &cpal::OutputCallbackInfo| {
        // Check if the input sample rate has updated from the decoder thread and if it has, recreate the resampler.
        if sample_rate_update.update() {
            let new_sample_rate_in = *sample_rate_update.read() as usize;
            bypass_resampler = new_sample_rate_in == sample_rate_out;
            resampler =
                FftFixedIn::new(new_sample_rate_in, sample_rate_out, CHUNK_SIZE, 1, 2).unwrap();
            samples_out = resampler.output_buffer_allocate(true);
        }
        while sample_deque.len() < data.len() {
            let samples_needed = if bypass_resampler {
                data.len()
            } else {
                resampler.input_frames_next() - samples_in[0].len()
            };
            for [l, r] in consumer.pop_iter().take(samples_needed) {
                samples_in[0].push(l);
                samples_in[1].push(r)
            }
            let (samples_out, consumed, output) = if bypass_resampler {
                (&samples_in, samples_in[0].len(), samples_in[0].len())
            } else {
                let samples_needed = resampler.input_frames_next();
                let samples_len = samples_in[0].len();
                if samples_needed > samples_len {
                    samples_in[0]
                        .extend(std::iter::repeat(0f64).take(samples_needed - samples_len));
                    samples_in[1]
                        .extend(std::iter::repeat(0f64).take(samples_needed - samples_len));
                }
                let (consumed, output) = resampler
                    .process_into_buffer(&samples_in, &mut samples_out, None)
                    .unwrap();
                (&samples_out, consumed, output)
            };
            for (l, r) in samples_out[0]
                .iter()
                .take(output)
                .zip(samples_out[1].iter().take(output))
            {
                sample_deque.push_back(*l);
                sample_deque.push_back(*r);
            }
            drop(samples_in[0].drain(0..consumed));
            drop(samples_in[1].drain(0..consumed));
        }
        write_audio(data, &mut sample_deque, channel_factor, &volume, cbinfo);
    };
    let err_fn = move |e| {
        eprintln!("{e}");
        let _ = stream_tx.send(e);
    };
    device.build_output_stream(stream_config, callback, err_fn, None)
}

fn stream_setup(
    sample_rate_update: Output<u32>,
    buffer_size: usize,
    volume: Arc<AtomicVolume>,
) -> (
    cpal::Stream,
    mpsc::Receiver<cpal::StreamError>,
    impl Producer<Item = [SampleType; 2]>,
) {
    let (device, stream_config) = init_cpal();
    let (producer, consumer) = {
        let buf: HeapRb<[f64; 2]> = HeapRb::new(buffer_size);
        buf.split()
    };
    let (stream_tx, stream_rx) = mpsc::channel::<cpal::StreamError>();
    let stream = match stream_config.sample_format() {
        SampleFormat::I8 => create_stream::<i8>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::I16 => create_stream::<i16>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::I32 => create_stream::<i32>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::I64 => create_stream::<i64>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::U8 => create_stream::<u8>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::U16 => create_stream::<u16>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::U32 => create_stream::<u32>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::U64 => create_stream::<u64>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::F32 => create_stream::<f32>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),
        SampleFormat::F64 => create_stream::<f64>(
            device,
            &stream_config.into(),
            sample_rate_update,
            stream_tx,
            consumer,
            volume,
        ),

        sample_format => panic!("Unsupported sample format: '{sample_format}'"),
    }
    .unwrap();
    (stream, stream_rx, producer)
}
