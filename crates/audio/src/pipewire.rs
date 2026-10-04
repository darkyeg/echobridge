//! The Linux backend: PipeWire, which also serves PulseAudio apps through `pipewire-pulse`.
//!
//! The microphone is captured straight from its node, so no desktop voice effect sits in
//! front of it. The playback reference is the monitor of the headphones' sink. The clean
//! microphone leaves through a virtual microphone that EchoBridge creates itself, so no
//! virtual cable has to be installed; it appears to call apps as "EchoBridge Microphone"
//! while EchoBridge runs. Every stream runs its own PipeWire loop on its own thread.
//!
//! Blocks are stamped from the graph's sample position (see `graph_clock`), so the
//! microphone and the loopback share one steady monotonic axis.

use std::cell::{Cell, RefCell};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::metadata::{Metadata, MetadataListener};
use pw::properties::PropertiesBox;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags, StreamState};
use pw::types::ObjectType;
use spa::param::ParamType;
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils;
use spa::pod::serialize::PodSerializer;
use spa::pod::{Object, Pod, Value};
use spa::utils::{Direction as PwDirection, SpaTypes};

use crate::graph_clock::GraphClock;
use crate::stream::ThreadStream;
use crate::{
    AudioBackend, BlockAssembler, CaptureBlock, CaptureCallback, CaptureInfo, Device, DeviceId, Direction, Error,
    Latency, RATE, RenderCallback, Source, Stream,
};

/// The id and name of the virtual microphone EchoBridge creates.
pub(crate) const VIRTUAL_MICROPHONE_ID: &str = "echobridge_microphone";
pub(crate) const VIRTUAL_MICROPHONE_NAME: &str = "EchoBridge Microphone";

/// How long a stream may take to open, and the daemon to answer a query.
const OPEN_TIMEOUT: Duration = Duration::from_secs(5);
/// `SPA_CHUNK_FLAG_EMPTY`: the chunk holds no signal.
const CHUNK_FLAG_EMPTY: i32 = 1 << 1;
/// How often a stream thread checks for a stop request.
const POLL: Duration = Duration::from_millis(20);
/// The longest the device is assumed to buffer, for judging timestamps.
const NODE_LATENCY_LOW: u32 = 256;

#[derive(Debug)]
pub struct PipeWire;

impl AudioBackend for PipeWire {
    fn devices(&self, direction: Direction) -> Result<Vec<Device>, Error> {
        Ok(snapshot()?.devices(direction))
    }

    fn default_device(&self, direction: Direction) -> Result<Option<DeviceId>, Error> {
        let snapshot = snapshot()?;
        let default = match direction {
            Direction::Input => &snapshot.default_source,
            Direction::Output => &snapshot.default_sink,
        };
        Ok(default.clone().filter(|id| snapshot.devices(direction).iter().any(|device| &device.id == id)))
    }

    fn capture(
        &self,
        device: &DeviceId,
        source: Source,
        frames: usize,
        callback: CaptureCallback,
    ) -> Result<(Box<dyn Stream>, CaptureInfo), Error> {
        let direction = if source == Source::Loopback { Direction::Output } else { Direction::Input };
        require(device, direction)?;
        let device = device.clone();
        let name = match source {
            Source::Microphone => "EchoBridge microphone",
            Source::Loopback => "EchoBridge playback reference",
        };
        let (stream, info) = ThreadStream::spawn(name, move |stop, opened| {
            Capture { device, source, frames, callback }
                .run(stop, |channels| opened(CaptureInfo { channels, raw: source == Source::Microphone }))
        })?;
        Ok((Box::new(stream), info))
    }

    fn render(&self, device: &DeviceId, latency: Latency, callback: RenderCallback) -> Result<Box<dyn Stream>, Error> {
        if device != VIRTUAL_MICROPHONE_ID {
            require(device, Direction::Output)?;
        }
        let device = device.clone();
        let (stream, ()) = ThreadStream::spawn("EchoBridge output", move |stop, opened| {
            Render { device, latency, callback }.run(stop, || opened(()))
        })?;
        Ok(Box::new(stream))
    }
}

