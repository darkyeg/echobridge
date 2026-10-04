//! The Windows backend: WASAPI in shared mode, opened the way Chrome opens it.
//!
//! The microphone uses `AUDCLNT_STREAMOPTIONS_RAW` where the device supports it, so Windows
//! voice effects (noise suppression, gating) cannot alter it before echo cancellation; that
//! processing is nonlinear and makes the playback leak impossible to model. Blocks carry
//! the audio engine's performance-counter time of their first sample, instead of a
//! callback-time estimate that jitters by about 0.7 ms. Invalid stamps recover onto the
//! same performance-counter axis, preserving a stable correction until a clock change.
//! Every stream runs on its own
//! thread at "Pro Audio" (MMCSS) priority.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{CloseHandle, HANDLE, PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDCLNT_STREAMOPTIONS_RAW, AudioCategory_Other, AudioClientProperties, DEVICE_STATE_ACTIVE, EDataFlow,
    IAudioCaptureClient, IAudioClient2, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eCapture, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    KSAUDIO_SPEAKER_MONO, SPEAKER_FRONT_LEFT, SPEAKER_FRONT_RIGHT, WAVE_FORMAT_EXTENSIBLE,
};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, STGM_READ,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::Variant::VT_BOOL;
use windows::core::{GUID, HSTRING, Interface};

use crate::stream::ThreadStream;
use crate::timestamps::CaptureClock;
use crate::{
    AudioBackend, BlockAssembler, CaptureCallback, CaptureInfo, Device, DeviceId, Direction, Error, Latency, RATE,
    RenderCallback, Source, Stream,
};

/// `PKEY_Devices_AudioDevice_RawProcessingSupported`, as Chrome checks it before asking
/// for RAW.
const RAW_PROCESSING_SUPPORTED: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0x8943b373_388c_4395_b557_bc6dbaffafdb), pid: 2 };
/// Capture buffer: 100 ms absorbs a stalled consumer; blocks are still read as soon as
/// they exist. In 100 ns units.
const CAPTURE_BUFFER: i64 = 1_000_000;
/// Render buffer: 20 ms, refilled on every device period.
const RENDER_BUFFER: i64 = 200_000;
/// How long a stream thread waits for its device before checking for a stop.
const WAIT_MS: u32 = 20;

#[derive(Debug)]
pub struct Wasapi;

impl AudioBackend for Wasapi {
    fn devices(&self, direction: Direction) -> Result<Vec<Device>, Error> {
        let _com = Com::init();
        let enumerator = enumerator()?;
        // SAFETY: plain COM calls on a valid enumerator and collection.
        let devices = unsafe { enumerator.EnumAudioEndpoints(flow(direction), DEVICE_STATE_ACTIVE) };
        let devices = devices.map_err(system("Listing audio devices"))?;
        // SAFETY: as above.
        let count = unsafe { devices.GetCount() }.map_err(system("Listing audio devices"))?;
        let mut result = Vec::new();
        for index in 0..count {
            // SAFETY: `index` is within the collection.
            let Ok(device) = (unsafe { devices.Item(index) }) else { continue };
            if let (Ok(id), Ok(name)) = (device_id(&device), friendly_name(&device)) {
                result.push(Device { id, name });
            }
        }
        Ok(result)
    }

