use std::io::{Read, Seek, SeekFrom};
use std::io::ErrorKind::NotSeekable;
use backoff::{ExponentialBackoff, retry_notify, Error};
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

#[derive(Error, Debug)]
pub enum R357Error {
    #[error("Not mp3 stream")]
    NotMp3Stream,
    #[error("Connect error {0}")]
    ConnectError(#[from] reqwest::Error),
    #[error("PulseAudio error {0}")]
    PulseError(#[from] libpulse_binding::error::PAErr),
    #[error("Symphonia error: {0}")]
    DecodeError(#[from] symphonia::core::errors::Error),
}

fn main() -> Result<(), R357Error> {
    println!("r357 started");

    let backoff = ExponentialBackoff::default();
    let notify = |err, dur| {
        println!("Retry error happened {} duration {:?} sec", err, dur);
    };
    let play = || { play().map_err(|e| Error::transient(e)) };
    retry_notify(backoff, play, notify)
        .map_err(|err| {
            match err {
                Error::Permanent(e) => e,
                Error::Transient {
                    err,
                    retry_after: _
                } => err,
            }
        })
}

fn parse_metaint(response: &Response) -> Option<usize> {
    let metaint = response.headers().get("Icy-Metaint");
    let metaint: Option<usize> = if let Some(metaint) = metaint {
        metaint.to_str()
            .ok()
            .map_or(None, |s| s.parse::<usize>().ok())
    } else {
        None
    };

    println!("Metaint {:?}", metaint);

    metaint
}

struct IcySource<R: Read> {
    inner: R,
    metaint: Option<usize>,
    remaining_audio: usize,
    current_title: Option<String>,
    // skip_first: bool,
}

impl<R: Read> IcySource<R> {
    fn new(inner: R, metaint: Option<usize>) -> IcySource<R> {
        IcySource {
            inner,
            metaint,
            remaining_audio: if let Some(metaint) = metaint {
                metaint
            } else {
                0
            },
            current_title: None,
            // skip_first: true
        }
    }

    fn read_metaint(&mut self) -> Result<(), std::io::Error> {
        let mut metaint_size_buf = [0u8];
        self.inner.read_exact(&mut metaint_size_buf)?;

        println!("Size of metadata {}", metaint_size_buf[0] as u32);
        let meta_len = metaint_size_buf[0] as usize * 16;

        if meta_len == 0 {
            return Ok(())
        }

        let mut buf = vec![0u8; meta_len];
        self.inner.read_exact(&mut buf)?;

        if let Some(title) = self.parse_title(&buf) {
            if self.current_title.as_deref() != Some(&title) {
                self.current_title = Some(title);

                println!("Title {:?}", self.current_title);
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
                // println!("remaining_audio {}", self.remaining_audio);

                let mut total = 0;

                while total < out.len() {
                    // Need to handle metadata?
                    if self.remaining_audio == 0 {
                        self.read_metaint()?;
                        self.remaining_audio = metaint;
                    }

                    let to_read = (out.len() - total).min(self.remaining_audio);
                    // println!("to_read {}", to_read);
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
        return false;
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

fn play() -> Result<(), R357Error> {
    let client = blocking::Client::new();
    let response = client.get("https://stream.radio357.pl/")
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

    let metaint = parse_metaint(&response);

    println!("{:?}", response.headers());

    let source = IcySource::new(Box::new(response), metaint);
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

    loop {
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
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor, Read};
    use crate::IcySource;

    #[test]
    fn read_2_out_of_4_metaint() {
        let str = "ABCD";
        let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));

        let mut buf: [u8; 2] = [0u8; 2];

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 2);
        assert_eq!(buf[0], 65u8);
        assert_eq!(buf[1], 66u8);
    }

    #[test]
    fn read_4_out_of_4_metaint() {
        let str = "ABCD";
        let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));

        let mut buf: [u8; 4] = [0u8; 4];

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 4);
        assert_eq!(buf[0], 65u8);
        assert_eq!(buf[1], 66u8);
        assert_eq!(buf[2], 67u8);
        assert_eq!(buf[3], 68u8);
    }

    #[test]
    fn read_4_out_of_2_metaint() {
        let str = "AB";
        let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));

        let mut buf: [u8; 4] = [0u8; 4];

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 2);
        assert_eq!(buf[0], 65u8);
        assert_eq!(buf[1], 66u8);
        assert_eq!(buf[2], 0u8);
        assert_eq!(buf[3], 0u8);
    }

    #[test]
    fn read_6_out_of_2_metaint() {
        let str = "AB_AB_AB";
        let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(2));

        let mut buf: [u8; 6] = [0u8; 6];

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 6);
        assert_eq!(buf[0], 65u8);
        assert_eq!(buf[1], 66u8);
        assert_eq!(buf[2], 65u8);
        assert_eq!(buf[3], 66u8);
        assert_eq!(buf[4], 65u8);
        assert_eq!(buf[5], 66u8);
    }


    #[test]
    fn read_3_out_of_4_metaint_3_times() {
        let str = "ABCD_ABCD_ABCD_";
        let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));

        let mut buf: [u8; 3] = [0u8; 3];

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 3);
        assert_eq!(buf[0], 65u8);
        assert_eq!(buf[1], 66u8);
        assert_eq!(buf[2], 67u8);

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 3);
        assert_eq!(buf[0], 68u8);
        assert_eq!(buf[1], 65u8);
        assert_eq!(buf[2], 66u8);

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 3);
        assert_eq!(buf[0], 67u8);
        assert_eq!(buf[1], 68u8);
        assert_eq!(buf[2], 65u8);
    }

    #[test]
    fn read_5_out_of_4_metaint_3_times() {
        let str = "ABCD_ABCD_ABCD_";
        let mut source = IcySource::new(Box::new(BufReader::new(Cursor::new(str))), Some(4));

        let mut buf: [u8; 5] = [0u8; 5];

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 5);
        assert_eq!(buf[0], 65u8);
        assert_eq!(buf[1], 66u8);
        assert_eq!(buf[2], 67u8);
        assert_eq!(buf[3], 68u8);
        assert_eq!(buf[4], 65u8);

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 5);
        assert_eq!(buf[0], 66u8);
        assert_eq!(buf[1], 67u8);
        assert_eq!(buf[2], 68u8);
        assert_eq!(buf[3], 65u8);
        assert_eq!(buf[4], 66u8);

        let result = source.read(buf.as_mut_slice());
        assert_eq!(result.is_ok(), true);
        assert_eq!(result.unwrap(), 2);
        assert_eq!(buf[0], 67u8);
        assert_eq!(buf[1], 68u8);
    }
}