/// What the daemon lists right now.
#[derive(Debug, Default)]
struct Snapshot {
    nodes: Vec<Node>,
    default_sink: Option<String>,
    default_source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Node {
    /// `node.name`: stable across restarts, unlike the numeric id.
    id: String,
    description: String,
    class: String,
}

impl Snapshot {
    fn devices(&self, direction: Direction) -> Vec<Device> {
        let mut devices = Vec::new();
        if direction == Direction::Output {
            devices.push(Device { id: VIRTUAL_MICROPHONE_ID.into(), name: VIRTUAL_MICROPHONE_NAME.into() });
        }
        devices.extend(
            self.nodes
                .iter()
                .filter(|node| is_direction(&node.class, direction))
                .map(|node| Device { id: node.id.clone(), name: node.description.clone() }),
        );
        devices
    }
}

fn is_direction(class: &str, direction: Direction) -> bool {
    match direction {
        Direction::Input => matches!(class, "Audio/Source" | "Audio/Source/Virtual" | "Audio/Duplex"),
        Direction::Output => matches!(class, "Audio/Sink" | "Audio/Duplex"),
    }
}

/// Fail with `DeviceNotFound` unless `device` is connected, before a stream thread starts.
fn require(device: &DeviceId, direction: Direction) -> Result<(), Error> {
    let snapshot = snapshot()?;
    if snapshot.devices(direction).iter().any(|d| &d.id == device) { Ok(()) } else { Err(Error::DeviceNotFound) }
}

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::System(format!(
        "EchoBridge could not reach PipeWire ({error}). Make sure the pipewire and pipewire-pulse services are running."
    ))
}

/// The nodes and default devices on the daemon, from a short-lived connection.
fn snapshot() -> Result<Snapshot, Error> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(unavailable)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(unavailable)?;
    let core = context.connect_rc(None).map_err(unavailable)?;
    let registry = core.get_registry_rc().map_err(unavailable)?;

    let nodes = Rc::new(RefCell::new(Vec::new()));
    let defaults = Rc::new(RefCell::new((None::<String>, None::<String>)));
    let bound: Rc<RefCell<Vec<(Metadata, MetadataListener)>>> = Rc::default();
    let _listener = {
        let (nodes, defaults, bound, registry) = (nodes.clone(), defaults.clone(), bound.clone(), registry.clone());
        registry
            .clone()
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else { return };
                match global.type_ {
                    ObjectType::Node => {
                        let (Some(id), Some(class)) = (props.get("node.name"), props.get("media.class")) else {
                            return;
                        };
                        let description = ["node.description", "node.nick", "node.name"]
                            .iter()
                            .find_map(|key| props.get(key).filter(|value| !value.is_empty()))
                            .unwrap_or(id);
                        nodes.borrow_mut().push(Node {
                            id: id.into(),
                            description: description.into(),
                            class: class.into(),
                        });
                    }
                    ObjectType::Metadata if props.get("metadata.name") == Some("default") => {
                        let Ok(metadata) = registry.bind::<Metadata, _>(global) else { return };
                        let defaults = defaults.clone();
                        let listener = metadata
                            .add_listener_local()
                            .property(move |_, key, _, value| {
                                let name = value.and_then(default_name);
                                match key {
                                    Some("default.audio.sink") => defaults.borrow_mut().0 = name,
                                    Some("default.audio.source") => defaults.borrow_mut().1 = name,
                                    _ => {}
                                }
                                0
                            })
                            .register();
                        bound.borrow_mut().push((metadata, listener));
                    }
                    _ => {}
                }
            })
            .register()
    };
    // The first round trip delivers the objects, the second the metadata bound while it ran.
    roundtrip(&mainloop, &core)?;
    roundtrip(&mainloop, &core)?;
    let (default_sink, default_source) = defaults.take();
    Ok(Snapshot { nodes: nodes.take(), default_sink, default_source })
}

