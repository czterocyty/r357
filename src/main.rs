use backoff::ExponentialBackoffBuilder;
use backoff::future::{Retry, Sleeper};
use clap::Parser;
use if_addrs::IfAddr;
use libpulse_binding::stream::Direction;
use libpulse_simple_binding as pulse;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use reqwest::blocking;
use reqwest::blocking::Response;
use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use std::convert::Infallible;
use std::env;
use std::io::ErrorKind::NotSeekable;
use std::io::{Read, Seek, SeekFrom};
use std::net::{AddrParseError, IpAddr, Ipv4Addr, SocketAddrV4};
use std::ops::{Deref, DerefMut};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use thiserror::Error;
use tokio::select;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;
use tokio::task::{JoinError, spawn, spawn_blocking};
use tokio::time::Sleep;
use tokio_util::sync::CancellationToken;
use tracing::instrument::Instrumented;
use tracing::{Instrument, Level, debug, info, instrument, warn};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};
use tracing_subscriber::{Registry, prelude::*};
use warp::Filter;
use warp::http::StatusCode;

#[derive(Error, Debug)]
pub enum R357Error {
    #[error("Not mp3 stream")]
    NotMp3Stream,
    #[error("Bad metaint header")]
    BadMetaIntHeader,
    #[error("Connect error: {0}")]
    ConnectError(#[from] reqwest::Error),
    #[error("PulseAudio error: {0}")]
    PulseError(#[from] libpulse_binding::error::PAErr),
    #[error("Symphonia error: {0}")]
    DecodeError(#[from] symphonia::core::errors::Error),
    #[error("Join error: {0}")]
    JoinError(#[from] JoinError),
    #[error("Bad binding: {0}")]
    BadBinding(#[from] AddrParseError),
    #[error("Failure of service discovery {0}")]
    ServiceDiscoveryFailure(#[from] mdns_sd::Error),
    #[error("Hostname not given")]
    NoHostname,
    #[error("Cannot get ifaces: {0}")]
    IfaceError(std::io::Error),
    #[error("Other failure")]
    Other,
}

#[derive(Debug, Parser)]
struct Args {
    #[arg(short, default_value = "https://stream.radio357.pl")]
    url: String,
    #[arg(short, default_value = "0.0.0.0")]
    binding: String,
    #[arg(short, default_value_t = 6681)]
    port: u16,
    #[arg(long, default_value_t = false, help = "Disables mDNS advertisement")]
    no_mdns: bool,
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

fn is_under_systemd() -> bool {
    env::var("INVOCATION_ID").is_ok()
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error,r357=debug"));

    let mut layers = Vec::new();

    let journald_layer = tracing_journald::layer();
    if is_under_systemd() && journald_layer.is_ok() {
        info!("Enabling journald logging");
        layers.push(journald_layer.unwrap().boxed());
    } else {
        info!("No journald logging, using stdout");

        let stdout_layer = tracing_subscriber::fmt::layer()
            .with_target(true)
            .with_thread_ids(true)
            .with_thread_names(true)
            .with_line_number(true)
            .with_file(true)
            // .with_span_events(FmtSpan::FULL)
            .boxed();

        layers.push(stdout_layer);
    }

    Registry::default().with(filter).with(layers).init();
}

#[tokio::main]
async fn main() {
    init_tracing();

    let args = Args::parse();
    let args: Arc<Args> = Arc::new(args);

    let (tx, rx) = mpsc::channel(8);

    let state = Arc::new(RwLock::new(PlayerState {
        playing: false,
        song_title: None,
    }));

    spawn(player_worker(rx, state.clone(), Arc::clone(&args)));

    let server = start_http_server(tx, state, Arc::clone(&args));

    if !args.no_mdns {
        spawn_blocking(move || {
            let _ = register_service(Arc::clone(&args));
        });
    }

    let _ = server.await;
}

fn find_hostname() -> Result<String, R357Error> {
    match hostname::get() {
        Err(_) => Err(R357Error::NoHostname),
        Ok(hostname) => hostname
            .to_str()
            .ok_or(R357Error::NoHostname)
            .map(|s| s.to_string()),
    }
}

fn find_ips(args: Arc<Args>) -> Result<Vec<IpAddr>, R357Error> {
    let ifaces = if_addrs::get_if_addrs().map_err(R357Error::IfaceError)?;

    let binding: Ipv4Addr = Ipv4Addr::from_str(&args.binding)?;

    let set = ifaces
        .iter()
        .filter_map(|iface| match &iface.addr {
            IfAddr::V4(addr) => {
                if binding == Ipv4Addr::new(0, 0, 0, 0) || addr.ip == binding {
                    Some(IpAddr::V4(addr.ip))
                } else {
                    None
                }
            }
            IfAddr::V6(_) => None,
        })
        .collect::<Vec<IpAddr>>();

    Ok(set)
}

fn register_service(args: Arc<Args>) -> Result<(), R357Error> {
    let mdns = ServiceDaemon::new()?;

    let instance_name = &env::var("SYSTEMD_UNIT").unwrap_or("instance".to_string());
    debug!("mDNS instance name {}", instance_name);

    let service = ServiceInfo::new(
        "_r357_._tcp.local.",
        instance_name,
        &find_hostname()?,
        find_ips(Arc::clone(&args))?.as_slice(),
        args.port,
        None,
    )?;

    let _result = mdns.register(service);

    info!("Service registered in mDNS");

    Ok(())
}

async fn handle_status(state: Arc<RwLock<PlayerState>>) -> Result<impl warp::Reply, Infallible> {
    let state = Arc::clone(&state);
    let state = state.read();
    let state = state.unwrap();
    let state = state.deref();
    Ok(warp::reply::json(state))
}

async fn handle_start(tx: Arc<Sender<Command>>) -> Result<impl warp::Reply, Infallible> {
    match tx.send(Command::Start).await {
        Ok(_) => Ok(StatusCode::ACCEPTED),
        Err(_) => Ok(StatusCode::INTERNAL_SERVER_ERROR),
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
    args: Arc<Args>,
) -> Result<(), R357Error> {
    let tx = Arc::new(tx);
    let tx1 = Arc::clone(&tx);
    let tx2 = Arc::clone(&tx);

    let logger = warp::log("r357");

    let status_route = warp::get()
        .and(warp::path("status"))
        .and_then(move || handle_status(state.clone()));

    let start_route = warp::post()
        .and(warp::path("start"))
        .and_then(move || handle_start(Arc::clone(&tx1)));

    let stop_route = warp::post()
        .and(warp::path("stop"))
        .and_then(move || handle_stop(Arc::clone(&tx2)));

    let routes = status_route.or(start_route).or(stop_route).with(logger);

    let ipv4 = Ipv4Addr::from_str(&args.binding)?;
    let socket_addr = SocketAddrV4::new(ipv4, args.port);

    let running = warp::serve(routes).run(socket_addr);

    info!("Http server started at {}:{}", ipv4, args.port);

    running.await;

    Ok(())
}

async fn player_worker(
    mut rx: mpsc::Receiver<Command>,
    state: Arc<RwLock<PlayerState>>,
    args: Arc<Args>,
) {
    let mut stored_token: Option<CancellationToken> = None;

    while let Some(cmd) = rx.recv().await {
        match cmd {
            Command::Start => {
                let token = CancellationToken::new();
                let token_child = token.clone();

                stored_token = Some(token);

                spawn(play_stream(state.clone(), Arc::clone(&args), token_child));
            }
            Command::Stop => {
                if let Some(token) = stored_token {
                    info!("Cancelling");
                    token.cancel();
                    stored_token = None;
                }
            }
        }
    }
}

fn create_sleeper() -> impl Sleeper {
    InstrumentedSleeper
}

struct InstrumentedSleeper;

impl Sleeper for InstrumentedSleeper {
    type Sleep = Instrumented<Sleep>;
    fn sleep(&self, dur: Duration) -> Self::Sleep {
        tokio::time::sleep(dur).instrument(tracing::span!(
            Level::DEBUG,
            "sleeper",
            sleep = dur.as_secs_f64()
        ))
    }
}

type RetryableResult = Result<(), backoff::Error<R357Error>>;

struct RetryablePlayback {
    state: Arc<RwLock<PlayerState>>,
    args: Arc<Args>,
    cancel_token: CancellationToken,
}

impl RetryablePlayback {
    fn new(
        state: Arc<RwLock<PlayerState>>,
        args: Arc<Args>,
        cancel_token: CancellationToken,
    ) -> Self {
        RetryablePlayback {
            state,
            args,
            cancel_token,
        }
    }

    fn playback<'a>(
        &'a mut self,
    ) -> impl FnMut() -> Pin<Box<dyn Future<Output = RetryableResult> + Send + 'a>> + 'a {
        || {
            Box::pin(async {
                let state = Arc::clone(&self.state);
                let args = Arc::clone(&self.args);
                let cancel_token = self.cancel_token.clone();

                let result: Result<Result<(), R357Error>, JoinError> = spawn_blocking(move || {
                    let ret = play_once(state, args, cancel_token);
                    info!("Blocked play_once result {:?}", ret);
                    ret
                })
                .await;
                info!("Result from play {:?}", result);

                let backoff_result = to_backoff_error(result);
                info!("Backoff result from play {:?}", backoff_result);
                backoff_result
            })
        }
    }
}

fn to_backoff_error(result: Result<Result<(), R357Error>, JoinError>) -> RetryableResult {
    match result {
        Ok(Ok(x)) => Ok(x),
        Ok(Err(e)) => Err(backoff::Error::transient(e)),
        Err(e) => Err(backoff::Error::transient(R357Error::JoinError(e))),
    }
}

#[instrument(level = "debug")]
async fn play_stream(
    state: Arc<RwLock<PlayerState>>,
    args: Arc<Args>,
    cancel_token: CancellationToken,
) -> Result<(), R357Error> {
    info!("r357 session started");

    {
        let mut state = state.write().unwrap();
        state.start();
    }

    let state_to_update = Arc::clone(&state);
    let args = Arc::clone(&args);

    let backoff = ExponentialBackoffBuilder::new()
        .with_max_elapsed_time(Some(Duration::from_secs(20 * 60)))
        .build();
    let notify = |err, dur| {
        warn!("Retry error happened {} duration {:?}", err, dur);
    };

    let mut retryable_playback =
        RetryablePlayback::new(Arc::clone(&state), Arc::clone(&args), cancel_token.clone());

    let sleeper = create_sleeper();
    let retry = Retry::new(sleeper, backoff, notify, retryable_playback.playback());
    let result: Result<(), R357Error> = select! {
        result = retry => result,
        _ = cancel_token.cancelled() => {
            info!("Cancelling playback");
            Ok(())
        }
    };

    // Make state stopped
    {
        let lock = state_to_update.write();
        lock.unwrap().stop();
    }

    match result {
        Ok(()) => info!("Stopped radio session"),
        Err(ref e) => warn!("Stopped radio session, due to error {e}"),
    };

    result
}

fn parse_metaint_header(response: &Response) -> Result<Option<usize>, R357Error> {
    let metaint = response.headers().get("Icy-Metaint");
    let metaint = if let Some(metaint) = metaint {
        metaint
            .to_str()
            .map_err(|_| R357Error::BadMetaIntHeader)
            .and_then(|s| {
                s.parse::<usize>()
                    .map(Some)
                    .map_err(|_| R357Error::BadMetaIntHeader)
            })
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
            return Ok(());
        }

        let mut buf = vec![0u8; meta_len];
        self.inner.read_exact(&mut buf)?;

        if let Some(title) = self.parse_title(&buf) {
            let lock = self.state.try_write();
            if let Ok(mut guard) = lock {
                let state = guard.deref_mut();
                if state.song_title.as_deref() != Some(&title) {
                    info!("{}", &title);
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
            }
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

#[instrument(level = "debug")]
fn play_once(
    state: Arc<RwLock<PlayerState>>,
    args: Arc<Args>,
    cancel_token: CancellationToken,
) -> Result<(), R357Error> {
    if cancel_token.is_cancelled() {
        return Ok(());
    }

    let client = blocking::Client::new();
    let response = client.get(&args.url).header("Icy-MetaData", "1").send()?;
    let status = response.status();
    if !status.is_success() {
        warn!("Status: {}", status);
        return Err(R357Error::NotMp3Stream);
    }

    let content_type = response.headers().get(CONTENT_TYPE);
    if content_type.is_none_or(|hv| hv != "audio/mpeg") {
        warn!(
            "Content-Type not supported {}",
            content_type.unwrap().to_str().unwrap()
        );
        return Err(R357Error::NotMp3Stream);
    }

    let metaint = parse_metaint_header(&response)?;

    let source = IcySource::new(Box::new(response), metaint, state);
    let stream_options = MediaSourceStreamOptions {
        buffer_len: 256 * 1024,
    };
    let mss = MediaSourceStream::new(Box::new(source), stream_options);

    let mut hint = Hint::new();
    hint.mime_type("audio/mpeg");

    let meta_opts = MetadataOptions::default();
    let format_opts = FormatOptions::default();

    let probe = symphonia::default::get_probe().format(&hint, mss, &format_opts, &meta_opts)?;

    let mut format = probe.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .unwrap();

    // Use the default options for the decoder.
    let dec_opts: DecoderOptions = DecoderOptions::default();
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &dec_opts)?;

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

    while !cancel_token.is_cancelled() {
        let packet = format.next_packet()?;
        let decoded = decoder.decode(&packet)?;

        if sample_buf.is_none() {
            info!("Playback started");
            sample_buf = Some(SampleBuffer::<i16>::new(
                decoded.capacity() as u64,
                *decoded.spec(),
            ))
        }

        let buf = sample_buf.as_mut().unwrap();
        buf.copy_interleaved_ref(decoded);

        let pcm: &[i16] = buf.samples();
        let pcm_u8: &[u8] = bytemuck::cast_slice(pcm);

        p.write(pcm_u8)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use backoff::{ExponentialBackoff, ExponentialBackoffBuilder};
    use std::cell::RefCell;
    use std::time::Instant;

    struct MockedPlay {
        calls: u32,
    }

    impl MockedPlay {
        fn new() -> Self {
            MockedPlay { calls: 0u32 }
        }

        async fn execute(
            &mut self,
            result: Result<(), R357Error>,
        ) -> Result<(), backoff::Error<R357Error>> {
            self.calls += 1;

            let result = spawn_blocking(move || result).await;

            to_backoff_error(result)
        }
    }

    #[tokio::test]
    async fn retry_ok() {
        let backoff = ExponentialBackoff::default();
        let notify = |err, dur| {
            println!("Retry error happened {} duration {:?}", err, dur);
        };

        let mocked_play = MockedPlay::new();
        let mocked_play = RefCell::new(mocked_play);

        let play = || async {
            let mut mocked_play = mocked_play.borrow_mut();
            let result = mocked_play.execute(Ok(()));
            result.await
        };

        let result = backoff::future::retry_notify(backoff, play, notify).await;

        assert!(result.is_ok());
        assert_eq!(1, mocked_play.borrow().calls);
    }

    #[tokio::test]
    async fn retry_error() {
        let backoff = ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(Duration::from_secs(2u64)))
            .build();
        let notify = |err, dur| {
            println!("Retry error happened {} duration {:?}", err, dur);
        };

        let mocked_play = MockedPlay::new();
        let mocked_play = RefCell::new(mocked_play);

        let func = || async {
            let mut mocked_play = mocked_play.borrow_mut();
            mocked_play.execute(Err(R357Error::Other)).await
        };

        let start_time = Instant::now();
        let result = backoff::future::retry_notify(backoff, func, notify).await;
        let duration = start_time.elapsed();

        assert!(result.is_err());
        assert!(Duration::from_secs(2u64) >= duration);
        assert!(duration <= Duration::from_secs(3u64));
        assert!(mocked_play.borrow().calls > 1);
    }

    #[tokio::test]
    async fn retry_error_using_retry() {
        let backoff = ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(Duration::from_secs(2u64)))
            .build();
        let notify = |err, dur| {
            println!("Retry error happened {} duration {:?}", err, dur);
        };

        let mocked_play = MockedPlay::new();
        let mocked_play = RefCell::new(mocked_play);

        let func = || async {
            let mut mocked_play = mocked_play.borrow_mut();
            mocked_play.execute(Err(R357Error::Other)).await
        };

        let start_time = Instant::now();
        let sleeper = create_sleeper();
        let result = Retry::new(sleeper, backoff, notify, func).await;
        let duration = start_time.elapsed();

        assert!(result.is_err());
        assert!(Duration::from_secs(2u64) >= duration);
        assert!(duration <= Duration::from_secs(3u64));
        assert!(mocked_play.borrow().calls > 1);
    }

    #[tokio::test]
    async fn retry_error_using_select() {
        let backoff = ExponentialBackoffBuilder::new()
            .with_max_elapsed_time(Some(Duration::from_secs(2u64)))
            .build();
        let notify = |err, dur| {
            println!("Retry error happened {} duration {:?}", err, dur);
        };

        let mocked_play = MockedPlay::new();
        let mocked_play = RefCell::new(mocked_play);

        let func = || async {
            let mut mocked_play = mocked_play.borrow_mut();
            mocked_play.execute(Err(R357Error::Other)).await
        };

        let start_time = Instant::now();
        let sleeper = create_sleeper();
        let retry = Retry::new(sleeper, backoff, notify, func);
        let result = select! {
            result = retry => result,
        };
        let duration = start_time.elapsed();

        assert!(result.is_err());
        assert!(Duration::from_secs(2u64) >= duration);
        assert!(duration <= Duration::from_secs(3u64));
        assert!(mocked_play.borrow().calls > 1);
    }
}
