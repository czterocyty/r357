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

It is rather hard to compile application on RaspberryPi due to limited resources.

To allow cross-compilation on let's say amd64 Linux PC, build docker image. It is based on Debian Bookworm
image which should be compatible with Raspberry Pi 4 operating-system.

See individual instructions in Dockerfile comments.

Once build on PC system, copy release binary onto you SBC system, like Raspberry Pi, under `/usr/local/bin`.

# Configuration

Execute command line to know default settings:

```bash
cargo run -- --help
```

Current output:

```bash
Usage: r357 [OPTIONS]

Options:
  -u <URL>      [default: https://stream.radio357.pl]
  -b <BINDING>  [default: 0.0.0.0]
  -p <PORT>     [default: 6681]
  -h, --help    Print help
```

# `systemctl` installation

Use systemctl unit template from `systemd/r357.service` to install r357 as system-wide daemon.

See systemctl documentation.

## Internal dependencies

* symphonia (for mp3 decoding)
* reqwest (for accessing radio http stream endpoint)
* backoff (to retry playback in case of runtime errors)
* libpulse (to play on audio device)
* warp (to manage radio playback via HTTP REST endpoints)
* tokio (to internally manage async operations)
* tracing (for logging and observability)