/// The `name` in a default-device metadata value such as `{"name":"alsa_output.pci"}`.
fn default_name(value: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(value).ok()?;
    value.get("name")?.as_str().map(String::from)
}

/// Wait until the daemon has answered everything asked so far.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) -> Result<(), Error> {
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0).map_err(unavailable)?;
    let _core_listener = {
        let (done, mainloop) = (done.clone(), mainloop.clone());
        core.add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    done.set(true);
                    mainloop.quit();
                }
            })
            .register()
    };
    let timeout = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    timeout.update_timer(Some(OPEN_TIMEOUT), None);
    mainloop.run();
    if done.get() { Ok(()) } else { Err(unavailable("no answer")) }
}

/// What a running stream's callbacks tell its thread.
#[derive(Debug, Default)]
struct Shared {
    failure: RefCell<Option<String>>,
    ready: Cell<bool>,
    /// Channels of the negotiated format, for capture.
    channels: Cell<usize>,
}

impl Shared {
    fn fail(&self, message: impl Into<String>) {
        self.failure.borrow_mut().get_or_insert_with(|| message.into());
    }
}

/// A PipeWire connection with a stream whose callbacks are installed by the caller, run
/// until the stop flag is set, the daemon goes away or the stream fails.
struct Connection {
    mainloop: pw::main_loop::MainLoopRc,
    core: pw::core::CoreRc,
    _context: pw::context::ContextRc,
    shared: Rc<Shared>,
    _core_listener: pw::core::Listener,
}

impl Connection {
    fn open() -> Result<Self, Error> {
        init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(unavailable)?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(unavailable)?;
        let core = context.connect_rc(None).map_err(unavailable)?;
        let shared = Rc::new(Shared::default());
        let core_listener = {
            let (shared, mainloop) = (shared.clone(), mainloop.clone());
            core.add_listener_local()
                .error(move |id, _, _, message| {
                    if id == pw::core::PW_ID_CORE {
                        shared.fail(format!("The audio system stopped: {message}"));
                        mainloop.quit();
                    }
                })
                .register()
        };
        Ok(Self { mainloop, core, _context: context, shared, _core_listener: core_listener })
    }

