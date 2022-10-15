# asound-conf-wizard

asound-conf-wizard is an interactive utility that generates a very simple `/etc/asound.conf`.

It is designed to be used on headless sytems that run bare ALSA.

It will **NOT** run on systems that have PulseAudio, Jack Audio or PipeWire installed. **That is by design.**

You should use those to configure audio if they are installed.

## Limitations

asound-conf-wizard is intentionally very simple and it generates a very basic `/etc/asound.conf` that (ideally) creates a default duplex device with full software conversion and mixing. If your use case is more niche and/or complex then that asound-conf-wizard is not for you.

asound-conf-wizard suports `hw:`, `hdmi:` `iec958:` ALSA PCMs, `U8` and depending on the platform either the `LE` or `BE` varants of `S16`, `S24_3`, `S24` and `S32` formats and any number of channels and sampling rates. Basically it supports what `dmix` and `dsnoop` both support.

## Usage

asound-conf-wizard requires write privileges to `/etc`.

Other then that, basically just run the binary and follow the prompts.

![screen-shot](https://github.com/JasonLG1979/asound-conf-wizard/blob/main/Screenshot.png)
## Building a Binary

### Install Rust
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Install Dependencies

Debian and Family:
``` 
sudo apt update && sudo apt install git libasound2-dev pkg-config
```

Fedora and Family:
```
sudo dnf install git alsa-lib-devel
```

### Clone the repo
```
git clone https://github.com/JasonLG1979/asound-conf-wizard.git
```

### Compile
```
cd asound-conf-wizard
```
```
cargo build --profile default
```
A binary named `awiz` will be in `./target/default/`

## Building a Deb

The .debs built target Debian Stable. Your mileage may vary on Debian derivatives.

They depend on:
* [libc6 (>= 2.31)](https://tracker.debian.org/pkg/libc6)
* [libasound2 (>= 1.2.4)](https://tracker.debian.org/pkg/libasound2)

### Build just for your Machines Architecture
#### Install Rust
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
#### Install cargo-deb
```
cargo install cargo-deb
```
#### Install Dependencies
``` 
sudo apt update && sudo apt install git libasound2-dev pkg-config
```
#### Clone the repo
```
git clone https://github.com/JasonLG1979/asound-conf-wizard.git
```
#### Build the deb
```
cd asound-conf-wizard
```
```
cargo-deb --profile default
```
A `asound-conf-wizard` .deb will be in `./target/debian/`

### Cross-Compile
#### Install Docker
Follow the [instructions here](https://docs.docker.com/engine/install/debian/) to install Docker on Debian.
#### Clone the repo
```
git clone https://github.com/JasonLG1979/asound-conf-wizard.git
```
#### Build the deb(s)
```
cd asound-conf-wizard
```
##### Build armhf, arm64, and amd64 .debs
```
make
```
##### Build armhf .deb
```
make armhf
```
##### Build arm64 .deb
```
make arm64
```
##### Build amd64 .deb
```
make amd64
```

The `asound-conf-wizard` .deb(s) will be in `./`
