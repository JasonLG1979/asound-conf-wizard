use std::{
    fmt, fs,
    fs::File,
    io::{stdin, stdout, Write},
    ops::RangeInclusive,
    process::exit,
    sync::mpsc,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alsa::{
    device_name::HintIter,
    pcm::{Format, HwParams, PCM},
    Direction, ValueOr,
};

use comfy_table::{
    modifiers::UTF8_SOLID_INNER_BORDERS, presets::UTF8_FULL, Attribute, Cell, ContentArrangement,
    Table,
};

use colored::*;
use glob::glob;
use which::which;

const FORMATS: &[AudioFormat] = &[
    AudioFormat::U8,
    AudioFormat::S16,
    AudioFormat::S24_3,
    AudioFormat::S24,
    AudioFormat::S32,
];

const RATES: &[u32] = &[
    8000, 11025, 16000, 22050, 44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000, 705600,
    768000,
];

const CONFLICTING_SOFTWARE: [[&str; 2]; 3] = [
    ["pulseaudio", "PulseAudio"],
    ["pipewire", "PipeWire"],
    ["jackd", "JACK Audio"],
];

const CHANNELS: RangeInclusive<u32> = 1..=12;

const ASOUND_FILE_PATH: &str = "/etc/asound.conf";
const DUMMY_FILE_PATH_TEMPLATE: &str = "/etc/foobarbaz{now}";
const BACKUP_FILE_PATH_TEMPLATE: &str = "/etc/asound.conf.bak{now}";

const CONVERTERS_GLOB_PATH: &str = "/usr/lib/*/alsa-lib/libasound_module_rate_*";
const CONVERTERS_PREFIX: &str = "/libasound_module_rate_";

// dmix and dsnoop are basically mirror images of each other.
// See:
// https://github.com/alsa-project/alsa-lib/blob/master/src/conf/pcm/dmix.conf
// https://github.com/alsa-project/alsa-lib/blob/master/src/conf/pcm/dsnoop.conf
const PLAYBACK_CAPTURE_TEMPLATE: &str = "\
pcm.{playback_capture} {
    type {dmix_dsnoop}
    ipc_key {
        @func refer
        name defaults.pcm.ipc_key
    }
    ipc_gid {
        @func refer
        name defaults.pcm.ipc_gid
    }
    ipc_perm {
        @func refer
        name defaults.pcm.ipc_perm
    }
    tstamp_type {
        @func refer
        name defaults.pcm.tstamp_type
    }
    slave {
        pcm {
            type hw
            card {card}
            device {device}
            subdevice {sub_device}
        }
        channels {channels}
        rate {rate}
        format {fmt}
    }
}";

// See:
// https://github.com/alsa-project/alsa-lib/blob/master/src/pcm/pcm_asym.c#L20
const ASYM_DEFAULT_TEMPLATE: &str = "\
pcm.!default {
    type asym
    capture.pcm {
        type plug
        slave.pcm {input_pcm}
    }
    playback.pcm {
        type plug
        slave.pcm {output_pcm}
    }
}";

const DIGITAL_OUPUT_TEMPLATE: &str = "\
defaults.pcm.dmix.rate {rate}
defaults.pcm.dmix.format {fmt}
defaults.pcm.dmix.channels {channels}";

const DEFAULT_CONTROL_TEMPLATE: &str = "\
ctl.!default {
    type hw
    card {card}
}";

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
enum AudioFormat {
    U8,
    S16,
    S24_3,
    S24,
    S32,
}

// We only care about the formats Dmix understands.
impl From<AudioFormat> for Format {
    fn from(f: AudioFormat) -> Format {
        use AudioFormat::*;
        match f {
            U8 => Format::U8,
            S16 => Format::s16(),
            S24_3 => Format::s24_3(),
            S24 => Format::s24(),
            S32 => Format::s32(),
        }
    }
}

impl fmt::Display for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use AudioFormat::*;
        match *self {
            U8 => write!(f, "U8"),
            #[cfg(target_endian = "little")]
            S16 => write!(f, "S16_LE"),
            #[cfg(target_endian = "big")]
            S16 => write!(f, "S16_BE"),
            #[cfg(target_endian = "little")]
            S24_3 => write!(f, "S24_3LE"),
            #[cfg(target_endian = "big")]
            S24_3 => write!(f, "S24_3BE"),
            #[cfg(target_endian = "little")]
            S24 => write!(f, "S24_LE"),
            #[cfg(target_endian = "big")]
            S24 => write!(f, "S24_BE"),
            #[cfg(target_endian = "little")]
            S32 => write!(f, "S32_LE"),
            #[cfg(target_endian = "big")]
            S32 => write!(f, "S32_BE"),
        }
    }
}