    /// Run the loop until the stream is ready, calling `ready` once; then until stopped.
    fn run(&self, stop: &Arc<AtomicBool>, ready: impl FnOnce()) -> Result<(), Error> {
        let deadline = Instant::now() + OPEN_TIMEOUT;
        let announced = Rc::new(Cell::new(false));
        let timer = self.mainloop.loop_().add_timer({
            let (mainloop, shared, stop, announced) =
                (self.mainloop.clone(), self.shared.clone(), stop.clone(), announced.clone());
            move |_| {
                let opening = !announced.get();
                let wake = stop.load(Ordering::Relaxed)
                    || shared.failure.borrow().is_some()
                    || (opening && (shared.ready.get() || Instant::now() >= deadline));
                if wake {
                    mainloop.quit();
                }
            }
        });
        timer.update_timer(Some(POLL), Some(POLL));
        let mut ready = Some(ready);
        loop {
            self.mainloop.run();
            if let Some(message) = self.shared.failure.borrow().clone() {
                return Err(Error::System(message));
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            if self.shared.ready.get() {
                if let Some(ready) = ready.take() {
                    announced.set(true);
                    ready();
                }
            } else if !announced.get() && Instant::now() >= deadline {
                return Err(Error::System("The audio device did not start.".into()));
            }
        }
    }
}

fn fail_on_state(shared: &Rc<Shared>, was_ready: &Cell<bool>, new: &StreamState) {
    match new {
        StreamState::Error(message) => shared.fail(format!("The audio device failed: {message}")),
        StreamState::Unconnected if was_ready.get() => shared.fail("The audio device was disconnected."),
        StreamState::Paused | StreamState::Streaming => was_ready.set(true),
        _ => {}
    }
}

fn format_pod(info: AudioInfoRaw) -> Vec<u8> {
    let object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
        .expect("serializing a format")
        .0
        .into_inner()
}

/// 48 kHz 32-bit float, in the device's own channel layout unless `channels` says otherwise.
fn raw_format(channels: Option<u32>) -> AudioInfoRaw {
    let mut info = AudioInfoRaw::new();
    info.set_format(AudioFormat::F32LE);
    info.set_rate(RATE);
    if let Some(channels) = channels {
        info.set_channels(channels);
        let mut position = [0; 64];
        position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
        info.set_position(position);
    }
    info
}

fn base_properties(category: &str, name: &str) -> PropertiesBox {
    let mut props = PropertiesBox::new();
    props.insert(*pw::keys::MEDIA_TYPE, "Audio");
    props.insert(*pw::keys::MEDIA_CATEGORY, category);
    // No "Communication" role: the desktop would lower other apps' volume for it.
    props.insert(*pw::keys::MEDIA_ROLE, "Production");
    props.insert(*pw::keys::APP_NAME, "EchoBridge");
    props.insert(*pw::keys::NODE_NAME, name);
    // A removed device ends the stream, as on Windows, instead of moving it elsewhere.
    props.insert(*pw::keys::NODE_DONT_RECONNECT, "true");
    props
}

struct Capture {
    device: DeviceId,
    source: Source,
    frames: usize,
    callback: CaptureCallback,
}

struct CaptureState {
    info: AudioInfoRaw,
    channels: usize,
    frames: usize,
    assembler: Option<BlockAssembler>,
    scratch: Vec<f32>,
    clock: GraphClock,
    callback: CaptureCallback,
    shared: Rc<Shared>,
    was_ready: Rc<Cell<bool>>,
}

impl Capture {
    fn run(self, stop: &Arc<AtomicBool>, opened: impl FnOnce(usize)) -> Result<(), Error> {
        let connection = Connection::open()?;
        let mut props = base_properties(
            "Capture",
            match self.source {
                Source::Microphone => "echobridge_capture_microphone",
                Source::Loopback => "echobridge_capture_playback",
            },
        );
        props.insert(*pw::keys::TARGET_OBJECT, self.device.as_str());
        props.insert(*pw::keys::NODE_LATENCY, format!("{}/{RATE}", self.frames));
        if self.source == Source::Loopback {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        }
        let stream = StreamBox::new(&connection.core, "EchoBridge capture", props).map_err(unavailable)?;
        let shared = connection.shared.clone();
        let was_ready = Rc::new(Cell::new(false));
        let state = CaptureState {
            info: AudioInfoRaw::new(),
            channels: 0,
            frames: self.frames,
            assembler: None,
            scratch: Vec::new(),
            clock: GraphClock::default(),
            callback: self.callback,
            shared: shared.clone(),
            was_ready: was_ready.clone(),
        };
        let _listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(|_, state, _, new| fail_on_state(&state.shared, &state.was_ready, &new))
            .param_changed(|_, state, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else { return };
                if !matches!(format_utils::parse_format(param), Ok((MediaType::Audio, MediaSubtype::Raw))) {
                    return;
                }
                if state.info.parse(param).is_err() || state.info.rate() != RATE || state.info.channels() == 0 {
                    state.shared.fail("The audio device did not accept 48 kHz audio.");
                    return;
                }
                state.channels = state.info.channels() as usize;
                state.assembler = Some(BlockAssembler::new(state.channels, state.frames));
                state.shared.channels.set(state.channels);
                state.clock.reset();
                state.shared.ready.set(true);
            })
            .process(capture_process)
            .register()
            .map_err(unavailable)?;
        let pod = format_pod(raw_format(None));
        let mut params = [Pod::from_bytes(&pod).expect("a valid format")];
        stream
            .connect(
                PwDirection::Input,
                None,
                StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::DONT_RECONNECT,
                &mut params,
            )
            .map_err(unavailable)?;
        connection.run(stop, || opened(shared.channels.get()))
    }
}

