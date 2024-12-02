use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, SizedSample};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::sync::{mpsc, MutexGuard};
use std::sync::{Arc, Mutex, RwLock};
use std::{fs, path::PathBuf, time::Duration};
use std::{io, thread};
use symphonia::core::audio::{AudioBuffer, Signal};
use symphonia::core::units;
use symphonia::core::{
    codecs::Decoder,
    errors::Result as SymphoniaResult,
    formats::{FormatOptions, FormatReader},
    io::MediaSourceStream,
};
use symphonia_bundle_mp3::{MpaDecoder, MpaReader};

type SampleType = f64;

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

pub struct Playlist {
    path: PathBuf,
    title: String,
    icon_path: PathBuf,
}

impl Playlist {
    pub fn new(path: PathBuf, title: String, icon_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            path: path.canonicalize()?,
            title,
            icon_path,
        })
    }

    pub fn songs(&self) -> std::io::Result<Vec<Song>> {
        Ok(self
            .path
            .read_dir()?
            .filter_map(|f| {
                let path = f.ok()?.path();
                if path.extension()? == "mp3" {
                    return Song::from_path("title".to_string(), path).ok();
                }
                None
            })
            .collect())
    }
}

#[derive(Copy, Clone, Debug)]
pub enum PlayerState {
    Paused,
    Playing,
    Finished,
    NotStarted,
}

pub enum PlayerMessage {
    Stop,
    Pause,
    Resume,
    Seek(Duration),
    Quit,
}

#[derive(Copy, Clone, Debug)]
pub struct Volume {
    percent: f64,
    multiplier: f64,
}

impl Volume {
    pub fn percent(&self) -> &f64 {
        &self.percent
    }

    /// Same as [`from_percent`], but fails when percentage is outside of the f32 range 0..1.
    /// 
    /// [`from_percent`]: Self::from_percent
    pub fn from_percent_checked(percent: f64) -> Result<Self, OutOfBoundsError<f64>> {
        if !(percent > 0. && percent < 1.) {
            return Err(OutOfBoundsError::range(percent, 0., 1.));
        }
        Ok(Self::from_percent(percent))
    }

