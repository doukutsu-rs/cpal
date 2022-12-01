use std::convert::TryInto;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};
use std::vec::IntoIter as VecIntoIter;

use traits::{DeviceTrait, HostTrait, StreamTrait};
use StreamInstant;

use crate::{
    BackendSpecificError, BufferSize, BuildStreamError, Data, DefaultStreamConfigError,
    DeviceNameError, DevicesError, InputCallbackInfo, OutputCallbackInfo, PauseStreamError,
    PlayStreamError, Sample, SampleFormat, SampleRate, StreamConfig, StreamError,
    SupportedBufferSize, SupportedStreamConfig, SupportedStreamConfigRange,
    SupportedStreamConfigsError,
};

pub struct Device;

pub struct Host;

pub struct Stream {
    handle: Option<JoinHandle<()>>,
    message_tx: mpsc::Sender<StreamMessage>,
}

pub type SupportedInputConfigs = VecIntoIter<SupportedStreamConfigRange>;
pub type SupportedOutputConfigs = VecIntoIter<SupportedStreamConfigRange>;
pub type Devices = VecIntoIter<Device>;

static HOST_REFCOUNT: AtomicUsize = AtomicUsize::new(0);

pub fn lm_trace(dstr: &str) {
    println!("{}", dstr);
}

mod nx {
    #[repr(C)]
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    pub enum PcmFormat {
        Invalid = 0,
        Int8 = 1,
        Int16 = 2,
        Int24 = 3,
        Int32 = 4,
        Float = 5,
        Adpcm = 6,
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    pub enum AudioOutState {
        Started = 0,
        Stopped = 1,
    }

    #[repr(C)]
    #[derive(Debug, Copy, Clone)]
    pub struct AudioOutBuffer {
        pub next: *mut AudioOutBuffer,
        pub buffer: *mut std::ffi::c_void,
        pub buffer_size: u64,
        pub data_size: u64,
        pub data_offset: u64,
    }

    extern "C" {
        pub fn audoutInitialize() -> u32;
        pub fn audoutExit();

        pub fn audoutOpenAudioOut(
            device_name_in: *const u8,
            device_name_out: *mut u8,
            sample_rate_in: u32,
            channel_count_in: u32,
            sample_rate_out: *mut u32,
            channel_count_out: *mut u32,
            format: *mut PcmFormat,
            state: *mut AudioOutState,
        ) -> u32;
        pub fn audoutStartAudioOut() -> u32;
        pub fn audoutStopAudioOut() -> u32;

        pub fn audoutAppendAudioOutBuffer(buffer: *mut AudioOutBuffer) -> u32;
        pub fn audoutWaitPlayFinish(
            released: *mut *mut AudioOutBuffer,
            released_count: *mut u32,
            timeout: u64,
        ) -> u32;

        pub fn audoutGetSampleRate() -> u32;
        pub fn audoutGetChannelCount() -> u32;
        pub fn audoutGetPcmFormat() -> PcmFormat;
        pub fn audoutGetDeviceState() -> AudioOutState;

        pub fn memalign(alignment: usize, size: usize) -> *mut std::ffi::c_void;
    }
}

struct AlignedBuffer<T, const Alignment: usize> {
    inner: Vec<T>,
}

enum StreamMessage {
    Play,
    Pause,
    Stop,
}

impl<T, const Alignment: usize> AlignedBuffer<T, Alignment> {
    fn new(capacity: usize) -> Option<Self> {
        let size = match capacity.checked_mul(std::mem::size_of::<T>()) {
            None => return None,
            Some(size) => size,
        };

        if size == 0 {
            return None;
        }

        // alloc aligned ptr
        // let layout = match std::alloc::Layout::from_size_align(size, Alignment) {
        //     Ok(layout) => layout,
        //     Err(_) => return None,
        // };

        // let ptr = unsafe { std::alloc::alloc(layout) };
        let ptr = unsafe { nx::memalign(Alignment, size) };
        if ptr.is_null() {
            return None;
        }

        unsafe { std::ptr::write_bytes(ptr as *mut u8, 0, size) };

        Some(Self {
            inner: unsafe { Vec::from_raw_parts(ptr as *mut _, 0, capacity) },
        })
    }

    fn as_mut_ptr(&mut self) -> *mut T {
        self.inner.as_mut_ptr()
    }
}

impl<T, const Alignment: usize> Into<Vec<T>> for AlignedBuffer<T, Alignment> {
    fn into(self) -> Vec<T> {
        self.inner
    }
}

impl Host {
    #[allow(dead_code)]
    pub fn new() -> Result<Self, crate::HostUnavailable> {
        Ok(Host)
    }
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    #[inline]
    fn name(&self) -> Result<String, DeviceNameError> {
        Ok("default".to_owned())
    }