fn capture_process(stream: &pw::stream::Stream, state: &mut CaptureState) {
    let Some(assembler) = state.assembler.as_mut() else { return };
    let Some(mut buffer) = stream.dequeue_buffer() else { return };
    let Some(data) = buffer.datas_mut().first_mut() else { return };
    let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
    let empty = data.chunk().flags().bits() & CHUNK_FLAG_EMPTY != 0;
    let Some(bytes) = data.data() else { return };
    let bytes = &bytes[offset.min(bytes.len())..(offset + size).min(bytes.len())];
    let frames = bytes.len() / (4 * state.channels);
    if frames == 0 {
        return;
    }
    // SAFETY: pw_time is plain data, for which all zeros is valid.
    let mut time: pw::sys::pw_time = unsafe { std::mem::zeroed() };
    // SAFETY: `time` is a valid pw_time and the size passed is its size; the stream is live.
    unsafe { pw::sys::pw_stream_get_time_n(stream.as_raw_ptr(), &mut time, size_of::<pw::sys::pw_time>()) };
    let rate = if time.rate.denom == 0 {
        f64::from(RATE)
    } else {
        f64::from(time.rate.denom) / f64::from(time.rate.num.max(1))
    };
    let placed = state.clock.place(
        time.now as f64 * 1e-9,
        time.ticks as f64 / rate,
        time.delay as f64 / rate,
        frames as f64 / f64::from(RATE),
    );
    // SAFETY: every bit pattern is a valid f32.
    let (head, aligned, _) = unsafe { bytes.align_to::<f32>() };
    let samples = if head.is_empty() {
        &aligned[..frames * state.channels]
    } else {
        // PipeWire aligns its buffers, so this is a defensive copy that should never run.
        state.scratch.clear();
        state
            .scratch
            .extend(bytes.as_chunks::<4>().0.iter().take(frames * state.channels).map(|b| f32::from_le_bytes(*b)));
        &state.scratch[..]
    };
    let callback = &mut state.callback;
    assembler.push(
        (!empty).then_some(samples),
        frames,
        placed.time,
        placed.discontinuity,
        1.0,
        &mut |block: CaptureBlock<'_>| callback(block),
    );
}

struct Render {
    device: DeviceId,
    latency: Latency,
    callback: RenderCallback,
}

struct RenderState {
    info: AudioInfoRaw,
    channels: usize,
    mono: Vec<f32>,
    callback: RenderCallback,
    shared: Rc<Shared>,
    was_ready: Rc<Cell<bool>>,
    virtual_source: bool,
}

