//! Plays a 440 Hz sine wave through an endpoint opened in WASAPI exclusive mode.
//!
//! This example demonstrates:
//! - Binding exclusive mode to a device with `WasapiDeviceExt::with_options(ShareMode::Exclusive)`
//! - Negotiating the config and building the stream on that same configured device
//! - Reporting the failures exclusive mode brings with it
//!
//! Run with: `cargo run --example exclusive`
//!
//! Exclusive mode needs a WASAPI endpoint; on any other device this reports that and exits.

use clap::Parser;
use cpal::{
    CallbackInfo, Error, ErrorKind, FromSample, I24, SampleFormat, SizedSample,
    SupportedStreamConfig,
    platform::wasapi_ext::{ShareMode, WasapiDeviceExt},
    traits::{DeviceTrait, HostTrait, StreamTrait},
};

#[derive(Parser, Debug)]
#[command(version, about = "CPAL WASAPI exclusive mode example", long_about = None)]
struct Opt {
    /// The audio device to use
    #[arg(short, long)]
    device: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();
    let host = cpal::default_host();

    let device = if let Some(device) = opt.device {
        let id = &device.parse().expect("failed to parse device id");
        host.device_by_id(id)
    } else {
        // Resolve the default device to its concrete endpoint ID: WASAPI exclusive mode locks
        // a specific hardware endpoint and cannot open virtual default-device GUIDs.
        host.default_output_device()
            .and_then(|d| d.id().ok())
            .and_then(|id| host.device_by_id(&id))
    }
    .ok_or_else(|| anyhow::Error::msg("failed to find output device"))?;
    println!("Output device: {}", device.id()?);

    let exclusive = match device.with_options(ShareMode::Exclusive) {
        Ok(exclusive) => exclusive,
        Err(err) => {
            report(&err);
            return Err(anyhow::Error::msg(format!("{err}")));
        }
    };

    // Both the query and the build go through `exclusive`: a config negotiated here and passed to
    // `device` would open shared mode instead.
    let config = match exclusive.default_output_config() {
        Ok(config) => config,
        Err(err) => {
            report(&err);
            return Err(anyhow::Error::msg(format!("{err}")));
        }
    };
    println!("Exclusive output config: {config:?}");

    if let Err(err) = play(&exclusive, &config) {
        report(&err);
        return Err(anyhow::Error::msg(format!("{err}")));
    }
    Ok(())
}

// Exclusive mode drives the endpoint natively with no engine-side sample conversion, so this
// probes only the formats the WASAPI backend actually supports exclusively — a narrower set than
// the cross-platform list in examples/beep.rs, by design.
fn play<D: DeviceTrait>(device: &D, config: &SupportedStreamConfig) -> Result<(), Error> {
    match config.sample_format() {
        SampleFormat::U8 => run::<u8, D>(device, config),
        SampleFormat::I16 => run::<i16, D>(device, config),
        SampleFormat::I24 => run::<I24, D>(device, config),
        SampleFormat::I32 => run::<i32, D>(device, config),
        SampleFormat::F32 => run::<f32, D>(device, config),
        sample_format => Err(Error::with_message(
            ErrorKind::UnsupportedConfig,
            format!("unsupported sample format '{sample_format}'"),
        )),
    }
}

fn run<T, D>(device: &D, config: &SupportedStreamConfig) -> Result<(), Error>
where
    T: SizedSample + FromSample<f32>,
    D: DeviceTrait,
{
    let channels = config.channels() as usize;
    let sample_rate = config.sample_rate() as f32;
    let mut sample_clock = 0f32;
    let mut next_value = move || {
        sample_clock = (sample_clock + 1.0) % sample_rate;
        (sample_clock * 440.0 * 2.0 * std::f32::consts::PI / sample_rate).sin()
    };

    let stream = device.build_output_stream(
        config.config(),
        move |data: &mut [T], info: &CallbackInfo| {
            if info.xrun() {
                eprintln!("output underrun");
            }
            for frame in data.chunks_mut(channels) {
                let value = T::from_sample(next_value());
                frame.fill(value);
            }
        },
        |err: Error| eprintln!("Stream error: {err}"),
        None,
    )?;
    stream.start()?;
    std::thread::sleep(std::time::Duration::from_millis(1000));
    stream.stop(Some(std::time::Duration::from_millis(200)))?;

    Ok(())
}

fn report(err: &Error) {
    // One-line pointers only; the error itself is printed once, by `main`'s error return.
    // Full troubleshooting lives in the README's WASAPI exclusive-mode section.
    match err.kind() {
        ErrorKind::DeviceBusy => eprintln!(
            "Another application already holds this endpoint exclusively — close it and retry."
        ),
        ErrorKind::ExclusiveModeDenied => eprintln!(
            "Enable \"Allow applications to take exclusive control of this device\" in the \
             device's sound properties — see README for details."
        ),
        ErrorKind::UnsupportedConfig => {
            eprintln!("This endpoint accepts no format CPAL can drive in exclusive mode.")
        }
        _ => {}
    }
}
