use std::convert::Infallible;
use std::io::{Read, Seek, SeekFrom};
use std::io::ErrorKind::NotSeekable;
use std::net::{AddrParseError, Ipv4Addr, SocketAddrV4};
use std::ops::{Deref, DerefMut};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use backoff::{ExponentialBackoff};
use libpulse_binding::stream::Direction;
use thiserror::Error;
use reqwest::header::CONTENT_TYPE;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use libpulse_simple_binding as pulse;
use reqwest::blocking;
use reqwest::blocking::Response;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use warp::Filter;
use warp::http::StatusCode;
use serde::Serialize;
use tokio::task::{JoinError, spawn, spawn_blocking};
use clap::Parser;

#[derive(Error, Debug)]
pub enum R357Error {
    #[error("Not mp3 stream")]
    NotMp3Stream,
    #[error("Bad metaint header")]
    BadMetaIntHeader,
    #[error("Connect error {0}")]
    ConnectError(#[from] reqwest::Error),
    #[error("PulseAudio error {0}")]
    PulseError(#[from] libpulse_binding::error::PAErr),
    #[error("Symphonia error: {0}")]
    DecodeError(#[from] symphonia::core::errors::Error),
    #[error("Join error: {0}")]
    JoinError(#[from] JoinError),
    #[error("Bad binding {0}")]
    BadBinding(#[from] AddrParseError),
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(short, default_value = "https://stream.radio357.pl")]
    url: String,
    #[arg(short, default_value = "0.0.0.0")]
    binding: String,
    #[arg(short, default_value_t = 6681)]
    port: u16,
}

#[derive(Serialize, Debug)]
struct PlayerState {
    playing: bool,
    song_title: Option<String>,
}

impl PlayerState {
    fn stop(&mut self) {
        self.playing = false;
        self.song_title = None;
    }

    fn start(&mut self) {
        self.playing = true;
        self.song_title = None;
    }
}

#[derive(Debug)]
enum Command {
    Start,
    Stop,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let args = Arc::new(args);

    let (tx, rx) = tokio::sync::mpsc::channel(8);

    let state = Arc::new(RwLock::new(PlayerState {
        playing: false,
        song_title: None,
    }));

    spawn(player_worker(rx, state.clone(), Arc::clone(&args)));

    let _ = start_http_server(tx, state, &args).await;
}

async fn handle_status(
    state: Arc<RwLock<PlayerState>>,
) -> Result<impl warp::Reply, Infallible> {
    let state = Arc::clone(&state);
    let state = state.read();
    let state= state.unwrap();
    let state = state.deref();
    Ok(warp::reply::json(state))
}

async fn handle_start(tx: Arc<Sender<Command>>) -> Result<impl warp::Reply, Infallible> {
    match tx.send(Command::Start).await {
        Ok(_) => Ok(StatusCode::ACCEPTED),
        Err(_) => Ok(StatusCode::INTERNAL_SERVER_ERROR)
    }

}

async fn handle_stop(tx: Arc<Sender<Command>>) -> Result<impl warp::Reply, warp::Rejection> {
    match tx.send(Command::Stop).await {
        Ok(_) => Ok(StatusCode::ACCEPTED),
        Err(_) => Ok(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn start_http_server(
    tx: Sender<Command>,
    state: Arc<RwLock<PlayerState>>,
    args: &Args,
) -> Result<(), R357Error> {
    let tx = Arc::new(tx);
    let tx1 = Arc::clone(&tx);
    let tx2 = Arc::clone(&tx);

    let status_route = warp::path("status")
        .and_then(move || handle_status(state.clone()));

    let start_route = warp::post().and(warp::path("start"))
        .and_then(move || handle_start(Arc::clone(&tx1)));

    let stop_route = warp::post().and(warp::path("stop"))
        .and_then(move || handle_stop(Arc::clone(&tx2)));

    let routes = status_route
        .or(start_route)
        .or(stop_route);

    let ipv4 = Ipv4Addr::from_str(&args.binding)?;
    let socket_addr = SocketAddrV4::new(ipv4, args.port);

    let _ = warp::serve(routes).run(socket_addr).await;

    println!("Http server started");

    Ok(())
}

async fn player_worker(
    mut rx: mpsc::Receiver<Command>,
    state: Arc<RwLock<PlayerState>>,
    args: Arc<Args>,
) {
    let stop_flag = Arc::new(RwLock::new(false));

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Start => {
                *stop_flag.write().unwrap() = false;
                spawn(play_stream(state.clone(), Arc::clone(&stop_flag), Arc::clone(&args)));
            },
            Command::Stop => {
                *stop_flag.write().unwrap() = true;
            }
        }
    }
}

async fn play_stream(
    state: Arc<RwLock<PlayerState>>,
    stop_flag: Arc<RwLock<bool>>,
    args: Arc<Args>,
) -> Result<(), R357Error> {
    println!("r357 session started");

    {
        let mut state = state.write().unwrap();
        state.start();
    }

    let state_to_update = Arc::clone(&state);

    let backoff = ExponentialBackoff::default();
    let notify = |err, dur| {
        println!("Retry error happened {} duration {:?}", err, dur);
    };

    let args = Arc::clone(&args);

    let play = || async {
        let state = Arc::clone(&state);
        let stop_flag = Arc::clone(&stop_flag);
        let args = Arc::clone(&args);

        let result = spawn_blocking(move || {
            play(state, stop_flag, Arc::clone(&args))
                .map_err(backoff::Error::transient)
        }).await;

        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(backoff::Error::transient(R357Error::JoinError(e)))
        }
    };

    let result = backoff::future::retry_notify(backoff, play, notify).await;

    // Make state stopped
    {
        let lock = state_to_update.write();
        lock.unwrap().stop();
    }

    println!("Stopped radio session");

    result
}

fn parse_metaint_header(response: &Response) -> Result<Option<usize>, R357Error> {
    let metaint = response.headers().get("Icy-Metaint");
    let metaint = if let Some(metaint) = metaint {
        metaint.to_str()
            .map_err(|_| R357Error::BadMetaIntHeader)
            .and_then(|s| s.parse::<usize>()
                .map(Some)
                .map_err(|_| R357Error::BadMetaIntHeader)
            )
    } else {
        Ok(None)
    };

    metaint
}

struct IcySource<R: Read> {
    inner: R,
    metaint: Option<usize>,
    remaining_audio: usize,
    state: Arc<RwLock<PlayerState>>,
}

impl<R: Read> IcySource<R> {
    fn new(inner: R, metaint: Option<usize>, state: Arc<RwLock<PlayerState>>) -> IcySource<R> {
        IcySource {
            inner,
            metaint,
            state,
            remaining_audio: metaint.unwrap_or(0),
        }
    }

    fn read_metaint(&mut self) -> Result<(), std::io::Error> {
        let mut metaint_size_buf = [0u8];
        self.inner.read_exact(&mut metaint_size_buf)?;

        let meta_len = metaint_size_buf[0] as usize * 16;

        if meta_len == 0 {
            return Ok(())
        }

        let mut buf = vec![0u8; meta_len];
        self.inner.read_exact(&mut buf)?;

        if let Some(title) = self.parse_title(&buf) {
            let lock = self.state.try_write();
            if let Ok(mut guard) = lock {
                let state = guard.deref_mut();
                if state.song_title.as_deref() != Some(&title) {
                    println!("{}", &title);
                    state.song_title = Some(title);
                }
            }
        }

        Ok(())
    }

    fn parse_title(&self, buf: &[u8]) -> Option<String> {
        let text = String::from_utf8_lossy(buf);
        let text = text.trim_matches('\0');

        for part in text.split(';') {
            if let Some(rest) = part.strip_prefix("StreamTitle='") {
                if let Some(end) = rest.find('\'') {
                    return Some(rest[..end].to_string());
                }
            }
        }

        None
    }
}

impl<R: Read> Read for IcySource<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        match self.metaint {
            None => self.inner.read(out),
            Some(metaint) => {
                let mut total = 0;

                while total < out.len() {
                    // Need to handle metadata?
                    if self.remaining_audio == 0 {
                        self.read_metaint()?;
                        self.remaining_audio = metaint;
                    }

                    let to_read = (out.len() - total).min(self.remaining_audio);
                    let n = self.inner.read(&mut out[total..total + to_read])?;

                    if n == 0 {
                        break;
                    }

                    self.remaining_audio -= n;
                    total += n;
                }

                Ok(total)
            },
        }
    }
}

impl<R: Read + Send> Seek for IcySource<R> {
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::from(NotSeekable))
    }
}

