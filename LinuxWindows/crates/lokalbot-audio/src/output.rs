use cpal::traits::{DeviceTrait, HostTrait};

fn device_key(device: &cpal::Device) -> Option<String> {
    let id = device.id().map(|id| id.to_string()).unwrap_or_default();
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_default();
    if id.is_empty() && name.is_empty() {
        None
    } else {
        Some(format!("{id}|{name}"))
    }
}

pub(crate) fn default_output_key() -> Option<String> {
    crate::capture::host()
        .default_output_device()
        .as_ref()
        .and_then(device_key)
}

pub(crate) fn should_reopen_on_error(error: &cpal::Error) -> bool {
    should_reopen_on_error_kind(error.kind())
}

pub(crate) fn should_reopen_on_error_kind(kind: cpal::ErrorKind) -> bool {
    matches!(
        kind,
        cpal::ErrorKind::StreamInvalidated
            | cpal::ErrorKind::DeviceNotAvailable
            | cpal::ErrorKind::DeviceChanged
    )
}

pub(crate) fn open_default_sink<E>(
    error_callback: E,
) -> Result<(rodio::MixerDeviceSink, Option<String>), rodio::DeviceSinkError>
where
    E: FnMut(cpal::Error) + Send + Clone + 'static,
{
    let device = crate::capture::host()
        .default_output_device()
        .ok_or(rodio::DeviceSinkError::NoDevice)?;
    let key = device_key(&device);
    let sink = match rodio::DeviceSinkBuilder::from_device(device.clone())?
        .with_buffer_size(cpal::BufferSize::Default)
        .with_error_callback(error_callback.clone())
        .open_stream()
    {
        Ok(sink) => sink,
        Err(_) => rodio::DeviceSinkBuilder::from_device(device)?
            .with_error_callback(error_callback)
            .open_sink_or_fallback()?,
    };
    Ok((sink, key))
}

pub(crate) fn open_default_sink_simple() -> Result<rodio::MixerDeviceSink, rodio::DeviceSinkError> {
    open_default_sink(|err| log::debug!("audio output stream error: {err}")).map(|(sink, _)| sink)
}

#[cfg(test)]
mod tests {
    use super::should_reopen_on_error_kind;

    #[test]
    fn reopen_only_on_device_change_or_invalidation_errors() {
        assert!(should_reopen_on_error_kind(
            cpal::ErrorKind::StreamInvalidated
        ));
        assert!(should_reopen_on_error_kind(
            cpal::ErrorKind::DeviceNotAvailable
        ));
        assert!(should_reopen_on_error_kind(cpal::ErrorKind::DeviceChanged));
        assert!(!should_reopen_on_error_kind(cpal::ErrorKind::Xrun));
        assert!(!should_reopen_on_error_kind(
            cpal::ErrorKind::UnsupportedConfig
        ));
    }
}
