use std::{
    cmp::Ordering,
    fs,
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
use itertools::Itertools;
use which::which;

const FORMATS: [Format; 4] = [Format::s16(), Format::s24_3(), Format::s24(), Format::s32()];

const MIN_RATE: u32 = 3000;
const MAX_RATE: u32 = 768000;
const US_PER_MS: u32 = 1000;
const PERIODS_PER_BUFFER: u32 = 5;
const MIN_BUFFER_TIME_US: u32 = 1000;
const MAX_BUFFER_TIME_US: u32 = 1000000;

const CONFLICTING_SOFTWARE: [[&str; 2]; 3] = [
    ["pulseaudio", "PulseAudio"],
    ["pipewire", "PipeWire"],
    ["jackd", "JACK Audio"],
];

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
        period_size 0
        buffer_size 0
        periods 0
        buffer_time {buffer_time}
        period_time {period_time}
    }
}";

// See:
// https://github.com/alsa-project/alsa-lib/blob/master/src/pcm/pcm_asym.c#L20
const ASYM_TEMPLATE: &str = "\
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

const CONTROL_TEMPLATE: &str = "\
ctl.!default {
    type hw
    card {card}
}";

#[derive(Debug, Clone)]
enum WorkerJob {
    GetPcm {
        name: String,
        card_name: String,
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

    pub fn add_job(&mut self, name: &str, direction: Direction) {
        let card_name = name[name.find('=').unwrap_or(0)..name.find(',').unwrap_or(name.len())]
            .replace('=', "")
            .trim()
            .to_string();

        let mut job_sent = false;
        let mut bad_worker = false;

        for worker in self.workers.iter_mut() {
            if worker.card_name == card_name {
                job_sent = worker.add_job(name, &card_name, direction);
                bad_worker = !job_sent;
            }
        }

        if bad_worker {
            // Drop the worker if it exists but add_job fails.
            self.workers.retain(|worker| worker.card_name != card_name);
        }

        if !job_sent {
            let mut worker = ThreadWorker::new(card_name.clone());

            if worker.add_job(name, &card_name, direction) {
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
                            direction,
                        } => match direction {
                            Direction::Playback => {
                                let alsa_pcm = AlsaPcm::new(&name, &card_name, direction);

                                if let Some(alsa_pcm) = alsa_pcm {
                                    playback_pcms.push(alsa_pcm);
                                }
                            }
                            Direction::Capture => {
                                let alsa_pcm = AlsaPcm::new(&name, &card_name, direction);

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

    pub fn add_job(&mut self, name: &str, card_name: &str, direction: Direction) -> bool {
        if let Some(sender) = self.job_sender.as_mut() {
            let job = WorkerJob::GetPcm {
                name: name.to_string(),
                card_name: card_name.to_string(),
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
    pub card_name: String,
    pub device_number: u32,
    pub sub_device_number: u32,
    pub format: Format,
    pub rate: u32,
    pub channels: u32,
    pub buffer_time_ms: u32,
    buffer_time_range: RangeInclusive<u32>,
}

impl ValidConfiguration {
    pub fn new(pcm: AlsaPcm, format: Format, rate: u32, channels: u32) -> Self {
        let (buffer_time_min, buffer_time_max) =
            Self::get_buffer_time_range(&pcm.name, pcm.direction, format, rate, channels);

        let fallback_buffer_time_ms = (buffer_time_max / 2).max(buffer_time_min) / US_PER_MS;

        Self {
            name: pcm.name,
            description: pcm.description,
            direction: pcm.direction,
            card_name: pcm.card_name,
            device_number: pcm.device_number,
            sub_device_number: pcm.sub_device_number,
            format,
            rate,
            channels,
            buffer_time_ms: fallback_buffer_time_ms,
            buffer_time_range: buffer_time_min..=buffer_time_max,
        }
    }

    pub fn get_buffer_times_ms(&mut self) -> Vec<u32> {
        let mut buffer_times_ms = Vec::with_capacity(1000);

        for buffer_time in self.buffer_time_range.clone().step_by(US_PER_MS as usize) {
            let period_time = buffer_time / PERIODS_PER_BUFFER;

            if self.test_buffer_times(buffer_time, period_time) {
                buffer_times_ms.push(buffer_time / US_PER_MS);
            }
        }

        buffer_times_ms
    }

    fn get_buffer_time_range(
        name: &str,
        direction: Direction,
        format: Format,
        rate: u32,
        channels: u32,
    ) -> (u32, u32) {
        if let Ok(pcm) = PCM::new(name, direction, false) {
            if let Ok(hwp) = HwParams::any(&pcm) {
                match hwp.set_rate_resample(false) {
                    Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                    Ok(_) => match hwp.get_rate_resample() {
                        Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                        Ok(actual_rate_resample) => {
                            if actual_rate_resample {
                                return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US);
                            }
                        }
                    },
                }

                match hwp.set_format(format) {
                    Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                    Ok(_) => match hwp.get_format() {
                        Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                        Ok(actual_format) => {
                            if actual_format != format {
                                return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US);
                            }
                        }
                    },
                }

                match hwp.set_rate(rate, ValueOr::Nearest) {
                    Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                    Ok(_) => match hwp.get_rate() {
                        Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                        Ok(actual_rate) => {
                            if actual_rate != rate {
                                return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US);
                            }
                        }
                    },
                }

                match hwp.set_channels(channels) {
                    Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                    Ok(_) => match hwp.get_channels() {
                        Err(_) => return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US),
                        Ok(actual_channels) => {
                            if actual_channels != channels {
                                return (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US);
                            }
                        }
                    },
                }

                let buffer_time_min = match hwp.get_buffer_time_min() {
                    Err(_) => MIN_BUFFER_TIME_US,
                    Ok(buffer_time_min) => {
                        ((buffer_time_min / US_PER_MS) * US_PER_MS).max(MIN_BUFFER_TIME_US)
                    }
                };

                let buffer_time_max = match hwp.get_buffer_time_max() {
                    Err(_) => MAX_BUFFER_TIME_US,
                    Ok(buffer_time_max) => {
                        ((buffer_time_max / US_PER_MS) * US_PER_MS).min(MAX_BUFFER_TIME_US)
                    }
                };

                return (buffer_time_min, buffer_time_max);
            }
        }
        (MIN_BUFFER_TIME_US, MAX_BUFFER_TIME_US)
    }

    fn test_buffer_times(&mut self, buffer_time: u32, period_time: u32) -> bool {
        // It's basically all or nothing with PCMs and HwParams.
        // Once they are in an error state they can't be reused.
        // So every time we test a combination of params we
        // have to create new ones from scratch.
        if let Ok(pcm) = PCM::new(&self.name, self.direction, false) {
            if let Ok(hwp) = HwParams::any(&pcm) {
                match hwp.set_rate_resample(false) {
                    Err(_) => return false,
                    Ok(_) => match hwp.get_rate_resample() {
                        Err(_) => return false,
                        Ok(actual_rate_resample) => {
                            if actual_rate_resample {
                                return false;
                            }
                        }
                    },
                }

                match hwp.set_format(self.format) {
                    Err(_) => return false,
                    Ok(_) => match hwp.get_format() {
                        Err(_) => return false,
                        Ok(actual_format) => {
                            if actual_format != self.format {
                                return false;
                            }
                        }
                    },
                }

                match hwp.set_rate(self.rate, ValueOr::Nearest) {
                    Err(_) => return false,
                    Ok(_) => match hwp.get_rate() {
                        Err(_) => return false,
                        Ok(actual_rate) => {
                            if actual_rate != self.rate {
                                return false;
                            }
                        }
                    },
                }

                match hwp.set_channels(self.channels) {
                    Err(_) => return false,
                    Ok(_) => match hwp.get_channels() {
                        Err(_) => return false,
                        Ok(actual_channels) => {
                            if actual_channels != self.channels {
                                return false;
                            }
                        }
                    },
                }

                match hwp.set_buffer_time_near(buffer_time, ValueOr::Nearest) {
                    Err(_) => return false,
                    Ok(actual_buffer_time) => {
                        if actual_buffer_time != buffer_time {
                            return false;
                        }
                    }
                }

                match hwp.set_period_time_near(period_time, ValueOr::Nearest) {
                    Err(_) => return false,
                    Ok(actual_period_time) => {
                        if actual_period_time != period_time {
                            return false;
                        }
                    }
                }

                return pcm.hw_params(&hwp).is_ok();
            }
        }

        false
    }
}

#[derive(Debug, Clone)]
struct AlsaPcm {
    pub name: String,
    pub description: String,
    pub direction: Direction,
    pub card_name: String,
    pub device_number: u32,
    pub sub_device_number: u32,
    pub formats: Vec<Format>,
    pub rates: Vec<u32>,
    pub channels: Vec<u32>,
    pub valid_configurations: Vec<ValidConfiguration>,
}

impl AlsaPcm {
    pub fn new(name: &str, card_name: &str, direction: Direction) -> Option<Self> {
        let mut description = String::new();
        let mut device_number: u32 = 0;
        let mut sub_device_number: u32 = 0;
        let mut formats = Vec::with_capacity(4);
        let mut rates = Vec::with_capacity(100);
        let mut channels = Vec::with_capacity(100);

        if let Ok(pcm) = PCM::new(name, direction, false) {
            if let Ok(info) = pcm.info() {
                description = info.get_name().unwrap_or("NONE").to_string();
                device_number = info.get_device();
                sub_device_number = info.get_subdevice();

                if let Ok(hwp) = HwParams::any(&pcm) {
                    if hwp.set_rate_resample(false).is_ok() {
                        for f in FORMATS {
                            if hwp.test_format(f).is_ok() {
                                formats.push(f)
                            }
                        }

                        if formats.is_empty() {
                            println!(
                                "{}",
                                format!(
                                    "\n{name} does not support any formats supported by dmix/dsnoop."
                                ).bold().yellow()
                            );

                            println!(
                                "{}",
                                format!("\n{name} is not software mixable, and will be ignored.")
                                    .bold()
                                    .yellow()
                            );

                            return None;
                        }

                        let min_rate = hwp.get_rate_min().unwrap_or(MIN_RATE).max(MIN_RATE);
                        let max_rate = hwp.get_rate_max().unwrap_or(MAX_RATE).min(MAX_RATE);

                        for r in min_rate..=max_rate {
                            if hwp.test_rate(r).is_ok() {
                                if rates.len() != rates.capacity() {
                                    rates.push(r);
                                } else {
                                    println!(
                                        "{}",
                                        format!(
                                            "\n{name} is reporting an unusually large number of supported sampling rates (100+)."
                                        ).bold().yellow()
                                    );

                                    println!(
                                        "{}",
                                        format!(
                                            "\n{name} is more than likely not a real hardware device, but is actually a hardware device behind a plug plugin."
                                        ).bold().yellow()
                                    );

                                    println!(
                                        "{}",
                                        format!(
                                            "\n{name} is not software mixable, and will be ignored."
                                        )
                                        .bold()
                                        .yellow()
                                    );

                                    return None;
                                }
                            }
                        }

                        let min_channels = hwp.get_channels_min().unwrap_or(1).max(1);
                        let max_channels = hwp.get_channels_max().unwrap_or(u32::MAX).max(1);

                        for c in min_channels..=max_channels {
                            if hwp.test_channels(c).is_ok() {
                                if channels.len() != channels.capacity() {
                                    channels.push(c);
                                } else {
                                    println!(
                                        "{}",
                                        format!(
                                            "\n{name} is reporting an unusually large number of supported channel counts (100+)."
                                        ).bold().yellow()
                                    );

                                    println!(
                                        "{}",
                                        format!(
                                            "\n{name} is more than likely not a real hardware device, but is actually a hardware device behind a plug plugin."
                                        ).bold().yellow()
                                    );

                                    println!(
                                        "{}",
                                        format!(
                                            "\n{name} is not software mixable, and will be ignored."
                                        )
                                        .bold()
                                        .yellow()
                                    );

                                    return None;
                                }
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
            println!(
                "{}",
                format!("\n{name} has no valid configurations, and will be ignored.")
                    .bold()
                    .yellow()
            );

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

    fn test_params(
        name: &str,
        direction: Direction,
        format: Format,
        rate: Option<u32>,
        channels: Option<u32>,
    ) -> bool {
        // It's basically all or nothing with PCMs and HwParams.
        // Once they are in an error state they can't be reused.
        // So every time we test a combination of params we
        // have to create new ones from scratch.
        if let Ok(pcm) = PCM::new(name, direction, false) {
            if let Ok(hwp) = HwParams::any(&pcm) {
                match hwp.set_rate_resample(false) {
                    Err(_) => return false,
                    Ok(_) => match hwp.get_rate_resample() {
                        Err(_) => return false,
                        Ok(actual_rate_resample) => {
                            if actual_rate_resample {
                                return false;
                            }
                        }
                    },
                }

                match hwp.set_format(format) {
                    Err(_) => return false,
                    Ok(_) => match hwp.get_format() {
                        Err(_) => return false,
                        Ok(actual_format) => {
                            if actual_format != format {
                                return false;
                            }
                        }
                    },
                }

                if let Some(rate) = rate {
                    match hwp.set_rate(rate, ValueOr::Nearest) {
                        Err(_) => return false,
                        Ok(_) => match hwp.get_rate() {
                            Err(_) => return false,
                            Ok(actual_rate) => {
                                if actual_rate != rate {
                                    return false;
                                }
                            }
                        },
                    }
                }

                if let Some(channels) = channels {
                    match hwp.set_channels(channels) {
                        Err(_) => return false,
                        Ok(_) => match hwp.get_channels() {
                            Err(_) => return false,
                            Ok(actual_channels) => {
                                if actual_channels != channels {
                                    return false;
                                }
                            }
                        },
                    }
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
        if let Ok(responce) = user_input(display_text).parse::<usize>() {
            if (1..=vec_len).contains(&responce) {
                return responce - 1;
            }
        }

        println!(
            "{}",
            format!("\nPlease Enter a Number [1 - {vec_len}]")
                .bold()
                .yellow()
        );
    }
}

fn pick_from_choices(display_text: &str, choices: &[u32]) -> u32 {
    let first_choice = choices[0];
    let last_choice = choices[choices.len() - 1];
    let display_text = &format!("{display_text} [{first_choice} - {last_choice}]: ");
    loop {
        if let Ok(responce) = user_input(display_text).parse::<u32>() {
            if (first_choice..=last_choice).contains(&responce) {
                if let Some(value) = choices.iter().min_by_key(|x| x.abs_diff(responce)) {
                    return *value;
                }
            }
        }

        println!(
            "{}",
            format!("\nPlease Enter a Number [{first_choice} - {last_choice}]")
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
            .add_row(vec![Cell::new(format!("CARD: {}", pcm.card_name))])
            .add_row(vec![Cell::new(format!("DEV: {}", pcm.device_number))])
            .add_row(vec![Cell::new(format!("DESCRIPTION: {}", pcm.description))])
            .add_row(vec![Cell::new(
                format!("FORMATS: {:?}", formats).replace('"', ""),
            )])
            .add_row(vec![Cell::new(format!("RATES: {:?}", pcm.rates))])
            .add_row(vec![Cell::new(format!("CHANNELS: {:?}", pcm.channels))]);
    }

    println!("\n{table}");
}

fn choose_a_configuration(mut configs: Vec<ValidConfiguration>) -> ValidConfiguration {
    if configs.len() == 1 {
        println!("{}", "\nThere is only one available configuration…".cyan());
    } else {
        let mut formats: Vec<Format> = configs
            .iter()
            .map(|config| config.format)
            .unique()
            .collect();

        formats.sort();

        let formats_len = formats.len();

        let mut format_index = 0;

        if formats_len > 1 {
            println!("{}", "\nThe following Formats are available.".cyan());

            show_list(&formats);

            format_index = pick_a_number("Please Choose a Format: ", formats_len);
        } else {
            println!("{}", "\nThere is only one available Format…".cyan());

            show_list(&formats);
        }

        let format = formats[format_index];

        configs.retain(|config| config.format == format);

        let mut rates: Vec<u32> = configs.iter().map(|config| config.rate).unique().collect();

        rates.sort();

        let rates_len = rates.len();

        let mut rate_index = 0;

        if rates_len > 1 {
            println!("{}", "\nThe following Sampling Rates are available.".cyan());

            show_list(&rates);

            rate_index = pick_a_number("Please Choose a Sampling Rate: ", rates_len);
        } else {
            println!("{}", "\nThere is only one available Sampling Rate…".cyan());

            show_list(&rates);
        }

        let rate = rates[rate_index];

        configs.retain(|config| config.rate == rate);

        let mut channels: Vec<u32> = configs
            .iter()
            .map(|config| config.channels)
            .unique()
            .collect();

        channels.sort();

        let channels_len = channels.len();

        let mut channels_index = 0;

        if channels_len > 1 {
            println!("{}", "\nThe following Channel Counts are available.".cyan());

            show_list(&channels);

            channels_index = pick_a_number("Please Choose a Channel Count: ", channels_len);
        } else {
            println!("{}", "\nThere is only one available Channel Count…".cyan());

            show_list(&channels);
        }

        let channels = channels[channels_index];

        configs.retain(|config| config.channels == channels);
    }

    let mut config = configs[0].clone();

    println!(
        "{}",
        "\nRetrieving Buffer parameters. This may take a moment…".cyan()
    );

    let buffer_times_ms = config.get_buffer_times_ms();

    let buffer_times_ms_len = buffer_times_ms.len();

    match buffer_times_ms_len.cmp(&1) {
        Ordering::Greater => {
            println!(
                "{}",
                "\nYour choice will be snapped to the nearest available time.".cyan()
            );
            config.buffer_time_ms = pick_from_choices(
                "Please Choose a Buffer Time in milliseconds from",
                &buffer_times_ms,
            );
        }
        Ordering::Equal => {
            println!(
                "{}",
                "\nThere is only one available Buffer Time in milliseconds…".cyan()
            );

            show_list(&buffer_times_ms);
            config.buffer_time_ms = buffer_times_ms[0];
        }
        Ordering::Less => {
            println!(
                "{}",
                format!(
                    "\nNo available Buffer Times were reported, falling back to {} milliseconds.",
                    config.buffer_time_ms
                )
                .bold()
                .yellow()
            );

            println!(
                    "{}",
                    format!("\nIf you experience issues you may need to manually edit {ASOUND_FILE_PATH} to correct them.").bold().yellow()
                );
        }
    }

    config
}

fn show_list<T: std::fmt::Display>(list: &[T]) {
    let mut table = Table::new();

    table
        .load_preset(UTF8_FULL)
        .apply_modifier(UTF8_SOLID_INNER_BORDERS)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_width(100);

    let list_len = list.len();

    if list_len == 1 {
        table.add_row(vec![Cell::new(format!("{}", list[0]))]);
    } else {
        let content_width = list
            .iter()
            .map(|x| x.to_string().len())
            .max()
            .unwrap_or_default();

        let index_width = list_len.to_string().len();

        for (i, item) in list.iter().enumerate() {
            table.add_row(vec![Cell::new(format!(
                "{:<index_width$} │ {:>content_width$}",
                i + 1,
                item
            ))]);
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
        .add_row(vec![Cell::new(format!("CARD: {}", config.card_name))])
        .add_row(vec![Cell::new(format!("DEV: {}", config.device_number))])
        .add_row(vec![Cell::new(format!(
            "DESCRIPTION: {}",
            config.description
        ))])
        .add_row(vec![Cell::new(format!("FORMAT: {}", config.format))])
        .add_row(vec![Cell::new(format!("RATE: {}", config.rate))])
        .add_row(vec![Cell::new(format!("CHANNELS: {}", config.channels))]);

    table.add_row(vec![Cell::new(format!(
        "BUFFER TIME MS: {}",
        config.buffer_time_ms
    ))]);

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
                if name.starts_with("hw:") {
                    if let Some(direction) = hint.direction {
                        thread_manager.add_job(&name, direction);
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
                ..converter.find(".so").unwrap_or(converter.len() - 1)]
                .replace(CONVERTERS_PREFIX, "")
                .trim()
                .to_string();

            rate_converters.push(converter);
        }
    }

    rate_converters
}

fn permission_check(now: &str) {
    // The most effective and least brittle way to see if
    // we have write privileges to /etc is to just try to
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
    let mut config_blocks = Vec::with_capacity(5);
    let mut input_pcm = "\"null\"".to_string();
    let mut output_pcm = "\"null\"".to_string();
    let mut converter = String::new();
    let mut playback = String::new();
    let mut capture = String::new();
    let mut control = String::new();

    if let Some(rate_converter) = rate_converter {
        converter = format!("defaults.pcm.rate_converter {rate_converter}");
    }

    if let Some(config) = playback_config {
        output_pcm = "\"playback\"".to_string();

        let buffer_time = config.buffer_time_ms * US_PER_MS;
        let period_time = buffer_time / PERIODS_PER_BUFFER;

        playback = PLAYBACK_CAPTURE_TEMPLATE
            .replace("{playback_capture}", "playback")
            .replace("{dmix_dsnoop}", "dmix")
            .replace("{card}", &config.card_name)
            .replace("{device}", &config.device_number.to_string())
            .replace("{sub_device}", &config.sub_device_number.to_string())
            .replace("{channels}", &config.channels.to_string())
            .replace("{rate}", &config.rate.to_string())
            .replace("{fmt}", &config.format.to_string())
            .replace("{buffer_time}", &buffer_time.to_string())
            .replace("{period_time}", &period_time.to_string());

        control = CONTROL_TEMPLATE.replace("{card}", &config.card_name);
    }

    if let Some(config) = capture_config {
        input_pcm = "\"capture\"".to_string();

        let buffer_time = config.buffer_time_ms * US_PER_MS;
        let period_time = buffer_time / PERIODS_PER_BUFFER;

        capture = PLAYBACK_CAPTURE_TEMPLATE
            .replace("{playback_capture}", "capture")
            .replace("{dmix_dsnoop}", "dsnoop")
            .replace("{card}", &config.card_name)
            .replace("{device}", &config.device_number.to_string())
            .replace("{sub_device}", &config.sub_device_number.to_string())
            .replace("{channels}", &config.channels.to_string())
            .replace("{rate}", &config.rate.to_string())
            .replace("{fmt}", &config.format.to_string())
            .replace("{buffer_time}", &buffer_time.to_string())
            .replace("{period_time}", &period_time.to_string());

        if control.is_empty() {
            control = CONTROL_TEMPLATE.replace("{card}", &config.card_name);
        }
    }

    if !converter.is_empty() {
        config_blocks.push(format!("{converter}\n"));
    }

    if !playback.is_empty() {
        config_blocks.push(format!("{playback}\n"));
    }

    if !capture.is_empty() {
        config_blocks.push(format!("{capture}\n"));
    }

    let asym = ASYM_TEMPLATE
        .replace("{input_pcm}", &input_pcm)
        .replace("{output_pcm}", &output_pcm);

    config_blocks.push(asym);

    if !control.is_empty() {
        config_blocks.push(format!("\n{control}"));
    }

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
                    "or revert it from the back up, if one was created, if you have any issues with the generated config.".cyan()
                );

                println!(
                    "{}",
                    "\nif you found this utility useful, and feel so inclined, you can buy me a RedBull at:".cyan()
                );

                println!(
                    "{}",
                    "\nhttps://github.com/sponsors/JasonLG1979".bold().cyan()
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
        "If you have any questions, issues, or would like to contribute to this project.".cyan()
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

            let config = {
                let config = choose_a_configuration(playback_pcm.valid_configurations.clone());

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
        println!("{}", "\nThere are no available Capture PCMs…".cyan());

        None
    } else {
        loop {
            let capture_pcm = choose_a_pcm(&capture_pcms, Direction::Capture);

            let config = {
                let config = choose_a_configuration(capture_pcm.valid_configurations.clone());

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