impl Render {
    fn run(self, stop: &Arc<AtomicBool>, opened: impl FnOnce()) -> Result<(), Error> {
        let virtual_source = self.device == VIRTUAL_MICROPHONE_ID;
        let connection = Connection::open()?;
        let mut props = base_properties("Playback", "echobridge_playback");
        if self.latency == Latency::Low {
            props.insert(*pw::keys::NODE_LATENCY, format!("{NODE_LATENCY_LOW}/{RATE}"));
        }
        if virtual_source {
            props.insert(*pw::keys::NODE_NAME, VIRTUAL_MICROPHONE_ID);
            props.insert(*pw::keys::MEDIA_CLASS, "Audio/Source");
            props.insert(*pw::keys::NODE_DESCRIPTION, VIRTUAL_MICROPHONE_NAME);
            props.insert(*pw::keys::NODE_NICK, VIRTUAL_MICROPHONE_NAME);
            props.insert(*pw::keys::NODE_VIRTUAL, "true");
        } else {
            props.insert(*pw::keys::TARGET_OBJECT, self.device.as_str());
        }
        let stream = StreamBox::new(&connection.core, "EchoBridge output", props).map_err(unavailable)?;
        let was_ready = Rc::new(Cell::new(false));
        let state = RenderState {
            info: AudioInfoRaw::new(),
            channels: if virtual_source { 2 } else { 0 },
            mono: Vec::new(),
            callback: self.callback,
            shared: connection.shared.clone(),
            was_ready: was_ready.clone(),
            virtual_source,
        };
        let _listener = stream
            .add_local_listener_with_user_data(state)
            .state_changed(|_, state, _, new| {
                fail_on_state(&state.shared, &state.was_ready, &new);
                // A virtual microphone has no peer to negotiate with until an app records.
                if state.virtual_source && matches!(new, StreamState::Paused | StreamState::Streaming) {
                    state.shared.ready.set(true);
                }
            })
            .param_changed(|_, state, id, param| {
                if id != ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else { return };
                if !matches!(format_utils::parse_format(param), Ok((MediaType::Audio, MediaSubtype::Raw))) {
                    return;
                }
                if state.info.parse(param).is_err() || state.info.rate() != RATE || state.info.channels() == 0 {
                    state.shared.fail("The audio device did not accept 48 kHz audio.");
                    return;
                }
                state.channels = state.info.channels() as usize;
                state.shared.ready.set(true);
            })
            .process(render_process)
            .register()
            .map_err(unavailable)?;
        let pod = format_pod(raw_format(virtual_source.then_some(2)));
        let mut params = [Pod::from_bytes(&pod).expect("a valid format")];
        let flags = if virtual_source {
            StreamFlags::MAP_BUFFERS
        } else {
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS | StreamFlags::DONT_RECONNECT
        };
        stream.connect(PwDirection::Output, None, flags, &mut params).map_err(unavailable)?;
        connection.run(stop, opened)
    }
}

fn render_process(stream: &pw::stream::Stream, state: &mut RenderState) {
    if state.channels == 0 {
        return;
    }
    let Some(mut buffer) = stream.dequeue_buffer() else { return };
    let requested = buffer.requested() as usize;
    let Some(data) = buffer.datas_mut().first_mut() else { return };
    let stride = 4 * state.channels;
    let capacity = data.as_raw().maxsize as usize / stride;
    let frames = if requested == 0 { capacity } else { requested.min(capacity) };
    let Some(bytes) = data.data() else { return };
    state.mono.clear();
    state.mono.resize(frames, 0.0);
    (state.callback)(&mut state.mono);
    for (frame, &sample) in bytes.chunks_exact_mut(stride).zip(&state.mono) {
        for channel in frame.as_chunks_mut::<4>().0 {
            *channel = sample.to_le_bytes();
        }
    }
    let chunk = data.chunk_mut();
    *chunk.offset_mut() = 0;
    *chunk.stride_mut() = stride as i32;
    *chunk.size_mut() = (frames * stride) as u32;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, class: &str) -> Node {
        Node { id: id.into(), description: format!("{id} description"), class: class.into() }
    }

    #[test]
    fn devices_are_sorted_by_direction_and_the_virtual_microphone_is_an_output() {
        let snapshot = Snapshot {
            nodes: vec![
                node("alsa_input.usb", "Audio/Source"),
                node("alsa_output.pci", "Audio/Sink"),
                node("bluez_headset", "Audio/Duplex"),
                node("some_stream", "Stream/Output/Audio"),
            ],
            ..Snapshot::default()
        };
        let inputs: Vec<_> = snapshot.devices(Direction::Input).into_iter().map(|d| d.id).collect();
        assert_eq!(inputs, ["alsa_input.usb", "bluez_headset"]);
        let outputs: Vec<_> = snapshot.devices(Direction::Output).into_iter().map(|d| d.id).collect();
        assert_eq!(outputs, [VIRTUAL_MICROPHONE_ID, "alsa_output.pci", "bluez_headset"]);
    }

    #[test]
    fn default_device_names_come_from_metadata_json() {
        assert_eq!(default_name(r#"{"name":"alsa_output.pci-0000"}"#).as_deref(), Some("alsa_output.pci-0000"));
        assert_eq!(default_name("not json"), None);
        assert_eq!(default_name(r#"{"other":1}"#), None);
    }
}