impl<R: Read + Send + Sync> MediaSource for IcySource<R> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn play(
    state: Arc<RwLock<PlayerState>>,
    stop_flag: Arc<RwLock<bool>>,
    args: Arc<Args>,
) -> Result<(), R357Error> {
    if *stop_flag.read().unwrap() {
        return Ok(())
    }

    let client = blocking::Client::new();
    let response = client.get(&args.url)
        .header("Icy-MetaData", "1")
        .send()?;
    let status = response.status();
    if !status.is_success() {
        eprintln!("Status: {}", status);
        return Err(R357Error::NotMp3Stream)
    }

    let content_type = response.headers().get(CONTENT_TYPE);
    if content_type.is_none_or(|hv| hv != "audio/mpeg") {
        eprintln!("Content-Type not supported {}", content_type.unwrap().to_str().unwrap());
        return Err(R357Error::NotMp3Stream)
    }

    let metaint = parse_metaint_header(&response)?;

    let source = IcySource::new(
        Box::new(response),
        metaint,
        state,
    );
    let mss = MediaSourceStream::new(
        Box::new(source),
        MediaSourceStreamOptions::default(),
    );

    let mut hint = Hint::new();
    hint.mime_type("audio/mpeg");

    let meta_opts = MetadataOptions::default();
    let format_opts = FormatOptions::default();

    let probe = symphonia::default::get_probe()
        .format(&hint, mss, &format_opts, &meta_opts)?;

    let mut format = probe.format;

    let track = format.tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL).unwrap();

    // Use the default options for the decoder.
    let dec_opts: DecoderOptions = DecoderOptions::default();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &dec_opts)?;

    let mut sample_buf: Option<SampleBuffer<i16>> = None;

    let spec = libpulse_binding::sample::Spec {
        format: libpulse_binding::sample::Format::S16le,
        channels: 2,
        rate: 44100,
    };

    let p = pulse::Simple::new(
        None,
        "R357",
        Direction::Playback,
        None,
        "Radio 357 Stream",
        &spec,
        None,
        None,
    )?;

    while !*stop_flag.read().unwrap() {
        let packet = format.next_packet()?;
        let decoded = decoder.decode(&packet)?;

        if sample_buf.is_none() {
            println!("Playback started");
            sample_buf = Some(
                SampleBuffer::<i16>::new(
                    decoded.capacity() as u64,
                    *decoded.spec(),
                )
            )
        }

        let buf = sample_buf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);

        let pcm: &[i16] = buf.samples();
        let pcm_u8: &[u8] = bytemuck::cast_slice(pcm);

        p.write(pcm_u8)?;
    }

    Ok(())
}