    #[inline]
    fn supported_input_configs(
        &self,
    ) -> Result<SupportedInputConfigs, SupportedStreamConfigsError> {
        Ok(Vec::new().into_iter())
    }

    #[inline]
    fn supported_output_configs(
        &self,
    ) -> Result<SupportedOutputConfigs, SupportedStreamConfigsError> {
        let sample_rate = unsafe { nx::audoutGetSampleRate() };
        let channels = unsafe { nx::audoutGetChannelCount() as u16 };
        let format = unsafe { nx::audoutGetPcmFormat() };

        let sample_format = match format {
            nx::PcmFormat::Int16 => SampleFormat::I16,
            nx::PcmFormat::Float => SampleFormat::F32,
            _ => {
                return Err(BackendSpecificError {
                    description: format!(
                        "audoutGetPcmFormat returned an invalid/unsupported sample format: {:?}",
                        format
                    ),
                }
                .into())
            }
        };

        let config = vec![SupportedStreamConfigRange {
            channels,
            min_sample_rate: SampleRate(sample_rate),
            max_sample_rate: SampleRate(sample_rate),
            buffer_size: SupportedBufferSize::Unknown,
            sample_format,
        }];

        Ok(config.into_iter())
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        let mut configs: Vec<_> = self.supported_input_configs().unwrap().collect();
        configs.sort_by(|a, b| b.cmp_default_heuristics(a));
        let config = configs
            .into_iter()
            .next()
            .ok_or(DefaultStreamConfigError::StreamTypeNotSupported)?
            .with_max_sample_rate();
        Ok(config)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, DefaultStreamConfigError> {
        let mut configs: Vec<_> = self.supported_output_configs().unwrap().collect();
        configs.sort_by(|a, b| b.cmp_default_heuristics(a));
        let config = configs
            .into_iter()
            .next()
            .ok_or(DefaultStreamConfigError::StreamTypeNotSupported)?
            .with_max_sample_rate();
        Ok(config)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        _config: &StreamConfig,
        _sample_format: SampleFormat,
        _data_callback: D,
        _error_callback: E,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        Err(BackendSpecificError {
            description: "Input is not supported yet.".to_owned(),
        }
        .into())
    }

    /// Create an output stream.
    fn build_output_stream_raw<D, E>(
        &self,
        config: &StreamConfig,
        sample_format: SampleFormat,
        mut data_callback: D,
        mut error_callback: E,
    ) -> Result<Self::Stream, BuildStreamError>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(StreamError) + Send + 'static,
    {
        let mut sample_rate_out = unsafe { nx::audoutGetSampleRate() };
        let mut channel_count_out = unsafe { nx::audoutGetChannelCount() };
        let mut pcm_format_out = unsafe { nx::audoutGetPcmFormat() };

        if sample_rate_out != config.sample_rate.0 {
            return Err(BackendSpecificError {
                description: format!("Incompatible sample rate specified: {}", sample_rate_out),
            }
            .into());
        }

        if channel_count_out != config.channels as u32 {
            return Err(BackendSpecificError {
                description: format!(
                    "Incompatible channel count specified: {}",
                    channel_count_out
                ),
            }
            .into());
        }

        match (sample_format, pcm_format_out) {
            (SampleFormat::I16, nx::PcmFormat::Int16) => (),
            (SampleFormat::F32, nx::PcmFormat::Float) => (),
            _ => {
                return Err(BackendSpecificError {
                    description: format!(
                        "Incompatible sample format specified: {:?}",
                        pcm_format_out
                    ),
                }
                .into());
            }
        }

        let buffer_size_samp = match config.buffer_size {
            BufferSize::Fixed(buffer_size) => buffer_size,
            BufferSize::Default => 480,
        } as usize;
        let buffer_size = buffer_size_samp * config.channels as usize * sample_format.sample_size();
        let aligned_size = (buffer_size + 0xFFF) & !0xFFF;

        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            const NUM_BUFFERS: usize = 2;
            let mut sample_buffer =
                match AlignedBuffer::<u8, 0x1000>::new(aligned_size * NUM_BUFFERS) {
                    Some(buf) => buf,
                    None => {
                        (error_callback)(
                            BackendSpecificError {
                                description: "Failed to allocate sample buffer.".to_owned(),
                            }
                            .into(),
                        );
                        return;
                    }
                };
            let mut buffers = Box::pin(Vec::with_capacity(NUM_BUFFERS));
            let mut released_buffer: *mut nx::AudioOutBuffer = std::ptr::null_mut();
            let mut released_out_count = 0u32;

            for i in 0..NUM_BUFFERS {
                buffers.push(nx::AudioOutBuffer {
                    next: std::ptr::null_mut(),
                    buffer: unsafe { sample_buffer.as_mut_ptr().add(i * aligned_size) as *mut _ },
                    buffer_size: aligned_size as u64,
                    data_size: buffer_size as u64,
                    data_offset: 0,
                });
            }

            let mut cur_buffer = 0;
            let mut next_buffer = 1;
            let mut paused = true;
            let creation_instant = Instant::now();

            loop {
                while let Some(msg) = if paused {
                    rx.recv().ok()
                } else {
                    rx.try_recv().ok()
                } {
                    match msg {
                        StreamMessage::Play => {
                            if paused {
                                let ret = unsafe { nx::audoutStartAudioOut() };
                                if ret != 0 {
                                    (error_callback)(StreamError::DeviceNotAvailable);
                                    return;
                                }
                                paused = false;
                            }
                        }
                        StreamMessage::Pause => {
                            if !paused {
                                let ret = unsafe { nx::audoutStopAudioOut() };
                                if ret != 0 {
                                    (error_callback)(StreamError::DeviceNotAvailable);
                                    return;
                                }
                                paused = true;
                            }
                        }
                        StreamMessage::Stop => {
                            unsafe { nx::audoutStopAudioOut() };
                            return;
                        }
                    }
                }

                if paused {
                    std::thread::yield_now();
                    continue;
                }

                let duration = creation_instant.elapsed();
                let callback = StreamInstant::new(
                    duration.as_secs().try_into().unwrap(),
                    duration.subsec_nanos(),
                );
                let buffer_duration =
                    frames_to_duration(buffer_size_samp, sample_rate_out as usize);
                let playback = callback
                    .add(buffer_duration)
                    .expect("`playback` occurs beyond representation supported by `StreamInstant`");
                let timestamp = crate::OutputStreamTimestamp { callback, playback };
                let cb_info = crate::OutputCallbackInfo { timestamp };

                cur_buffer = next_buffer;
                (data_callback)(
                    unsafe {
                        &mut Data::from_parts(
                            buffers[cur_buffer].buffer as *mut _,
                            buffer_size_samp * channel_count_out as usize,
                            sample_format,
                        )
                    },
                    &cb_info,
                );
                let ret = unsafe { nx::audoutAppendAudioOutBuffer(&mut buffers[cur_buffer]) };
                if ret != 0 {
                    (error_callback)(StreamError::DeviceNotAvailable);
                    return;
                }
                next_buffer = (cur_buffer + 1) % NUM_BUFFERS;

                let ret = unsafe {
                    nx::audoutWaitPlayFinish(
                        &mut released_buffer,
                        &mut released_out_count,
                        u64::MAX,
                    )
                };
                if ret != 0 {
                    (error_callback)(StreamError::DeviceNotAvailable);
                    return;
                }

                // lm_trace("loop");
                // std::thread::yield_now();
            }
        });

