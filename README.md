# asound-conf-wizard

asound-conf-wizard is an interactive utility that generates a very simple `/etc/asound.conf`.

It is designed to be used on headless sytems that run bare ALSA.

It will **NOT** run on systems that have PulseAudio, Jack Audio or PipeWire installed. **That is by design.**

You should use those to configure audio if they are installed.

## Building

### Install Rust
```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Install Dependencies

Debian and Family:
``` 
sudo apt update && sudo apt install libasound2-dev pkg-config
```

Fedora and Family:
```
sudo dnf install alsa-lib-devel
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

## Usage

asound-conf-wizard requires write privileges to `/etc`.

Other then that, basically just run the binary and follow the prompts.

## Limitations

asound-conf-wizard is intentionally very simple and it generates a very basic `/etc/asound.conf` that (ideally) creates a default duplex device with full software conversion and mixing. If your use case is more niche and/or complex then that asound-conf-wizard is not for you.

asound-conf-wizard suports `hw:`, `hdmi:` `iec958:` ALSA PCMs, `U8` and depending on the platform either the `LE` or `BE` varants of `S16`, `S24_3`, `S24` and `S32` formats and any number of channels and sampling rates. Basically it supports what `dmix` and `dsnoop` both support. 
