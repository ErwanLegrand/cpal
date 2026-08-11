//! The WASAPI backend's implementation of [`WasapiDeviceExt`].
//!
//! The trait, [`ShareMode`] and [`WasapiStreamOptions`] all live in
//! [`crate::platform::wasapi_ext`] and are compiled on every platform; this is the half that
//! actually reaches an endpoint, and is the only half that is Windows-only.

use std::time::Duration;

use crate::{
    platform::{WasapiDeviceExt, WasapiStreamOptions},
    Data, Error, InputCallbackInfo, OutputCallbackInfo, SampleFormat, StreamConfig,
    SupportedStreamConfig,
};

impl WasapiDeviceExt for super::Device {
    type SupportedInputConfigs = super::SupportedInputConfigs;
    type SupportedOutputConfigs = super::SupportedOutputConfigs;
    type Stream = super::Stream;

    fn default_input_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        self.default_input_config_for(options.share_mode)
    }

    fn default_output_config_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<SupportedStreamConfig, Error> {
        self.default_output_config_for(options.share_mode)
    }

    fn supported_input_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedInputConfigs, Error> {
        self.supported_input_configs_for(options.share_mode)
    }

    fn supported_output_configs_with(
        &self,
        options: WasapiStreamOptions,
    ) -> Result<Self::SupportedOutputConfigs, Error> {
        self.supported_output_configs_for(options.share_mode)
    }

    fn build_input_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        options: WasapiStreamOptions,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_input_stream_raw_for(
            config,
            sample_format,
            options.share_mode,
            data_callback,
            error_callback,
            timeout,
        )
    }

    fn build_output_stream_raw_with<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        options: WasapiStreamOptions,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.build_output_stream_raw_for(
            config,
            sample_format,
            options.share_mode,
            data_callback,
            error_callback,
            timeout,
        )
    }
}