// #[cfg(test)]
// mod tests {
//     use std::io::{BufReader, Cursor, Read};
//     use crate::IcySource;
//
//     #[test]
//     fn read_2_out_of_4_metaint() {
//         let str = "ABCD";
//         let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));
//
//         let mut buf: [u8; 2] = [0u8; 2];
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 2);
//         assert_eq!(buf[0], 65u8);
//         assert_eq!(buf[1], 66u8);
//     }
//
//     #[test]
//     fn read_4_out_of_4_metaint() {
//         let str = "ABCD";
//         let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));
//
//         let mut buf: [u8; 4] = [0u8; 4];
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 4);
//         assert_eq!(buf[0], 65u8);
//         assert_eq!(buf[1], 66u8);
//         assert_eq!(buf[2], 67u8);
//         assert_eq!(buf[3], 68u8);
//     }
//
//     #[test]
//     fn read_4_out_of_2_metaint() {
//         let str = "AB";
//         let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));
//
//         let mut buf: [u8; 4] = [0u8; 4];
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 2);
//         assert_eq!(buf[0], 65u8);
//         assert_eq!(buf[1], 66u8);
//         assert_eq!(buf[2], 0u8);
//         assert_eq!(buf[3], 0u8);
//     }
//
//     #[test]
//     fn read_6_out_of_2_metaint() {
//         let str = "AB_AB_AB";
//         let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(2));
//
//         let mut buf: [u8; 6] = [0u8; 6];
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 6);
//         assert_eq!(buf[0], 65u8);
//         assert_eq!(buf[1], 66u8);
//         assert_eq!(buf[2], 65u8);
//         assert_eq!(buf[3], 66u8);
//         assert_eq!(buf[4], 65u8);
//         assert_eq!(buf[5], 66u8);
//     }
//
//
//     #[test]
//     fn read_3_out_of_4_metaint_3_times() {
//         let str = "ABCD_ABCD_ABCD_";
//         let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));
//
//         let mut buf: [u8; 3] = [0u8; 3];
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 3);
//         assert_eq!(buf[0], 65u8);
//         assert_eq!(buf[1], 66u8);
//         assert_eq!(buf[2], 67u8);
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 3);
//         assert_eq!(buf[0], 68u8);
//         assert_eq!(buf[1], 65u8);
//         assert_eq!(buf[2], 66u8);
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 3);
//         assert_eq!(buf[0], 67u8);
//         assert_eq!(buf[1], 68u8);
//         assert_eq!(buf[2], 65u8);
//     }
//
//     #[test]
//     fn read_5_out_of_4_metaint_3_times() {
//         let str = "ABCD_ABCD_ABCD_";
//         let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));
//
//         let mut buf: [u8; 5] = [0u8; 5];
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 5);
//         assert_eq!(buf[0], 65u8);
//         assert_eq!(buf[1], 66u8);
//         assert_eq!(buf[2], 67u8);
//         assert_eq!(buf[3], 68u8);
//         assert_eq!(buf[4], 65u8);
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 5);
//         assert_eq!(buf[0], 66u8);
//         assert_eq!(buf[1], 67u8);
//         assert_eq!(buf[2], 68u8);
//         assert_eq!(buf[3], 65u8);
//         assert_eq!(buf[4], 66u8);
//
//         let result = source.read(buf.as_mut_slice());
//         assert_eq!(result.is_ok(), true);
//         assert_eq!(result.unwrap(), 2);
//         assert_eq!(buf[0], 67u8);
//         assert_eq!(buf[1], 68u8);
//     }
// }