    fn default_device(&self, direction: Direction) -> Result<Option<DeviceId>, Error> {
        let _com = Com::init();
        let enumerator = enumerator()?;
        // SAFETY: plain COM call.
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow(direction), eConsole) };
        device.ok().map(|device| device_id(&device)).transpose()
    }

    fn capture(
        &self,
        device: &DeviceId,
        source: Source,
        frames: usize,
        mut callback: CaptureCallback,
    ) -> Result<(Box<dyn Stream>, CaptureInfo), Error> {
        let device = device.clone();
        let name = match source {
            Source::Microphone => "EchoBridge microphone",
            Source::Loopback => "EchoBridge playback reference",
        };
        let (stream, info) = ThreadStream::spawn(name, move |stop, opened| {
            let _com = Com::init();
            let capture = Capture::open(&device, source)?;
            opened(capture.info);
            capture.run(stop, frames, &mut *callback)
        })?;
        Ok((Box::new(stream), info))
    }

    fn render(
        &self,
        device: &DeviceId,
        latency: Latency,
        mut callback: RenderCallback,
    ) -> Result<Box<dyn Stream>, Error> {
        let device = device.clone();
        let (stream, ()) = ThreadStream::spawn("EchoBridge output", move |stop, opened| {
            let _com = Com::init();
            let render = Render::open(&device, latency)?;
            opened(());
            render.run(stop, &mut *callback)
        })?;
        Ok(Box::new(stream))
    }
}

struct Capture {
    client: IAudioClient2,
    capture: IAudioCaptureClient,
    volume: Option<IAudioEndpointVolume>,
    event: Event,
    info: CaptureInfo,
    source: Source,
}

impl Capture {
    fn open(id: &DeviceId, source: Source) -> Result<Self, Error> {
        let loopback = source == Source::Loopback;
        let device = find_device(id)?;
        // Playback endpoints apply their master volume and mute after the loopback tap
        // (measured: the tap read -29.0 dBFS at both 0 and -20.5 dB master volume).
        let volume = if loopback {
            // SAFETY: plain COM activation.
            unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }.ok()
        } else {
            None
        };
        // SAFETY: plain COM activation.
        let client: IAudioClient2 =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(system("Opening the audio device"))?;
        let mut raw = false;
        if !loopback && raw_supported(&device) {
            // Category "Other" keeps Windows from ducking music as it does for calls.
            let properties = AudioClientProperties {
                cbSize: size_of::<AudioClientProperties>() as u32,
                eCategory: AudioCategory_Other,
                Options: AUDCLNT_STREAMOPTIONS_RAW,
                ..Default::default()
            };
            // SAFETY: `properties` is a valid, sized structure.
            raw = unsafe { client.SetClientProperties(&properties) }.is_ok();
        }
        let channels = mix_channels(&client)?;
        let mut flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let format = float_format(channels);
        let event = Event::new()?;
        // SAFETY: `format` is a valid WAVEFORMATEXTENSIBLE that outlives the call.
        let capture = unsafe {
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, CAPTURE_BUFFER, 0, &format.Format, None)
                .and_then(|()| client.SetEventHandle(event.0))
                .and_then(|()| client.GetService::<IAudioCaptureClient>())
                .and_then(|capture| client.Start().map(|()| capture))
        }
        .map_err(system("Starting the audio device"))?;
        Ok(Self { client, capture, volume, event, info: CaptureInfo { channels, raw }, source })
    }

    fn gain(&self) -> f32 {
        let Some(volume) = &self.volume else { return 1.0 };
        // SAFETY: plain COM calls.
        match unsafe { (volume.GetMasterVolumeLevel(), volume.GetMute()) } {
            (Ok(_), Ok(muted)) if muted.as_bool() => 0.0,
            (Ok(level_db), Ok(_)) => 10f32.powf(level_db / 20.0),
            _ => 1.0,
        }
    }

    fn run(
        self,
        stop: &AtomicBool,
        frames: usize,
        callback: &mut dyn FnMut(crate::CaptureBlock<'_>),
    ) -> Result<(), Error> {
        let channels = self.info.channels;
        let mut assembler = BlockAssembler::new(channels, frames);
        let mut gain = self.gain();
        let result = (|| {
            let mut frequency = 0i64;
            // SAFETY: the output pointer is valid. Read the counter frequency once per stream.
            unsafe { QueryPerformanceFrequency(&mut frequency) }.map_err(read_error)?;
            // SAFETY: plain COM call on the initialized capture client.
            let buffer_frames = unsafe { self.client.GetBufferSize() }.map_err(read_error)?;
            let buffer_seconds = f64::from(buffer_frames) / f64::from(RATE);
            let mut clock = CaptureClock::default();
            let mut last_warning = None;
            let mut reanchors = 0u64;
            let mut timestamp_errors = 0u64;
            while !stop.load(Ordering::Relaxed) {
                self.event.wait(WAIT_MS);
                loop {
                    // SAFETY: plain COM call.
                    let packet = unsafe { self.capture.GetNextPacketSize() }.map_err(read_error)?;
                    if packet == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let (mut count, mut flags, mut time) = (0u32, 0u32, 0u64);
                    // SAFETY: all out-pointers are valid; the buffer stays valid until
                    // ReleaseBuffer below.
                    unsafe { self.capture.GetBuffer(&mut data, &mut count, &mut flags, None, Some(&mut time)) }
                        .map_err(read_error)?;
                    let frames = count as usize;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                    // SAFETY: the device wrote `frames * channels` floats at `data`, in the
                    // format requested at initialization.
                    let samples =
                        (!silent).then(|| unsafe { std::slice::from_raw_parts(data.cast::<f32>(), frames * channels) });
                    let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                    let timestamp_error = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;
                    let mut counter = 0i64;
                    // SAFETY: the output pointer is valid; the counter shares the packet's epoch.
                    unsafe { QueryPerformanceCounter(&mut counter) }.map_err(read_error)?;
                    let now = counter as f64 / frequency as f64;
                    let reported = time as f64 * 1e-7;
                    let placed =
                        clock.place((!timestamp_error).then_some(reported), now, frames, buffer_seconds, discontinuity);
                    reanchors = reanchors.saturating_add(u64::from(placed.reanchored));
                    timestamp_errors = timestamp_errors.saturating_add(u64::from(timestamp_error));
                    if (placed.reanchored || timestamp_error) && last_warning.is_none_or(|last| now - last >= 30.0) {
                        log::warn!(
                            "capture timestamp recovered: loopback={} reported_age={:.1}ms correction={:.1}ms reanchors={} timestamp_errors={}",
                            self.source == Source::Loopback,
                            (now - reported) * 1000.0,
                            placed.offset * 1000.0,
                            reanchors,
                            timestamp_errors,
                        );
                        last_warning = Some(now);
                    }
                    assembler.push(samples, frames, placed.time, placed.discontinuity, gain, callback);
                    // SAFETY: releases exactly the frames obtained above.
                    unsafe { self.capture.ReleaseBuffer(count) }.map_err(read_error)?;
                }
                gain = self.gain();
            }
            Ok(())
        })();
        // SAFETY: plain COM call; stopping an already stopped client is harmless.
        unsafe { self.client.Stop() }.ok();
        result
    }
}

