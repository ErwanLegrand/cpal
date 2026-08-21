//! Plays a 440 Hz sine wave through an endpoint opened in WASAPI exclusive mode.
//!
//! This example demonstrates:
//! - Binding exclusive mode to a device with `WasapiDeviceExt::with_options`
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
    platform::wasapi_ext::{WasapiDeviceExt, WasapiStreamOptions},
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
        host.default_output_device()
    }
    .ok_or_else(|| anyhow::Error::msg("failed to find output device"))?;
    println!("Output device: {}", device.id()?);

    let exclusive = match device.with_options(WasapiStreamOptions::exclusive()) {
        Ok(exclusive) => exclusive,
        Err(err) => {
            println!("{err}");
            return Ok(());
        }
    };

    // Both the query and the build go through `exclusive`: a config negotiated here and passed to
    // `device` would open shared mode instead.
    let config = match exclusive.default_output_config() {
        Ok(config) => config,
        Err(err) => {
            report(&err);
            return Ok(());
        }
    };
    println!("Exclusive output config: {config:?}");

    if let Err(err) = play(&exclusive, &config) {
        report(&err);
    }
    Ok(())
}

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
    eprintln!("Exclusive mode failed: {err}");
    match err.kind() {
        ErrorKind::DeviceBusy => {
            eprintln!("Another application already holds this endpoint exclusively.")
        }
        ErrorKind::UnsupportedConfig => eprintln!(
            "Check that \"Allow applications to take exclusive control of this device\" is \
             enabled under this device's Sound properties."
        ),
        _ => {}
    }
}