#[derive(Debug, Clone)]
enum WorkerJob {
    GetPcm {
        name: String,
        card_name: String,
        description: String,
        direction: Direction,
    },
    Done,
}

#[derive(Debug)]
struct ThreadManager {
    workers: Vec<ThreadWorker>,
}

impl ThreadManager {
    pub fn new() -> Self {
        // The ThreadManager's job is to keep track
        // of works, give them jobs and make sure that
        // there's only ever one worker per card.
        Self {
            workers: Vec::with_capacity(20),
        }
    }

    pub fn add_job(&mut self, name: &str, description: &str, direction: Direction) {
        let card_name = name[name.find('=').unwrap_or(0)..name.find(',').unwrap_or(name.len())]
            .replace('=', "")
            .trim()
            .to_string();

        let job_sent = {
            let mut job_sent = false;
            let mut bad_worker = false;

            for worker in self.workers.iter_mut() {
                if worker.card_name == card_name {
                    job_sent = worker.add_job(name, &card_name, description, direction);
                    bad_worker = !job_sent;
                }
            }

            if bad_worker {
                // Drop the worker if it exists but add_job fails.
                self.workers.retain(|worker| worker.card_name != card_name);
            }

            job_sent
        };

        if !job_sent {
            let mut worker = ThreadWorker::new(card_name.clone());

            if worker.add_job(name, &card_name, description, direction) {
                self.workers.push(worker);
            }
        }
    }

    pub fn get_pcms(&mut self) -> (Vec<AlsaPcm>, Vec<AlsaPcm>) {
        let mut playback_pcms = Vec::with_capacity(20);
        let mut capture_pcms = Vec::with_capacity(20);

        for worker in self.workers.iter_mut() {
            if let Some((p_pcms, c_pcms)) = worker.get_pcms() {
                playback_pcms.extend(p_pcms);
                capture_pcms.extend(c_pcms);
            }
        }

        (playback_pcms, capture_pcms)
    }
}

type WorkerHandle = thread::JoinHandle<Option<(Vec<AlsaPcm>, Vec<AlsaPcm>)>>;

#[derive(Debug)]
struct ThreadWorker {
    pub card_name: String,
    thread_handle: Option<WorkerHandle>,
    job_sender: Option<mpsc::Sender<WorkerJob>>,
}

impl ThreadWorker {
    pub fn new(card_name: String) -> Self {
        // Workers handle all jobs for one card in a
        // synchronous manner to avoid concurrently
        // opening the same card which for cards
        // that lack some sort of builtin mixer will fail.
        let (job_sender, job_receiver) = mpsc::channel();

        let thread_handle = Some(thread::spawn(move || {
            let mut playback_pcms = Vec::with_capacity(20);
            let mut capture_pcms = Vec::with_capacity(20);

            loop {
                match job_receiver.recv() {
                    Err(_) => return None,
                    Ok(job) => match job {
                        WorkerJob::Done => return Some((playback_pcms, capture_pcms)),
                        WorkerJob::GetPcm {
                            name,
                            card_name,
                            description,
                            direction,
                        } => match direction {
                            Direction::Playback => {
                                let alsa_pcm =
                                    AlsaPcm::new(&name, &card_name, &description, direction);

                                if let Some(alsa_pcm) = alsa_pcm {
                                    playback_pcms.push(alsa_pcm);
                                }
                            }
                            Direction::Capture => {
                                let alsa_pcm =
                                    AlsaPcm::new(&name, &card_name, &description, direction);

                                if let Some(alsa_pcm) = alsa_pcm {
                                    capture_pcms.push(alsa_pcm);
                                }
                            }
                        },
                    },
                }
            }
        }));

        Self {
            card_name,
            thread_handle,
            job_sender: Some(job_sender),
        }
    }

    pub fn add_job(
        &mut self,
        name: &str,
        card_name: &str,
        description: &str,
        direction: Direction,
    ) -> bool {
        if let Some(sender) = self.job_sender.as_mut() {
            let job = WorkerJob::GetPcm {
                name: name.to_string(),
                card_name: card_name.to_string(),
                description: description.to_string(),
                direction,
            };

            return sender.send(job).is_ok();
        }

        false
    }