struct Render {
    client: IAudioClient2,
    render: IAudioRenderClient,
    event: Event,
    channels: usize,
    buffer_frames: u32,
}

impl Render {
    fn open(id: &DeviceId, latency: Latency) -> Result<Self, Error> {
        let device = find_device(id)?;
        let activate = || -> Result<IAudioClient3, Error> {
            // SAFETY: plain COM activation.
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(system("Opening the output device"))
        };
        let mut client = activate()?;
        let event = Event::new()?;
        let mut low_latency = false;
        if latency == Latency::Low {
            // SAFETY: `client` is a fresh, uninitialized audio client.
            low_latency = unsafe { initialize_low_latency(&client) };
            if !low_latency {
                client = activate()?; // a failed Initialize leaves the client unusable
            }
        }
        let channels = mix_channels(&client)?;
        if !low_latency {
            let format = float_format(channels);
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            // SAFETY: `format` is a valid WAVEFORMATEXTENSIBLE that outlives the call.
            unsafe { client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, RENDER_BUFFER, 0, &format.Format, None) }
                .map_err(system("Starting the output device"))?;
        }
        let client: IAudioClient2 = client.cast().map_err(system("Starting the output device"))?;
        // SAFETY: the client is initialized; the event outlives the stream.
        let (render, buffer_frames) = unsafe {
            client
                .SetEventHandle(event.0)
                .and_then(|()| client.GetService::<IAudioRenderClient>())
                .and_then(|render| client.GetBufferSize().map(|size| (render, size)))
        }
        .map_err(system("Starting the output device"))?;
        Ok(Self { client, render, event, channels, buffer_frames })
    }

    fn run(self, stop: &AtomicBool, callback: &mut dyn FnMut(&mut [f32])) -> Result<(), Error> {
        let mut mono = Vec::with_capacity(self.buffer_frames as usize);
        let result = (|| {
            // Start from a buffer of silence so the first period cannot run dry.
            // SAFETY: requests the whole buffer, which is empty before Start.
            unsafe {
                self.render.GetBuffer(self.buffer_frames)?;
                self.render.ReleaseBuffer(self.buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                self.client.Start()?;
            }
            while !stop.load(Ordering::Relaxed) {
                self.event.wait(WAIT_MS);
                // SAFETY: plain COM call.
                let padding = unsafe { self.client.GetCurrentPadding() }?;
                let available = self.buffer_frames.saturating_sub(padding);
                if available == 0 {
                    continue;
                }
                mono.resize(available as usize, 0.0);
                callback(&mut mono);
                // SAFETY: `available` frames fit in the free part of the buffer; it is
                // written in full and released immediately.
                unsafe {
                    let data = self.render.GetBuffer(available)?.cast::<f32>();
                    let out = std::slice::from_raw_parts_mut(data, available as usize * self.channels);
                    for (frame, &sample) in out.chunks_exact_mut(self.channels).zip(&mono) {
                        frame.fill(sample);
                    }
                    self.render.ReleaseBuffer(available, 0)?;
                }
            }
            Ok(())
        })()
        .map_err(read_error);
        // SAFETY: plain COM call.
        unsafe { self.client.Stop() }.ok();
        result
    }
}

