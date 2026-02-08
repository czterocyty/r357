# r357

Rust daemon that listens to Polish internet [Radio 357](https://radio357.pl/)
and emits audio to PulseAudio sink.

This is very convenient on single board computers like popular Raspberrypi that
has audio card.

The daemon exposes REST HTTP API endpoint to start, stop and get current daemon status:

* `GET /status` -> daemon status
* `POST /start` -> starts playback
* `POST /stop` -> stops running playback

Also there is build-in backoff retry mechanism in case of playbard runtime
errors.

## Android r357 remote control application

I am planning to write simple Android application that allows to
connect to the daemon in home network and control playback as well
as display current played song name.

# Build

Install Rust toolchains - https://rust-lang.org/tools/install/

```bash
cargo build --release
```

## Cross compilation

TODO

# Configuration

Execute command line to know default settings:

```bash
cargo run -- --help
```

## Internal dependencies

* symphonia (for mp3 decoding)
* reqwest (for accessing radio http stream endpoint)
* backoff (to retry playback in case of runtime errors)
* libpulse (to play on audio device)
* warp (to manage radio playback)
* tokio (to intenally manage async operations)
* tracing (for logging and observability)

Also CPU SIMD instruction extension sets can be used.