        Ok(Stream {
            handle: Some(thread),
            message_tx: tx,
        })
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.message_tx.send(StreamMessage::Stop);
        self.handle.take().unwrap().join().unwrap();
    }
}

impl HostTrait for Host {
    type Device = Device;
    type Devices = Devices;

    fn is_available() -> bool {
        true
    }

    fn devices(&self) -> Result<Self::Devices, DevicesError> {
        Ok(vec![Device].into_iter())
    }

    fn default_input_device(&self) -> Option<Device> {
        None
    }

    fn default_output_device(&self) -> Option<Device> {
        let ret = unsafe { nx::audoutInitialize() };
        if ret != 0 {
            return None;
        }

        Some(Device)
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), PlayStreamError> {
        println!("play");
        self.message_tx
            .send(StreamMessage::Play)
            .map_err(|_| PlayStreamError::DeviceNotAvailable)?;
        Ok(())
    }

    fn pause(&self) -> Result<(), PauseStreamError> {
        println!("pause");
        self.message_tx
            .send(StreamMessage::Pause)
            .map_err(|_| PauseStreamError::DeviceNotAvailable)?;
        Ok(())
    }
}

fn frames_to_duration(frames: usize, rate: usize) -> std::time::Duration {
    let secsf = frames as f64 / rate as f64;
    let secs = secsf as u64;
    let nanos = ((secsf - secs as f64) * 1_000_000_000.0) as u32;
    std::time::Duration::new(secs, nanos)
}
