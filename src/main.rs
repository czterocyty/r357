use libpulse_binding::stream::Direction;
use thiserror::Error;
use reqwest::header::CONTENT_TYPE;
use reqwest::blocking::Response;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use libpulse_simple_binding as pulse;

#[derive(Error, Debug)]
pub enum R357Error {
    #[error("Not mp3 stream")]
    NotMp3Stream,
    #[error("Connect error")]
    ConnectError(#[from] reqwest::Error),
    #[error("PulseAudio error")]
    PulseError(#[from] libpulse_binding::error::PAErr),
    #[error("Symphonia error")]
    DecodeError(#[from] symphonia::core::errors::Error),
}

// #[tokio::main]
fn main() -> Result<(), R357Error> {
    println!("r357");

    let response = reqwest::blocking::get("https://stream.radio357.pl/")?;
    let status = response.status();
    println!("Status: {}", status);
    let content_type = response.headers().get(CONTENT_TYPE);
    println!("Header: {:?}", content_type);
    if content_type.is_none_or(|hv| hv != "audio/mpeg") {
        eprintln!("Content-Type not supported {}", content_type.unwrap().to_str().unwrap());
        return Err(R357Error::NotMp3Stream)
    }

    let _ = play(response);

    Ok(())
}

fn play(response: Response) -> Result<(), R357Error> {
    let source = ReadOnlySource::new(response);
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
    let dec_opts: DecoderOptions = Default::default();
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

    loop {
        let packet = format.next_packet()?;
        let decoded = decoder.decode(&packet)?;

        if sample_buf.is_none() {
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
}