    /// Calculates the sample multiplier depending on a percentage to adjust for human hearing.
    ///
    /// The curve is: `(e^(B*percent) - 1) / (e^B - 1)`, where B is 7.0.
    /// Why? Because.
    pub fn from_percent(percent: f64) -> Self {
        const B: f64 = 7.0;
        let multiplier = ((B * percent).exp() - 1.) / (B.exp() - 1.);
        Self {
            percent,
            multiplier,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Player {
    queue: Arc<Mutex<Queue<Song>>>,
    state: Arc<RwLock<PlayerState>>,
    /// None if the player hasn't started yes, the player's state is [`PlayerState::NotStarted`] in this case
    sender: Option<mpsc::Sender<PlayerMessage>>,
    time_playing: Arc<RwLock<Duration>>,
    volume: Arc<RwLock<Volume>>,
}

impl Player {
    /// Create a new player with the given volume.
    pub fn new(volume: Volume) -> Self {
        Self {
            queue: Arc::new(Mutex::new(Queue::new(RepeatMode::All))),
            state: Arc::new(RwLock::new(PlayerState::NotStarted)),
            sender: None,
            time_playing: Arc::new(RwLock::new(Duration::from_secs(0))),
            volume: Arc::new(RwLock::new(volume)),
        }
    }

    // TODO: fix this ugly mess
    pub fn with_queue(queue: Queue<Song>, volume: Volume) -> Self {
        Self {
            queue: Arc::new(Mutex::new(queue)),
            ..Player::new(volume)
        }
    }

    /// Return a Mutex guard to the inner queue.
    pub fn queue_mut(&mut self) -> MutexGuard<Queue<Song>> {
        self.queue.lock().unwrap()
    }

    /// Set the player's volume.
    pub fn set_volume(&mut self, volume: Volume) {
        let mut self_volume = self.volume.write().unwrap();
        *self_volume = volume
    }

    /// Get the player's volume
    pub fn volume(&self) -> Volume {
        *self.volume.read().unwrap()
    }

    /// If available, return a cloned version of the [`Song`] that's currently playing.
    pub fn current(&mut self) -> Option<Song> {
        let queue_lock = self.queue.lock().unwrap();
        queue_lock.current().cloned()
    }

    /// Return a bool if the player is currently paused.
    /// 
    /// The player might not be paused immediately after [`pause`].
    /// 
    /// [`pause`]: Self::pause
    pub fn is_paused(&self) -> bool {
        let state = *self.state.read().unwrap();
        matches!(state, PlayerState::Paused)
    }

    /// Return a bool if the player is currently paused.
    /// 
    /// The player might not be playing immediately after [`resume`].
    /// 
    /// [`resume`]: Self::resume
    pub fn is_playing(&self) -> bool {
        let state = *self.state.read().unwrap();
        matches!(state, PlayerState::Playing)
    }

    /// Return the player's state at this moment.
    pub fn state(&self) -> PlayerState {
        *self.state.read().unwrap()
    }

    /// Send a message to the audio playing thread.
    ///
    /// Returns true on successful messages.
    pub fn send_message(&self, message: PlayerMessage) -> bool {
        let Some(tx) = &self.sender else { return false };
        tx.send(message).is_ok()
    }

    /// Send a message to the audio thread to quit playing entirely.
    ///
    /// NOTE: the player's state is NOT updated to Finished by this call, but by the audio thread
    pub fn quit(&self) -> bool {
        self.send_message(PlayerMessage::Quit)
    }

    /// Send a message to the audio thread to stop the current song.
    ///
    /// If there are more songs available, they will be played.
    pub fn stop(&self) -> bool {
        self.send_message(PlayerMessage::Stop)
    }

    /// Send a message to the audio thread to pause playback.
    ///
    /// NOTE: the player's state is NOT updated to Paused by this call, but by the audio thread
    pub fn pause(&self) -> bool {
        self.send_message(PlayerMessage::Pause)
    }

    /// Send a message to the audio thread to resume playback.
    ///
    /// NOTE: the player's state is NOT updated to Playing by this call, but by the audio thread
    pub fn resume(&self) -> bool {
        self.send_message(PlayerMessage::Resume)
    }

    /// Start the player.
    /// 
    /// This method spawns a seperate thread which continously decodes audio for the current song, and pushes it to a consumer for the cpal library to use
    pub fn run(&mut self) -> Result<(), PlayerRunningError> {
        {
            let mut state_lock = self.state.write().unwrap();
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

        thread::spawn(move || {
            let (mut stream, mut stream_rx, mut stream_channels, mut producer) =
                stream_setup(volume.clone());
            stream.play().unwrap();
            'main_loop: loop {
                let song = {
                    let mut queue_lock = queue.lock().unwrap();
                    let Some(song) = queue_lock.next() else {
                        break;
                    };
                    println!("{song:?}");
                    song.clone()
                };
                let (mut reader, mut decoder) = song.reader_decoder().unwrap();
                let track = reader.default_track().unwrap();
                let channel_factor = stream_channels / track.codec_params.channels.unwrap().count();
                let track_id = track.id;
                let time_base = track.codec_params.time_base.unwrap();
                {
                    let mut time_playing_lock = time_playing.write().unwrap();
                    *time_playing_lock = Default::default();

                    let mut state_lock = player_state.write().unwrap();
                    *state_lock = PlayerState::Playing;
                }

                let mut playing = true;
                let mut source_exhausted = false;
                let mut sample_deque: VecDeque<SampleType> = VecDeque::new();
                while !source_exhausted || !producer.is_empty() {
                    match stream_rx.try_recv() {
                        // Currently we recreate the device and audio stream for any error, but I'm not sure if that's stupid
                        Ok(e) => {
                            let _ = control_tx.send(PlayerMessage::Pause);
                            (stream, stream_rx, stream_channels, producer) =
                                stream_setup(volume.clone());
                            println!("Got stream error: {e}");
                        },
                        Err(mpsc::TryRecvError::Disconnected) => break 'main_loop,
                        _ => (),
                    }
                    match control_rx.try_recv() {
                        Ok(message) => match message {
                            PlayerMessage::Quit => break 'main_loop,
                            PlayerMessage::Stop => break,
                            PlayerMessage::Pause => {
                                let mut state_lock = player_state.write().unwrap();
                                *state_lock = PlayerState::Paused;
                                playing = false;
                                stream.pause().unwrap();
                                // We can slow down the thread a bit if the player is paused
                                thread::sleep(Duration::from_millis(100));
                            }
                            PlayerMessage::Resume => {
                                let mut state_lock = player_state.write().unwrap();
                                *state_lock = PlayerState::Playing;
                                stream.play().unwrap();
                                playing = true;
                            }
                            PlayerMessage::Seek(dur) => {
                                use symphonia::core::formats::{SeekMode, SeekTo};
                                let time: units::Time = dur.into();
                                // FormatReader is seekable depending on the MediaSourceStream.is_seekable() method
                                // I'm fairly certain this should always be true for mp3 files
                                // TODO: The bool `seekable` should be used to check if we can seek, I don't know how to handle that yet
                                let seeked_to = reader
                                    .seek(
                                        SeekMode::Coarse,
                                        SeekTo::Time {
                                            time,
                                            track_id: Some(track_id),
                                        },
                                    )
                                    .expect("Mp3 readers should always be seekable");
                                let mut time_playing_lock = time_playing.write().unwrap();
                                let time = time_base.calc_time(seeked_to.actual_ts);
                                *time_playing_lock = time.into();
                                // Reset the decoder after seeking, the docs say this is a necessary step after seeking
                                decoder.reset();
                            }
                        },
                        // Break if the channel is disconnected, because this means the sender was dropped
                        Err(mpsc::TryRecvError::Disconnected) => break 'main_loop,
                        _ => (),
                    }
                    if !playing {
                        continue;
                    }
                    if !sample_deque.is_empty() {
                        // If there are samples available, write them to the producer if there is space
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

                        if let Ok(packet) = reader.next_packet() {
                            {
                                let mut duration_lock = time_playing.write().unwrap();
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
                                // In theory, this could make the sample deque grow as large as the song, which probably isn't good
                                sample_deque.push_back(*l);
                                sample_deque.push_back(*r);
                            }
                        } else {
                            source_exhausted = true;
                        }
                    }
                }
            }
            // Set the player's state to Finished after we break out of the loop
            let mut state_lock = player_state.write().unwrap();
            *state_lock = PlayerState::Finished;

            // Set the player's time_playing duration to 0 when finished
            let mut time_playing_lock = time_playing.write().unwrap();
            *time_playing_lock = Default::default();
        });
        Ok(())
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
        let mut queue_lock = self.queue.lock().unwrap();
        queue_lock.skip(1);
        self.stop();
    }

    /// Rewind to the beginning of the track if it has been playing long enough, otherwise the previous track.
    pub fn rewind(&mut self) {
        let time_playing = self.time_playing.read().unwrap().as_secs_f32();
        /// If the current song has been playing for longer than this constant, go back to the beginning of it
        const REWIND_TOLERANCE: f32 = 3.0;
        if time_playing < REWIND_TOLERANCE && self.current().is_some() {
            self.seek_duration(Duration::from_secs(0))
                .expect("Rewinding to 0 with a song playing should not fail");
        } else {
            let mut queue_lock = self.queue.lock().unwrap();
            queue_lock.rewind(1);
            self.stop();
        }
    }
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
    volume: &RwLock<Volume>,
    _cbinfo: &cpal::OutputCallbackInfo,
) where
    T: cpal::FromSample<SampleType>,
{
    // Channel remapping might be done here, to lower the load on the Player thread
    let volume = volume.read().unwrap();
    for d in data.iter_mut() {
        match samples.try_pop() {
            Some(sample) => *d = T::from_sample(sample * volume.multiplier),
            None => *d = T::from_sample(SampleType::EQUILIBRIUM),
        }
    }
}

/// Create a stream to the `device`, reading data from the `consumer`
fn create_stream<T>(
    device: cpal::Device,
    stream_config: &cpal::StreamConfig,
    stream_tx: mpsc::Sender<cpal::StreamError>,
    mut consumer: (impl Consumer<Item = SampleType> + std::marker::Send + 'static),
    volume: Arc<RwLock<Volume>>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: SizedSample + cpal::FromSample<SampleType>,
{
    let callback = move |data: &mut [T], cbinfo: &cpal::OutputCallbackInfo| {
        write_audio(data, &mut consumer, &volume, cbinfo)
    };
    // TODO: set up mpsc between Player thread and cpal thread to get new audio device on disconnect, or otherwise catch other errors
    let err_fn = move |e| {
        let _ = stream_tx.send(e);
    };
    device.build_output_stream(stream_config, callback, err_fn, None)
}

fn stream_setup(
    volume: Arc<RwLock<Volume>>,
) -> (
    cpal::Stream,
    mpsc::Receiver<cpal::StreamError>,
    usize,
    impl Producer<Item = SampleType>,
) {
    let (device, stream_config) = init_cpal();
    let stream_channels = stream_config.channels() as usize;
    const BUF_SIZE: usize = 32 * 1024;
    let (producer, consumer) = {
        let buf: HeapRb<f64> = HeapRb::new(BUF_SIZE);
        buf.split()
    };
    let (stream_tx, stream_rx) = mpsc::channel::<cpal::StreamError>();
    let stream = match stream_config.sample_format() {
        SampleFormat::I8 => {
            create_stream::<i8>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::I16 => {
            create_stream::<i16>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::I32 => {
            create_stream::<i32>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::I64 => {
            create_stream::<i64>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::U8 => {
            create_stream::<u8>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::U16 => {
            create_stream::<u16>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::U32 => {
            create_stream::<u32>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::U64 => {
            create_stream::<u64>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::F32 => {
            create_stream::<f32>(device, &stream_config.into(), stream_tx, consumer, volume)
        }
        SampleFormat::F64 => {
            create_stream::<f64>(device, &stream_config.into(), stream_tx, consumer, volume)
        }

        sample_format => panic!("Unsupported sample format: '{sample_format}'"),
    }
    .unwrap();
    stream.play().unwrap();
    (stream, stream_rx, stream_channels, producer)
}