/// COM for the current thread, released on drop if this call initialized it.
struct Com(bool);

impl Com {
    fn init() -> Self {
        // SAFETY: balanced by CoUninitialize in drop when it succeeded. A thread already in
        // a single-threaded apartment (a UI thread) keeps it and can still use these APIs.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self(result.is_ok() && result != RPC_E_CHANGED_MODE)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: matches the successful CoInitializeEx in `init`.
            unsafe { CoUninitialize() };
        }
    }
}

struct Event(HANDLE);

impl Event {
    fn new() -> Result<Self, Error> {
        // SAFETY: creates an unnamed auto-reset event.
        unsafe { CreateEventW(None, false, false, None) }.map(Self).map_err(system("Creating an audio event"))
    }

    fn wait(&self, milliseconds: u32) {
        // SAFETY: the handle is valid until drop.
        unsafe { WaitForSingleObject(self.0, milliseconds) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        // SAFETY: the handle is owned and closed once.
        unsafe { CloseHandle(self.0) }.ok();
    }
}

fn flow(direction: Direction) -> EDataFlow {
    match direction {
        Direction::Input => eCapture,
        Direction::Output => eRender,
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, Error> {
    // SAFETY: plain COM instantiation.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(system("Opening the Windows device list"))
}

fn find_device(id: &DeviceId) -> Result<IMMDevice, Error> {
    let enumerator = enumerator()?;
    // SAFETY: the id is a valid wide string for the duration of the call.
    unsafe { enumerator.GetDevice(&HSTRING::from(id.as_str())) }.map_err(|_| Error::DeviceNotFound)
}

fn device_id(device: &IMMDevice) -> Result<DeviceId, Error> {
    // SAFETY: GetId returns a CoTaskMem string that is copied and then freed.
    unsafe {
        let id = device.GetId().map_err(system("Reading a device id"))?;
        let text = id.to_string().map_err(|e| Error::System(e.to_string()));
        CoTaskMemFree(Some(id.0 as *const _));
        text
    }
}

fn property(device: &IMMDevice, key: &PROPERTYKEY) -> Option<PROPVARIANT> {
    // SAFETY: plain COM calls with a valid key.
    unsafe { device.OpenPropertyStore(STGM_READ).and_then(|store| store.GetValue(key)) }.ok()
}

fn friendly_name(device: &IMMDevice) -> Result<String, Error> {
    let value =
        property(device, &PKEY_Device_FriendlyName).ok_or_else(|| Error::System("Reading a device name".into()))?;
    Ok(value.to_string())
}

fn raw_supported(device: &IMMDevice) -> bool {
    property(device, &RAW_PROCESSING_SUPPORTED)
        .is_some_and(|value| value.vt() == VT_BOOL && bool::try_from(&value).unwrap_or(false))
}

/// Stereo for multichannel devices, mono for mono ones.
/// Initialize shared low-latency mode (Windows 10 and later) at the device's smallest
/// engine period, instead of the usual 10 ms period with a 20 ms buffer. It needs the
/// device's own mix format, so it applies when that is 32-bit float at [`RATE`]; otherwise
/// returns `false` and the caller falls back.
///
/// # Safety
/// `client` must be activated and not yet initialized.
unsafe fn initialize_low_latency(client: &IAudioClient3) -> bool {
    // SAFETY: GetMixFormat returns a CoTaskMem structure that is read, used and freed here.
    unsafe {
        let Ok(mix) = client.GetMixFormat() else { return false };
        let format = mix.read_unaligned(); // the format structures are packed
        let sub_format = || std::ptr::addr_of!((*mix.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat).read_unaligned();
        let float = format.wFormatTag == WAVE_FORMAT_IEEE_FLOAT as u16
            || (format.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 && sub_format() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
        let usable =
            float && { format.wBitsPerSample } == 32 && { format.nSamplesPerSec } == RATE && { format.nChannels } <= 2;
        let (mut default, mut fundamental, mut minimum, mut maximum) = (0, 0, 0, 0);
        let initialized = usable
            && client
                .GetSharedModeEnginePeriod(mix, &mut default, &mut fundamental, &mut minimum, &mut maximum)
                .is_ok()
            && client.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, minimum, mix, None).is_ok();
        CoTaskMemFree(Some(mix as *const _));
        initialized
    }
}

fn mix_channels(client: &IAudioClient2) -> Result<usize, Error> {
    // SAFETY: GetMixFormat returns a CoTaskMem structure that is read and freed.
    unsafe {
        let mix = client.GetMixFormat().map_err(system("Reading the device format"))?;
        let channels = if (*mix).nChannels > 1 { 2 } else { 1 };
        CoTaskMemFree(Some(mix as *const _));
        Ok(channels)
    }
}

/// 32-bit float at [`RATE`]; the audio engine converts from and to the device's own format.
fn float_format(channels: usize) -> WAVEFORMATEXTENSIBLE {
    let block_align = (4 * channels) as u16;
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE as u16,
            nChannels: channels as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * u32::from(block_align),
            nBlockAlign: block_align,
            wBitsPerSample: 32,
            cbSize: (size_of::<WAVEFORMATEXTENSIBLE>() - size_of::<WAVEFORMATEX>()) as u16,
        },
        Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: if channels == 2 { SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT } else { KSAUDIO_SPEAKER_MONO },
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

fn system(step: &'static str) -> impl Fn(windows::core::Error) -> Error {
    move |error| Error::System(format!("{step} failed ({error})"))
}

fn read_error(error: windows::core::Error) -> Error {
    if error.code() == AUDCLNT_E_DEVICE_INVALIDATED {
        Error::System("The audio device was disconnected.".into())
    } else {
        Error::System(format!("Audio stopped ({error})"))
    }
}