    pub fn get_pcms(&mut self) -> Option<(Vec<AlsaPcm>, Vec<AlsaPcm>)> {
        if let Some(sender) = self.job_sender.take() {
            // Send a WorkerJob::Done to break the loop
            // in the worker thread so it joins the main thread
            // and returns it's results.
            let _ = sender.send(WorkerJob::Done);
        }

        if let Some(handle) = self.thread_handle.take() {
            if let Ok(pcms) = handle.join() {
                return pcms;
            }
        }

        None
    }
}

impl Drop for ThreadWorker {
    fn drop(&mut self) {
        if let Some(sender) = self.job_sender.take() {
            let _ = sender.send(WorkerJob::Done);
        }

        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

#[derive(Debug, Clone)]
struct ValidConfiguration {
    pub name: String,
    pub description: String,
    pub direction: Direction,
    pub is_real_hw: bool,
    pub card_name: String,
    pub device_number: u32,
    pub sub_device_number: u32,
    pub format: AudioFormat,
    pub rate: u32,
    pub channels: u32,
}

impl ValidConfiguration {
    pub fn new(pcm: AlsaPcm, format: AudioFormat, rate: u32, channels: u32) -> Self {
        Self {
            name: pcm.name,
            description: pcm.description,
            direction: pcm.direction,
            is_real_hw: pcm.is_real_hw,
            card_name: pcm.card_name,
            device_number: pcm.device_number,
            sub_device_number: pcm.sub_device_number,
            format,
            rate,
            channels,
        }
    }
}

#[derive(Debug, Clone)]
struct AlsaPcm {
    pub name: String,
    pub description: String,
    pub direction: Direction,
    pub is_real_hw: bool,
    pub software_mixable: bool,
    pub has_mixer: bool,
    pub card_name: String,
    pub device_number: u32,
    pub sub_device_number: u32,
    pub formats: Vec<AudioFormat>,
    pub rates: Vec<u32>,
    pub channels: Vec<u32>,
    pub valid_configurations: Vec<ValidConfiguration>,
}

impl AlsaPcm {
    pub fn new(
        name: &str,
        card_name: &str,
        description: &str,
        direction: Direction,
    ) -> Option<Self> {
        let description = description[description.find(',').unwrap_or(0)..]
            .replace(',', "")
            .trim()
            .to_string();

        let vdevice_number = name[name.find("DEV=").unwrap_or(0)..]
            .replace("DEV=", "")
            .trim()
            .parse::<u32>()
            .unwrap_or_default();

        let software_mixable = name.starts_with("hw:");

        let mut is_real_hw = name.starts_with("hw:");

        let has_mixer = Self::has_mixer(name, direction);

        let mut device_number: u32 = 0;
        let mut sub_device_number: u32 = 0;
        let mut formats = Vec::with_capacity(5);
        let mut rates = Vec::with_capacity(100);
        let mut channels = Vec::with_capacity(100);

        if let Ok(pcm) = PCM::new(name, direction, false) {
            if let Ok(info) = pcm.info() {
                device_number = info.get_device();
                sub_device_number = info.get_subdevice();

                if device_number != vdevice_number {
                    is_real_hw = false;
                }

                if let Ok(hwp) = HwParams::any(&pcm) {
                    for f in FORMATS {
                        if hwp.test_format(Format::from(*f)).is_ok() {
                            formats.push(*f)
                        }
                    }

                    let min_rate = hwp.get_rate_min().unwrap_or(8000).max(8000);
                    let max_rate = hwp.get_rate_max().unwrap_or(768000).min(768000);

                    for r in min_rate..=max_rate {
                        if hwp.test_rate(r).is_ok() {
                            if rates.len() != rates.capacity() {
                                rates.push(r);
                            } else {
                                // Device with conversion. Retest with limited range.
                                //
                                // Devices with rate conversion will say they support
                                // every single sampling rate in min_rate..=max_rate.
                                // We don't want a list of 764000 different available
                                // sampling rates.
                                is_real_hw = false;
                                rates.clear();

                                for r in RATES {
                                    if hwp.test_rate(*r).is_ok() {
                                        rates.push(*r)
                                    }
                                }
                                break;
                            }
                        }
                    }

                    let min_channels = hwp.get_channels_min().unwrap_or(1).max(1);
                    let max_channels = hwp.get_channels_max().unwrap_or(255).min(255);

                    for c in min_channels..=max_channels {
                        if hwp.test_channels(c).is_ok() {
                            if channels.len() != channels.capacity() {
                                channels.push(c);
                            } else {
                                // Device with conversion. Retest with limited range. Same as above.
                                // We don't need a huge list of available channel counts.
                                is_real_hw = false;
                                channels.clear();

                                for c in CHANNELS {
                                    if hwp.test_channels(c).is_ok() {
                                        channels.push(c)
                                    }
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }

        let mut pcm = AlsaPcm {
            name: name.to_string(),
            description,
            direction,
            is_real_hw,
            software_mixable,
            has_mixer,
            card_name: card_name.to_string(),
            device_number,
            sub_device_number,
            formats,
            rates,
            channels,
            valid_configurations: vec![],
        };

        let valid_configs = Self::get_valid_configurations(&pcm);

        if valid_configs.is_empty() {
            None
        } else {
            // Filter out Formats, rates and channels that never
            // appear in a valid config.
            pcm.formats
                .retain(|format| valid_configs.iter().any(|config| config.format == *format));

            pcm.rates
                .retain(|rate| valid_configs.iter().any(|config| config.rate == *rate));

            pcm.channels.retain(|channels| {
                valid_configs
                    .iter()
                    .any(|config| config.channels == *channels)
            });

            pcm.valid_configurations = valid_configs;

            Some(pcm)
        }
    }

    fn has_mixer(name: &str, direction: Direction) -> bool {
        // Try to open two instances of the PCM concurrently to see if it has a builtin mixer.
        PCM::new(name, direction, false).is_ok() && PCM::new(name, direction, false).is_ok()
    }

    fn test_params(
        name: &str,
        direction: Direction,
        audio_format: AudioFormat,
        rate: Option<u32>,
        channels: Option<u32>,
    ) -> bool {
        // It's basically all or nothing with PCMs and HwParams.
        // Once they are in an error state they can't be reused.
        // So every time we test a combination of params we
        // have to create new ones from scratch.
        if let Ok(pcm) = PCM::new(name, direction, false) {
            if let Ok(hwp) = HwParams::any(&pcm) {
                let alsa_format = Format::from(audio_format);

                let _ = hwp.set_format(alsa_format);

                if let Some(rate) = rate {
                    let _ = hwp.set_rate(rate, ValueOr::Nearest);
                }

                if let Some(channels) = channels {
                    let _ = hwp.set_channels(channels);
                }

                return pcm.hw_params(&hwp).is_ok();
            }
        }

        false
    }

    fn get_valid_configurations(pcm: &AlsaPcm) -> Vec<ValidConfiguration> {
        // The supported formats, rates and channels are a bit deceptive.
        // Not all combinations necessarily result in a valid config.
        //
        // Step though all combinations of format, rate and channels testing
        // that they are in fact valid combinations along the way.
        let possible_num_configs = pcm.formats.len() * pcm.rates.len() * pcm.channels.len();

        let mut configs = Vec::with_capacity(possible_num_configs);

        for format in &pcm.formats {
            if Self::test_params(&pcm.name, pcm.direction, *format, None, None) {
                for rate in &pcm.rates {
                    if Self::test_params(&pcm.name, pcm.direction, *format, Some(*rate), None) {
                        for channels in &pcm.channels {
                            if Self::test_params(
                                &pcm.name,
                                pcm.direction,
                                *format,
                                Some(*rate),
                                Some(*channels),
                            ) {
                                let valid_config =
                                    ValidConfiguration::new(pcm.clone(), *format, *rate, *channels);
                                configs.push(valid_config);
                            }
                        }
                    }
                }
            }
        }

        configs
    }
}

fn user_input<T: std::fmt::Display>(display_text: T) -> String {
    print!("{}", format!("\n{display_text}").bold());

    let _ = stdout().flush();

    let mut responce = String::new();

    match stdin().read_line(&mut responce) {
        Ok(_) => responce.trim().to_string(),
        Err(_) => String::new(),
    }
}

fn pick_a_number(display_text: &str, vec_len: usize) -> usize {
    loop {
        let responce = user_input(display_text)
            .parse::<usize>()
            .unwrap_or_default();

        if responce != 0 && responce <= vec_len {
            return responce - 1;
        }

        println!(
            "{}",
            format!("\nPlease Enter a Number [1 - {vec_len}]")
                .bold()
                .yellow()
        );
    }
}

fn choose_a_pcm(pcms: &[AlsaPcm], direction: Direction) -> AlsaPcm {
    let vec_len = pcms.len();
    let mut pcm_index = 0;

    show_pcms(pcms);

    println!("{}", "\nPlease Note:".cyan().bold());
    println!(
        "{}",
        "\nThe listed FORMATS, RATES and CHANNELS can be a bit deceptive.".cyan()
    );

    println!("{}", "\nAlthough the FORMATS, RATES and CHANNELS that never appear in a valid Configuration have been filtered out,".cyan());

    println!("{}", "not all FORMATS, RATES and CHANNELS combinations necessarily result in a valid Configuration.".cyan());

    println!(
        "{}",
        "\nAfter you choose a PCM you will step though the FORMATS, RATES and CHANNELS.".cyan()
    );

    println!(
        "{}",
        format!(
            "\nThe end result {} be a valid Configuration.",
            "should".bold().italic()
        )
        .cyan()
    );

    if vec_len == 1 {
        println!(
            "{}",
            format!("\nThere is only one available {:?} PCM…", direction).cyan()
        );
    } else {
        pcm_index = pick_a_number(&format!("Please Choose a {:?} PCM: ", direction), vec_len);
    }

    pcms[pcm_index].clone()
}

fn show_pcms(pcms: &[AlsaPcm]) {
    let mut table = Table::new();

    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(100);

    for (i, pcm) in pcms.iter().enumerate() {
        let direction = format!("{:?}", pcm.direction).to_uppercase();
        let formats: Vec<String> = pcm.formats.iter().map(|f| f.to_string()).collect();

        table
            .add_row(vec![
                Cell::new(format!("{direction}: {}", i + 1)).add_attribute(Attribute::Bold)
            ])
            .add_row(vec![Cell::new(format!("PCM: {}", pcm.name))])
            .add_row(vec![Cell::new(format!("DESCRIPTION: {}", pcm.description))])
            .add_row(vec![Cell::new(
                format!("FORMATS: {:?}", formats).replace('"', ""),
            )])
            .add_row(vec![Cell::new(format!("RATES: {:?}", pcm.rates))])
            .add_row(vec![Cell::new(format!("CHANNELS: {:?}", pcm.channels))]);
    }

    println!("\n{table}");
}

fn choose_a_configuration(pcm: &AlsaPcm) -> ValidConfiguration {
    let mut configs = pcm.valid_configurations.clone();

    if configs.len() == 1 {
        println!("{}", "\nThere is only one available configuration…".cyan());
    } else {
        let formats = &pcm.formats;
        let formats_len = formats.len();
        let mut format_index = 0;

        let rates = &pcm.rates;
        let rates_len = rates.len();
        let mut rate_index = 0;

        let channels = &pcm.channels;
        let channels_len = channels.len();
        let mut channels_index = 0;

        if formats_len > 1 {
            println!("{}", "\nThe following Formats are available.".cyan());

            show_list(formats);

            format_index = pick_a_number("Please Choose a Format: ", formats_len);
        } else {
            println!("{}", "\nThere is only one available Format…".cyan());

            show_list(formats);
        }

        if rates_len > 1 {
            println!("{}", "\nThe following Sampling Rates are available.".cyan());

            show_list(rates);

            rate_index = pick_a_number("Please Choose a Sampling Rate: ", rates_len);
        } else {
            println!("{}", "\nThere is only one available Sampling Rate…".cyan());

            show_list(rates);
        }

        if channels_len > 1 {
            println!("{}", "\nThe following Channel Counts are available.".cyan());

            show_list(channels);

            channels_index = pick_a_number("Please Choose a Channel Count: ", channels_len);
        } else {
            println!("{}", "\nThere is only one available Channel Count…".cyan());

            show_list(channels);
        }

        let format = formats[format_index];
        let rate = rates[rate_index];
        let channels = channels[channels_index];

        configs.retain(|config| {
            config.format == format && config.rate == rate && config.channels == channels
        });
    }

    configs[0].clone()
}

fn show_list<T: std::fmt::Display>(list: &[T]) {
    let mut table = Table::new();

    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(100);

    if list.len() == 1 {
        table.add_row(vec![Cell::new(format!("{}", list[0]))]);
    } else {
        for (i, item) in list.iter().enumerate() {
            table.add_row(vec![Cell::new(format!("{} - {item}", i + 1))]);
        }
    }

    println!("\n{table}");
}

fn show_configuration(config: &ValidConfiguration) {
    let mut table = Table::new();

    let direction = format!("{:?}", config.direction);

    println!("{}", format!("\n{direction} Configuration:").cyan());

    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(100)
        .add_row(vec![
            Cell::new(direction.to_uppercase()).add_attribute(Attribute::Bold)
        ])
        .add_row(vec![Cell::new(format!("PCM: {}", config.name))])
        .add_row(vec![Cell::new(format!(
            "DESCRIPTION: {}",
            config.description
        ))])
        .add_row(vec![Cell::new(format!("FORMAT: {}", config.format))])
        .add_row(vec![Cell::new(format!("RATE: {}", config.rate))])
        .add_row(vec![Cell::new(format!("CHANNELS: {}", config.channels))]);

    println!("\n{table}");
}

fn choose_a_converter(converters: &[String]) -> &str {
    let vec_len = converters.len();
    let mut converter_index = 0;

    if vec_len == 1 {
        println!(
            "{}",
            "\nThere is only one available Sample Rate converter…".cyan()
        );

        show_list(converters);
    } else {
        println!(
            "{}",
            "\nThe following Sample Rate Converters are available.".cyan()
        );

        show_list(converters);

        converter_index = pick_a_number("Please Choose a Sample Rate Converter: ", vec_len);
    }

    &converters[converter_index]
}

fn get_pcms() -> (Vec<AlsaPcm>, Vec<AlsaPcm>) {
    let mut thread_manager = ThreadManager::new();

    if let Ok(hints) = HintIter::new_str(None, "pcm") {
        for hint in hints {
            if let Some(name) = hint.name {
                let name = name.trim().to_string();
                if name.starts_with("hw:")
                    || name.starts_with("hdmi:")
                    || name.starts_with("iec958:")
                {
                    let description = hint
                        .desc
                        .unwrap_or_else(|| "NONE".to_string())
                        .replace('\n', " ")
                        .trim()
                        .to_string();

                    if let Some(direction) = hint.direction {
                        thread_manager.add_job(&name, &description, direction);
                    }
                }
            }
        }
    }

    thread_manager.get_pcms()
}

fn get_rate_converters() -> Vec<String> {
    let mut rate_converters = Vec::with_capacity(20);

    if let Ok(converters) = glob(CONVERTERS_GLOB_PATH) {
        for converter in converters.flatten() {
            let mut converter = converter.display().to_string();

            converter = converter[converter.find(CONVERTERS_PREFIX).unwrap_or(0)
                ..converter.find(".so").unwrap_or(converter.len())]
                .replace(CONVERTERS_PREFIX, "")
                .trim()
                .to_string();

            rate_converters.push(converter);
        }
    }

    rate_converters
}

fn permission_check(now: &str) {
    // The most effective and least fragile way to see if
    // if have write privileges to /etc is to just try to
    // write a dummy file in /etc.
    let path = DUMMY_FILE_PATH_TEMPLATE.replace("{now}", now);
    if let Err(e) = File::create(path.clone()) {
        let message = format!("\nError: This utility requires write privileges to /etc: {e}")
            .bold()
            .red();

        eprintln!("{message}");
        exit(1);
    }

    let _ = fs::remove_file(path);
}

fn conflict_check() {
    let mut conflicts = Vec::with_capacity(3);

    for [program, name] in CONFLICTING_SOFTWARE {
        if which(program).is_ok() {
            conflicts.push(name)
        }
    }

    if !conflicts.is_empty() {
        let message = format!(
            "\nError: This utility is not compatible with {}.",
            conflicts.join(" / ")
        )
        .bold()
        .red();

        eprintln!("{message}");

        let message = "It is intended to be used on systems that run bare ALSA."
            .bold()
            .red();

        eprintln!("{message}");
        exit(1);
    }
}

fn build_asound_conf(
    playback_config: Option<ValidConfiguration>,
    capture_config: Option<ValidConfiguration>,
    rate_converter: Option<&str>,
) -> String {
    let mut config_blocks = Vec::with_capacity(8);
    let mut input_pcm = "\"null\"".to_string();
    let mut output_pcm = "\"null\"".to_string();
    let mut control = String::new();

    if let Some(rate_converter) = rate_converter {
        let converter = format!("defaults.pcm.rate_converter {rate_converter}");

        config_blocks.push(converter);
    }

    if let Some(config) = playback_config {
        if config.is_real_hw {
            output_pcm = "\"playback\"".to_string();

            let dmix = PLAYBACK_CAPTURE_TEMPLATE
                .replace("{playback_capture}", "playback")
                .replace("{dmix_dsnoop}", "dmix")
                .replace("{card}", &config.card_name)
                .replace("{device}", &config.device_number.to_string())
                .replace("{sub_device}", &config.sub_device_number.to_string())
                .replace("{channels}", &config.channels.to_string())
                .replace("{rate}", &config.rate.to_string())
                .replace("{fmt}", &config.format.to_string());

            config_blocks.push(format!("\n{dmix}"));
        } else {
            output_pcm = format!("\"{}\"", config.name);

            let defaults = DIGITAL_OUPUT_TEMPLATE
                .replace("{channels}", &config.channels.to_string())
                .replace("{rate}", &config.rate.to_string())
                .replace("{fmt}", &config.format.to_string());

            config_blocks.push(defaults);
        }

        control = DEFAULT_CONTROL_TEMPLATE.replace("{card}", &config.card_name);
    }

    if let Some(config) = capture_config {
        if config.is_real_hw {
            input_pcm = "\"capture\"".to_string();

            let dsnoop = PLAYBACK_CAPTURE_TEMPLATE
                .replace("{playback_capture}", "capture")
                .replace("{dmix_dsnoop}", "dsnoop")
                .replace("{card}", &config.card_name)
                .replace("{device}", &config.device_number.to_string())
                .replace("{sub_device}", &config.sub_device_number.to_string())
                .replace("{channels}", &config.channels.to_string())
                .replace("{rate}", &config.rate.to_string())
                .replace("{fmt}", &config.format.to_string());

            config_blocks.push(format!("\n{dsnoop}"));
        } else {
            input_pcm = format!("\"{}\"", config.name);
        }

        if control.is_empty() {
            control = DEFAULT_CONTROL_TEMPLATE.replace("{card}", &config.card_name);
        }
    }

    let asym_default = ASYM_DEFAULT_TEMPLATE
        .replace("{input_pcm}", &input_pcm)
        .replace("{output_pcm}", &output_pcm);

    config_blocks.push(format!("\n{asym_default}"));

    config_blocks.push(format!("\n{control}"));

    config_blocks.join("\n")
}

fn backup_asound_conf(now: &str) {
    let path = BACKUP_FILE_PATH_TEMPLATE.replace("{now}", now);

    if fs::rename(ASOUND_FILE_PATH, path.clone()).is_ok() {
        let message = format!("\n{ASOUND_FILE_PATH} already exists renaming it to:").cyan();
        println!("{message}");
        println!("{}", path.cyan());
    }
}

fn write_asound_conf(config: String) {
    match File::create(ASOUND_FILE_PATH).as_mut() {
        Err(e) => {
            let message = format!("\nError: Could not write {ASOUND_FILE_PATH}: {e}")
                .bold()
                .red();
            eprintln!("{message}");
            exit(1);
        }
        Ok(output) => match write!(output, "{}", &config) {
            Err(e) => {
                let message = format!("\nError: Could not write {ASOUND_FILE_PATH}: {e}")
                    .bold()
                    .red();
                eprintln!("{message}");
                exit(1);
            }
            Ok(_) => {
                println!(
                    "{}",
                    format!("\n{ASOUND_FILE_PATH} was written successfully.").cyan()
                );

                println!(
                    "{}",
                    format!("\nYou can revert your system to it's default state by deleting {ASOUND_FILE_PATH},").cyan()
                );

                println!(
                    "{}",
                    "or revert it from the back up, if one was created,".cyan()
                );

                println!(
                    "{}",
                    "if you have any issues with the generated config.".cyan()
                );

                println!(
                    "{}",
                    "\nif you found this utility useful, and feel so inclined, you can buy me a RedBull".cyan()
                );

                println!(
                    "{} {}",
                    "by sponsoring me at GitHub:".cyan(),
                    "https://github.com/sponsors/JasonLG1979".bold().cyan()
                );

                println!("{}", "\nThanks, and happy listening!!!\n".bold().cyan());
            }
        },
    }
}

fn main() {
    let now = &SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .to_string();

    permission_check(now);

    println!(
        "{} {} {}",
        "\nThis utility will backup".cyan(),
        ASOUND_FILE_PATH.bold().italic().cyan(),
        "if it already exists,".cyan()
    );

    println!(
        "{} {} {}",
        "and generate a new".cyan(),
        ASOUND_FILE_PATH.bold().italic().cyan(),
        "based on your choices.".cyan()
    );

    println!(
        "{}",
        "\nThis utility is intended to be used on headless systems".cyan()
    );

    println!(
        "{}",
        "that run bare ALSA where the hardware does not change often or at all.".cyan()
    );

    println!(
        "{}",
        "\nIt is NOT advised to run this utility on desktop systems."
            .bold()
            .cyan()
    );

    println!(
        "\n{} {} {}",
        "This utility will".cyan(),
        "NOT".bold().cyan(),
        "run on systems that have PulseAudio,".cyan()
    );

    println!(
        "{} {}",
        "Jack Audio or PipeWire installed.".cyan(),
        "That is by design.".bold().cyan()
    );

    println!(
        "{}",
        "\nYou should use those to configure audio if they are installed."
            .bold()
            .italic()
            .cyan()
    );

    println!(
        "\n{}",
        "You can exit this utility any time by pressing Ctrl+C.".cyan()
    );

    println!("\n{}", "Please go to:".cyan());

    println!(
        "\n{}",
        "https://github.com/JasonLG1979/asound-conf-wizard"
            .cyan()
            .bold()
    );

    println!(
        "\n{}",
        "If you any questions, issues, or would like to contribute to this project.".cyan()
    );

    let confirm = user_input("Please Enter \"OK\" to Continue: ").to_lowercase();

    if confirm != "ok" {
        exit(0);
    }

    conflict_check();

    println!(
        "{}",
        "\nPlease make sure that all Audio Playback and Capture Devices are not currently in use."
            .cyan()
            .bold()
    );

    println!(
        "{}",
        "\nDevices that are in use may not be available to choose from."
            .cyan()
            .bold()
    );

    let enter = user_input("Please Press Enter to Continue");

    if !enter.is_empty() {
        exit(0);
    }

    println!(
        "{}",
        "\nRetrieving PCM parameters. This may take a moment…".cyan()
    );

    let (playback_pcms, capture_pcms) = get_pcms();

    let converters = get_rate_converters();

    let playback_config = if playback_pcms.is_empty() {
        println!("{}", "\nThere are no available Playback PCMs…".cyan());

        None
    } else {
        loop {
            let playback_pcm = choose_a_pcm(&playback_pcms, Direction::Playback);

            let pcm_name = &playback_pcm.name;

            if !playback_pcm.software_mixable && !playback_pcm.has_mixer {
                println!(
                    "{}",
                    format!("\n{pcm_name}. Does NOT support Software Mixing,")
                        .bold()
                        .yellow()
                );

                println!(
                    "{}",
                    "and does not appear to have a Hardware Mixer either, concurrent access may not be possible."
                        .bold()
                        .yellow()
                );

                let confirm = user_input("If this is acceptable Please Enter \"OK\" to Continue: ")
                    .to_lowercase();

                if confirm != "ok" {
                    continue;
                }
            }

            let config = {
                let config = choose_a_configuration(&playback_pcm);

                show_configuration(&config);

                let confirm = user_input("If this is acceptable Please Enter \"OK\" to Continue: ")
                    .to_lowercase();

                if confirm != "ok" {
                    None
                } else {
                    Some(config)
                }
            };

            if config.is_some() {
                break config;
            }
        }
    };

    let capture_config = if capture_pcms.is_empty() {
        println!("{}", "\nThere are no available capture PCMs…".cyan());

        None
    } else {
        loop {
            let capture_pcm = choose_a_pcm(&capture_pcms, Direction::Capture);

            let pcm_name = &capture_pcm.name;

            if !capture_pcm.software_mixable && !capture_pcm.has_mixer {
                println!(
                    "{}",
                    format!("\n{pcm_name}. Does NOT support Software Mixing,")
                        .bold()
                        .yellow()
                );

                println!(
                    "{}",
                    "and does not appear to have a Hardware Mixer either. Concurrent access may not be possible."
                        .bold()
                        .yellow()
                );

                let confirm = user_input("If this is acceptable Please Enter \"OK\" to Continue: ")
                    .to_lowercase();

                if confirm != "ok" {
                    continue;
                }
            }

            let config = {
                let config = choose_a_configuration(&capture_pcm);

                show_configuration(&config);

                let confirm = user_input("If this is acceptable Please Enter \"OK\" to Continue: ")
                    .to_lowercase();

                if confirm != "ok" {
                    None
                } else {
                    Some(config)
                }
            };

            if config.is_some() {
                break config;
            }
        }
    };

    if playback_config.is_some() || capture_config.is_some() {
        let converter = if !converters.is_empty() {
            Some(choose_a_converter(&converters))
        } else {
            println!(
                "{}",
                "\nThere are no available Sample Rate Converters…".cyan()
            );

            None
        };

        let confirm = user_input(format!(
            "Please Enter \"OK\" to commit your choices to {ASOUND_FILE_PATH}: "
        ))
        .to_lowercase();

        if confirm != "ok" {
            println!("{}", "\nYou did not enter \"OK\".".cyan());

            println!(
                "{}",
                "\nNo files or configurations have been changed.".cyan()
            );

            exit(0);
        }

        backup_asound_conf(now);

        write_asound_conf(build_asound_conf(
            playback_config,
            capture_config,
            converter,
        ));
    }
}
