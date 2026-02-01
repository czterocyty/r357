use thiserror::Error;
use reqwest::header::CONTENT_TYPE;
use futures_util::StreamExt;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

#[derive(Error, Debug)]
pub enum R357Error {
    #[error("Not mp3 stream")]
    NotMp3Stream,
    #[error("Connect error")]
    ConnectError(#[from] reqwest::Error),
}

#[tokio::main]
async fn main() -> Result<(), R357Error> {
    println!("r357");

    let response = reqwest::get("https://stream.radio357.pl/").await?;
    let status = response.status();
    println!("Status: {}", status);
    let content_type = response.headers().get(CONTENT_TYPE);
    println!("Header: {:?}", content_type);
    match content_type {
        Some(header_value) if header_value == "audio/mpeg" => {
            let mut stream = response.bytes_stream();

            let mut buf = Vec::with_capacity(8);

            while let Some(chunk) = stream.next().await {
                let chunk = chunk?;
                let needed = 8 - buf.len();
                buf.extend_from_slice(&chunk[..chunk.len().min(needed)]);
                if buf.len() == 8 {
                    break;
                }
            }

            println!("{:X?}", buf);



            Ok(())
        }
        _ => {
            eprintln!("Content-Type not supported {}", content_type.unwrap().to_str().unwrap());
            Err(R357Error::NotMp3Stream)
        },
    }
    // let bytes = request.bytes().await?;
    // let bytes = bytes;

    // println!("Bytes {:x?}", bytes);

}

async fn play() {
    let mut hint = Hint::new();
    hint.mime_type("audio/mpeg");

    let media_source = todo!();
    let mss: MediaSourceStream = todo!();

    let meta_opts = MetadataOptions::default();
    let format_opts = FormatOptions::default();

    let probe = symphonia::default::get_probe()
        .format(&hint, mss, &format_opts, &meta_opts)
        .expect("unsupported format");

    let mut format = probe.format;

    let track = format.tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .expect("no supported audio tracks");

    // Use the default options for the decoder.
    let dec_opts: DecoderOptions = Default::default();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &dec_opts)
        .expect("decoder is needed");

    loop {
        let packet = match format.next_packet() {
            Ok(packet) => packet,
            _ => panic!("panic"),
        };

        match decoder.decode(&packet) {
            Ok(decoded) => todo!(),
            _ => panic!("panic"),
        }
    }
}
