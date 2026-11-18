use anyhow::{Result, bail};
use cpal::Device;
use cpal::traits::{DeviceTrait, HostTrait};

/// Names of every audio input device on this Mac.
pub fn list_inputs() -> Result<Vec<String>> {
    let host = cpal::default_host();

    Ok(host
        .input_devices()?
        .filter_map(|device| device_name(&device))
        .collect())
}

/// Find an input device by exact name, or failing that, by a case-insensitive part of the name.
pub fn find_input(wanted: &str) -> Result<Device> {
    let host = cpal::default_host();
    let devices: Vec<Device> = host.input_devices()?.collect();

    let exact = devices
        .iter()
        .position(|d| device_name(d).as_deref() == Some(wanted));

    let partial = || {
        let wanted = wanted.to_lowercase();
        devices.iter().position(|d| {
            device_name(d).is_some_and(|name| name.to_lowercase().contains(&wanted))
        })
    };

    match exact.or_else(partial) {
        Some(index) => Ok(devices.into_iter().nth(index).expect("index is in range")),
        None => bail!(
            "audio input '{wanted}' not found. Available: {}",
            list_inputs()?.join(", ")
        ),
    }
}

/// Human-readable device name, if the system gives one.
pub fn device_name(device: &Device) -> Option<String> {
    device.description().ok().map(|d| d.name().to_string())
}
