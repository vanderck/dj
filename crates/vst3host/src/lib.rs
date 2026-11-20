//! A small, focused VST3 *effect* host for dj's channel inserts.
//!
//! * Loads `.vst3` bundles on Linux, macOS and Windows.
//! * Enumerates audio effect classes (with vendor and sub-categories for the
//!   insert browser).
//! * Runs the processor on the audio thread ([`Vst3Effect`], implementing
//!   the engine's [`Effect`] trait) while the UI keeps a [`Vst3Handle`] for
//!   parameters, display strings, state and latency.
//! * Feeds a full `ProcessContext` (tempo, musical position, time signature)
//!   so tempo-synced plugins lock to the master clock.

#![allow(non_snake_case)]

mod com;

use std::collections::HashMap;
use std::ffi::{c_void, CString};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use dj_engine::dsp::Frame;
use dj_engine::fx::{Effect, FxCtx};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper, Interface};

use com::{ComponentHandler, HostApplication, MemoryStream, ParamChanges};

pub use com::{cstring, wstring};

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(s: impl Into<String>) -> Result<T, Error> {
    Err(Error(s.into()))
}

/// A plugin class found in a bundle.
#[derive(Clone, Debug, PartialEq)]
pub struct PluginInfo {
    pub bundle: PathBuf,
    pub cid: [u8; 16],
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// e.g. "Fx|Delay"
    pub sub_categories: String,
}

impl PluginInfo {
    pub fn is_instrument(&self) -> bool {
        self.sub_categories.contains("Instrument") && !self.sub_categories.contains("Fx")
    }

    /// Stable identifier: `vst3:<hex cid>`.
    pub fn id(&self) -> String {
        let mut s = String::from("vst3:");
        for b in self.cid {
            s.push_str(&format!("{b:02X}"));
        }
        s
    }

    pub fn cid_from_id(id: &str) -> Option<[u8; 16]> {
        let hex = id.strip_prefix("vst3:")?;
        if hex.len() != 32 {
            return None;
        }
        let mut out = [0u8; 16];
        for i in 0..16 {
            out[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(out)
    }
}

// ------------------------------------------------------------ discovery

/// Standard VST3 locations for this platform (plus `$VST3_PATH`).
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("VST3_PATH") {
        v.extend(std::env::split_paths(&p));
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join(".vst3"));
        }
        v.push("/usr/lib/vst3".into());
        v.push("/usr/local/lib/vst3".into());
    }
    #[cfg(target_os = "macos")]
    {
        if let Some(h) = std::env::var_os("HOME") {
            v.push(PathBuf::from(h).join("Library/Audio/Plug-Ins/VST3"));
        }
        v.push("/Library/Audio/Plug-Ins/VST3".into());
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(p) = std::env::var_os("CommonProgramFiles") {
            v.push(PathBuf::from(p).join("VST3"));
        }
        if let Some(p) = std::env::var_os("LOCALAPPDATA") {
            v.push(PathBuf::from(p).join("Programs/Common/VST3"));
        }
    }
    v
}

/// Find `.vst3` bundles (directories or single files) below `roots`.
pub fn find_bundles(roots: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
        if depth > 6 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map(|x| x.eq_ignore_ascii_case("vst3")).unwrap_or(false) {
                out.push(p);
            } else if p.is_dir() {
                walk(&p, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for r in roots {
        walk(r, 0, &mut out);
    }
    out.sort();
    out.dedup();
    out
}

fn binary_path(bundle: &Path) -> Result<PathBuf, Error> {
    if bundle.is_file() {
        return Ok(bundle.to_path_buf());
    }
    let stem = bundle.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    #[cfg(target_os = "linux")]
    let p = bundle.join("Contents").join(format!("{}-linux", std::env::consts::ARCH)).join(format!("{stem}.so"));
    #[cfg(target_os = "macos")]
    let p = bundle.join("Contents/MacOS").join(&stem);
    #[cfg(target_os = "windows")]
    let p = {
        let arch = if cfg!(target_arch = "aarch64") { "arm64-win" } else { "x86_64-win" };
        bundle.join("Contents").join(arch).join(format!("{stem}.vst3"))
    };
    if p.exists() {
        Ok(p)
    } else {
        err(format!("no binary for this platform in {}", bundle.display()))
    }
}

// ------------------------------------------------------------ modules

pub struct Module {
    _lib: libloading::Library,
    factory: ComPtr<IPluginFactory>,
    pub path: PathBuf,
}

// SAFETY: the factory is only used from non-realtime threads behind a Mutex'd cache.
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

type GetFactory = unsafe extern "system" fn() -> *mut IPluginFactory;

fn modules() -> &'static Mutex<HashMap<PathBuf, Arc<Module>>> {
    static M: OnceLock<Mutex<HashMap<PathBuf, Arc<Module>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

impl Module {
    /// Load (or fetch the cached) module for a bundle. Modules stay loaded
    /// for the life of the process: unloading plugin binaries is a common
    /// source of crashes, and reloading is slow.
    pub fn load(bundle: &Path) -> Result<Arc<Module>, Error> {
        let key = bundle.to_path_buf();
        if let Some(m) = modules().lock().unwrap().get(&key) {
            return Ok(m.clone());
        }
        let bin = binary_path(bundle)?;
        let lib = unsafe { open_and_enter(&bin, bundle)? };
        let get: libloading::Symbol<GetFactory> =
            unsafe { lib.get(b"GetPluginFactory\0") }.map_err(|e| Error(format!("GetPluginFactory: {e}")))?;
        let raw = unsafe { get() };
        let factory = unsafe { ComPtr::from_raw(raw) }.ok_or_else(|| Error("plugin returned no factory".into()))?;
        let m = Arc::new(Module { _lib: lib, factory, path: key.clone() });
        modules().lock().unwrap().insert(key, m.clone());
        Ok(m)
    }

    pub fn classes(&self) -> Vec<PluginInfo> {
        let mut out = Vec::new();
        unsafe {
            let f2 = self.factory.cast::<IPluginFactory2>();
            let mut finfo: PFactoryInfo = std::mem::zeroed();
            let factory_vendor =
                if self.factory.getFactoryInfo(&mut finfo) == kResultOk { cstring(&finfo.vendor) } else { String::new() };
            for i in 0..self.factory.countClasses() {
                let mut info: PClassInfo = std::mem::zeroed();
                if self.factory.getClassInfo(i, &mut info) != kResultOk {
                    continue;
                }
                if cstring(&info.category) != "Audio Module Class" {
                    continue;
                }
                let mut p = PluginInfo {
                    bundle: self.path.clone(),
                    cid: *(&info.cid as *const TUID as *const [u8; 16]),
                    name: cstring(&info.name),
                    vendor: factory_vendor.clone(),
                    version: String::new(),
                    sub_categories: "Fx".into(),
                };
                if let Some(f2) = &f2 {
                    let mut i2: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut i2) == kResultOk {
                        p.sub_categories = cstring(&i2.subCategories);
                        let v = cstring(&i2.vendor);
                        if !v.is_empty() {
                            p.vendor = v;
                        }
                        p.version = cstring(&i2.version);
                    }
                }
                out.push(p);
            }
        }
        out
    }

    unsafe fn create<I: Interface>(&self, cid: &[u8; 16]) -> Option<ComPtr<I>> {
        let mut obj: *mut c_void = std::ptr::null_mut();
        let r = self.factory.createInstance(cid.as_ptr() as FIDString, I::IID.as_ptr() as FIDString, &mut obj);
        if r != kResultOk || obj.is_null() {
            return None;
        }
        ComPtr::from_raw(obj as *mut I)
    }
}

#[cfg(target_os = "linux")]
unsafe fn open_and_enter(bin: &Path, _bundle: &Path) -> Result<libloading::Library, Error> {
    use libloading::os::unix::{Library, RTLD_LOCAL, RTLD_NOW};
    let lib = Library::open(Some(bin), RTLD_NOW | RTLD_LOCAL).map_err(|e| Error(format!("dlopen: {e}")))?;
    let handle = lib.into_raw();
    let lib = Library::from_raw(handle);
    type Entry = unsafe extern "system" fn(*mut c_void) -> bool;
    if let Ok(entry) = lib.get::<Entry>(b"ModuleEntry\0") {
        if !entry(handle) {
            return err("ModuleEntry failed");
        }
    }
    Ok(lib.into())
}

#[cfg(target_os = "windows")]
unsafe fn open_and_enter(bin: &Path, _bundle: &Path) -> Result<libloading::Library, Error> {
    let lib = libloading::Library::new(bin).map_err(|e| Error(format!("LoadLibrary: {e}")))?;
    type Entry = unsafe extern "system" fn() -> bool;
    if let Ok(entry) = lib.get::<Entry>(b"InitDll\0") {
        if !entry() {
            return err("InitDll failed");
        }
    }
    Ok(lib)
}

#[cfg(target_os = "macos")]
unsafe fn open_and_enter(bin: &Path, bundle: &Path) -> Result<libloading::Library, Error> {
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFURLCreateFromFileSystemRepresentation(a: *const c_void, b: *const u8, l: isize, d: u8) -> *const c_void;
        fn CFBundleCreate(a: *const c_void, url: *const c_void) -> *mut c_void;
        fn CFRelease(cf: *const c_void);
    }
    let lib = libloading::Library::new(bin).map_err(|e| Error(format!("dlopen: {e}")))?;
    let path = bundle.to_string_lossy();
    let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), path.as_ptr(), path.len() as isize, 1);
    let cfb = CFBundleCreate(std::ptr::null(), url);
    CFRelease(url);
    type Entry = unsafe extern "system" fn(*mut c_void) -> bool;
    let entry = lib.get::<Entry>(b"bundleEntry\0").or_else(|_| lib.get::<Entry>(b"BundleEntry\0"));
    if let Ok(entry) = entry {
        if !entry(cfb) {
            return err("bundleEntry failed");
        }
    }
    // The bundle ref must outlive the module; it is intentionally leaked.
    Ok(lib)
}

/// Load a bundle and list its effect classes. Prefer running this in a
/// child process (see the app's `--vst3-scan`) so a broken plugin can't take
/// the host down.
pub fn scan_bundle(bundle: &Path) -> Result<Vec<PluginInfo>, Error> {
    Ok(Module::load(bundle)?.classes())
}

// ------------------------------------------------------------ instances

#[derive(Clone, Debug)]
pub struct ParamInfo {
    pub id: u32,
    pub title: String,
    pub units: String,
    pub steps: i32,
    pub default: f64,
}

/// Serialized plugin state (component + controller chunks).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub component: Vec<u8>,
    pub controller: Vec<u8>,
}

struct Core {
    _module: Arc<Module>,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    separate_controller: bool,
    connections: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    handler: ComWrapper<ComponentHandler>,
    _host: ComWrapper<HostApplication>,
}

// SAFETY: VST3 defines which thread calls which method; we only call
// `process` from the audio thread and everything else from the UI thread.
unsafe impl Send for Core {}
unsafe impl Sync for Core {}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            if let Some((a, b)) = &self.connections {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                if self.separate_controller {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Bus {
    channels: usize,
}

/// UI-side handle to a running plugin instance.
#[derive(Clone)]
pub struct Vst3Handle {
    core: Arc<Core>,
    pub info: PluginInfo,
    pub params: Arc<Vec<ParamInfo>>,
}

/// Audio-side processor, installed into a channel insert slot.
pub struct Vst3Effect {
    core: Arc<Core>,
    param_ids: Vec<u32>,
    pending: Vec<(u32, f64)>,
    in_changes: ComWrapper<ParamChanges>,
    out_changes: ComWrapper<ParamChanges>,
    in_buses: Vec<Bus>,
    out_buses: Vec<Bus>,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    in_abb: Vec<AudioBusBuffers>,
    out_abb: Vec<AudioBusBuffers>,
    max_block: usize,
    started: bool,
    failed: bool,
    frames: i64,
    latency: usize,
}

// SAFETY: raw pointers point into buffers owned by this struct.
unsafe impl Send for Vst3Effect {}

impl Vst3Handle {
    pub fn latency(&self) -> u32 {
        unsafe { self.core.processor.getLatencySamples() }
    }

    /// Restart flags reported by the plugin since the last call.
    pub fn take_restart_flags(&self) -> i32 {
        self.core.handler.restart_flags.swap(0, Ordering::AcqRel)
    }

    pub fn latency_changed(flags: i32) -> bool {
        flags & RestartFlags_::kLatencyChanged != 0
    }

    /// Parameter edits made inside the plugin (e.g. in its own editor).
    pub fn take_edits(&self) -> Vec<(u32, f64)> {
        self.core.handler.edits.lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
    }

    pub fn param_index(&self, id: u32) -> Option<usize> {
        self.params.iter().position(|p| p.id == id)
    }

    pub fn get_param(&self, index: usize) -> f64 {
        match (&self.core.controller, self.params.get(index)) {
            (Some(c), Some(p)) => unsafe { c.getParamNormalized(p.id) },
            _ => 0.0,
        }
    }

    /// Mirror a value into the edit controller (the processor gets it via the
    /// insert's `set_param`).
    pub fn set_param(&self, index: usize, value: f64) {
        if let (Some(c), Some(p)) = (&self.core.controller, self.params.get(index)) {
            unsafe {
                c.setParamNormalized(p.id, value.clamp(0.0, 1.0));
            }
        }
    }

    pub fn display(&self, index: usize, value: f64) -> String {
        let (Some(c), Some(p)) = (&self.core.controller, self.params.get(index)) else { return String::new() };
        unsafe {
            let mut s: String128 = [0; 128];
            if c.getParamStringByValue(p.id, value, &mut s) == kResultOk {
                let txt = wstring(&s);
                if p.units.is_empty() {
                    txt
                } else {
                    format!("{txt} {}", p.units)
                }
            } else {
                format!("{:.0}%", value * 100.0)
            }
        }
    }

    pub fn save_state(&self) -> State {
        unsafe {
            let cs = ComWrapper::new(MemoryStream::default());
            let cp: ComPtr<IBStream> = cs.to_com_ptr().unwrap();
            self.core.component.getState(cp.as_ptr());
            let mut st = State { component: cs.data.borrow().clone(), controller: Vec::new() };
            if let Some(c) = &self.core.controller {
                if self.core.separate_controller {
                    let es = ComWrapper::new(MemoryStream::default());
                    let ep: ComPtr<IBStream> = es.to_com_ptr().unwrap();
                    c.getState(ep.as_ptr());
                    st.controller = es.data.borrow().clone();
                }
            }
            st
        }
    }

    pub fn load_state(&self, st: &State) {
        unsafe {
            if !st.component.is_empty() {
                let cs = ComWrapper::new(MemoryStream::from_bytes(&st.component));
                let cp: ComPtr<IBStream> = cs.to_com_ptr().unwrap();
                self.core.component.setState(cp.as_ptr());
                if let Some(c) = &self.core.controller {
                    let cs2 = ComWrapper::new(MemoryStream::from_bytes(&st.component));
                    let cp2: ComPtr<IBStream> = cs2.to_com_ptr().unwrap();
                    c.setComponentState(cp2.as_ptr());
                }
            }
            if let (Some(c), false) = (&self.core.controller, st.controller.is_empty()) {
                let es = ComWrapper::new(MemoryStream::from_bytes(&st.controller));
                let ep: ComPtr<IBStream> = es.to_com_ptr().unwrap();
                c.setState(ep.as_ptr());
            }
        }
    }
}

fn bus_list(component: &ComPtr<IComponent>, dir: BusDirection) -> Vec<Bus> {
    unsafe {
        let n = component.getBusCount(MediaTypes_::kAudio as MediaType, dir);
        (0..n)
            .map(|i| {
                let mut info: BusInfo = std::mem::zeroed();
                component.getBusInfo(MediaTypes_::kAudio as MediaType, dir, i, &mut info);
                Bus { channels: info.channelCount.max(0) as usize }
            })
            .collect()
    }
}

/// Instantiate an effect. Returns the UI handle and the audio processor.
pub fn instantiate(info: &PluginInfo, sample_rate: f64, max_block: usize) -> Result<(Vst3Handle, Vst3Effect), Error> {
    let module = Module::load(&info.bundle)?;
    unsafe {
        let host = ComWrapper::new(HostApplication);
        let host_unknown = host.to_com_ptr::<FUnknown>().unwrap();
        let component: ComPtr<IComponent> =
            module.create(&info.cid).ok_or_else(|| Error(format!("could not create {}", info.name)))?;
        if component.initialize(host_unknown.as_ptr()) != kResultOk {
            return err("component initialize failed");
        }
        let processor: ComPtr<IAudioProcessor> =
            component.cast().ok_or_else(|| Error("component is not an audio processor".into()))?;

        // Edit controller: single-component plugins implement it directly.
        let (controller, separate) = match component.cast::<IEditController>() {
            Some(c) => (Some(c), false),
            None => {
                let mut cid: TUID = [0; 16];
                if component.getControllerClassId(&mut cid) == kResultOk {
                    let cid = *(&cid as *const TUID as *const [u8; 16]);
                    match module.create::<IEditController>(&cid) {
                        Some(c) if c.initialize(host_unknown.as_ptr()) == kResultOk => (Some(c), true),
                        _ => (None, false),
                    }
                } else {
                    (None, false)
                }
            }
        };

        let mut connections = None;
        if let (true, Some(c)) = (separate, &controller) {
            if let (Some(a), Some(b)) = (component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>()) {
                a.connect(b.as_ptr());
                b.connect(a.as_ptr());
                connections = Some((a, b));
            }
        }

        let handler = ComWrapper::new(ComponentHandler::default());
        if let Some(c) = &controller {
            let h = handler.to_com_ptr::<IComponentHandler>().unwrap();
            c.setComponentHandler(h.as_ptr());
            if separate {
                let s = ComWrapper::new(MemoryStream::default());
                let sp: ComPtr<IBStream> = s.to_com_ptr().unwrap();
                if component.getState(sp.as_ptr()) == kResultOk {
                    let s2 = ComWrapper::new(MemoryStream::from_bytes(&s.data.borrow()));
                    let sp2: ComPtr<IBStream> = s2.to_com_ptr().unwrap();
                    c.setComponentState(sp2.as_ptr());
                }
            }
        }

        // Buses: ask for stereo on the main buses, keep the rest as they are.
        let in_buses = bus_list(&component, BusDirections_::kInput as BusDirection);
        let out_buses = bus_list(&component, BusDirections_::kOutput as BusDirection);
        if out_buses.is_empty() {
            return err("plugin has no audio outputs");
        }
        let mut in_arr: Vec<SpeakerArrangement> = in_buses
            .iter()
            .enumerate()
            .map(|(i, b)| if i == 0 { SpeakerArr::kStereo } else { (1u64 << b.channels.min(63)) - 1 })
            .collect();
        let mut out_arr: Vec<SpeakerArrangement> = out_buses
            .iter()
            .enumerate()
            .map(|(i, b)| if i == 0 { SpeakerArr::kStereo } else { (1u64 << b.channels.min(63)) - 1 })
            .collect();
        processor.setBusArrangements(in_arr.as_mut_ptr(), in_arr.len() as i32, out_arr.as_mut_ptr(), out_arr.len() as i32);
        // Re-read the (possibly negotiated) layout.
        let in_buses = bus_list(&component, BusDirections_::kInput as BusDirection);
        let out_buses = bus_list(&component, BusDirections_::kOutput as BusDirection);

        let mut setup = ProcessSetup {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: max_block as i32,
            sampleRate: sample_rate,
        };
        if processor.setupProcessing(&mut setup) != kResultOk {
            return err("setupProcessing failed");
        }
        for (i, _) in in_buses.iter().enumerate() {
            component.activateBus(MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection, i as i32, (i == 0) as u8);
        }
        for (i, _) in out_buses.iter().enumerate() {
            component.activateBus(MediaTypes_::kAudio as MediaType, BusDirections_::kOutput as BusDirection, i as i32, (i == 0) as u8);
        }
        if component.setActive(1) != kResultOk {
            return err("setActive failed");
        }

        // Parameters (hidden / read-only / bypass ones are not exposed).
        let mut params = Vec::new();
        if let Some(c) = &controller {
            for i in 0..c.getParameterCount() {
                let mut pi: ParameterInfo = std::mem::zeroed();
                if c.getParameterInfo(i, &mut pi) != kResultOk {
                    continue;
                }
                let flags = pi.flags;
                use ParameterInfo_::ParameterFlags_ as F;
                if flags & (F::kIsHidden | F::kIsReadOnly | F::kIsBypass) != 0 {
                    continue;
                }
                params.push(ParamInfo {
                    id: pi.id,
                    title: wstring(&pi.title),
                    units: wstring(&pi.units),
                    steps: pi.stepCount,
                    default: pi.defaultNormalizedValue,
                });
            }
        }
        let latency = processor.getLatencySamples() as usize;

        let core = Arc::new(Core {
            _module: module,
            component,
            processor,
            controller,
            separate_controller: separate,
            connections,
            handler,
            _host: host,
        });
        let params = Arc::new(params);
        let handle = Vst3Handle { core: core.clone(), info: info.clone(), params: params.clone() };
        let total_in: usize = in_buses.iter().map(|b| b.channels).sum();
        let total_out: usize = out_buses.iter().map(|b| b.channels).sum();
        let mut fx = Vst3Effect {
            core,
            param_ids: params.iter().map(|p| p.id).collect(),
            pending: Vec::with_capacity(512),
            in_changes: ComWrapper::new(ParamChanges::new(128)),
            out_changes: ComWrapper::new(ParamChanges::new(128)),
            in_bufs: (0..total_in).map(|_| vec![0.0; max_block]).collect(),
            out_bufs: (0..total_out).map(|_| vec![0.0; max_block]).collect(),
            in_ptrs: vec![std::ptr::null_mut(); total_in.max(1)],
            out_ptrs: vec![std::ptr::null_mut(); total_out.max(1)],
            in_abb: Vec::with_capacity(in_buses.len()),
            out_abb: Vec::with_capacity(out_buses.len()),
            in_buses,
            out_buses,
            max_block,
            started: false,
            failed: false,
            frames: 0,
            latency,
        };
        fx.wire_buffers();
        Ok((handle, fx))
    }
}

impl Vst3Effect {
    fn wire_buffers(&mut self) {
        for (i, b) in self.in_bufs.iter_mut().enumerate() {
            self.in_ptrs[i] = b.as_mut_ptr();
        }
        for (i, b) in self.out_bufs.iter_mut().enumerate() {
            self.out_ptrs[i] = b.as_mut_ptr();
        }
        self.in_abb.clear();
        let mut ch = 0;
        for b in &self.in_buses {
            self.in_abb.push(AudioBusBuffers {
                numChannels: b.channels as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: unsafe { self.in_ptrs.as_mut_ptr().add(ch) } },
            });
            ch += b.channels;
        }
        self.out_abb.clear();
        let mut ch = 0;
        for b in &self.out_buses {
            self.out_abb.push(AudioBusBuffers {
                numChannels: b.channels as i32,
                silenceFlags: 0,
                __field0: AudioBusBuffers__type0 { channelBuffers32: unsafe { self.out_ptrs.as_mut_ptr().add(ch) } },
            });
            ch += b.channels;
        }
    }

    fn run(&mut self, buf: &mut [Frame], ctx: &FxCtx, offset_beats: f64) {
        let n = buf.len();
        let in_main = self.in_buses.first().map(|b| b.channels).unwrap_or(0);
        let out_main = self.out_buses[0].channels;
        // De-interleave into the main input bus; silence any aux inputs.
        if in_main >= 2 {
            for (i, f) in buf.iter().enumerate() {
                self.in_bufs[0][i] = f[0];
                self.in_bufs[1][i] = f[1];
            }
        } else if in_main == 1 {
            for (i, f) in buf.iter().enumerate() {
                self.in_bufs[0][i] = (f[0] + f[1]) * 0.5;
            }
        }
        for b in self.in_bufs.iter_mut().skip(in_main) {
            b[..n].fill(0.0);
        }

        self.in_changes.clear();
        self.out_changes.clear();
        for &(id, v) in &self.pending {
            self.in_changes.push(id, v);
        }
        self.pending.clear();

        use ProcessContext_::StatesAndFlags_ as S;
        let beat = ctx.beat + offset_beats;
        let mut pc: ProcessContext = unsafe { std::mem::zeroed() };
        // (The flag constants are i32 with MSVC, u32 elsewhere.)
        pc.state = (S::kPlaying | S::kTempoValid | S::kProjectTimeMusicValid | S::kBarPositionValid | S::kTimeSigValid) as _;
        pc.sampleRate = ctx.sample_rate as f64;
        pc.projectTimeSamples = self.frames;
        pc.continousTimeSamples = self.frames;
        pc.projectTimeMusic = beat;
        pc.barPositionMusic = (beat / 4.0).floor() * 4.0;
        pc.tempo = ctx.bpm;
        pc.timeSigNumerator = 4;
        pc.timeSigDenominator = 4;

        let in_changes = self.in_changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr();
        let out_changes = self.out_changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr();
        let mut data = ProcessData {
            processMode: ProcessModes_::kRealtime as i32,
            symbolicSampleSize: SymbolicSampleSizes_::kSample32 as i32,
            numSamples: n as i32,
            numInputs: self.in_abb.len() as i32,
            numOutputs: self.out_abb.len() as i32,
            inputs: if self.in_abb.is_empty() { std::ptr::null_mut() } else { self.in_abb.as_mut_ptr() },
            outputs: self.out_abb.as_mut_ptr(),
            inputParameterChanges: in_changes,
            outputParameterChanges: out_changes,
            inputEvents: std::ptr::null_mut(),
            outputEvents: std::ptr::null_mut(),
            processContext: &mut pc,
        };
        let r = unsafe { self.core.processor.process(&mut data) };
        if r != kResultOk && r != kResultTrue {
            self.failed = true;
            return;
        }
        self.frames += n as i64;

        if out_main >= 2 {
            let (l, r) = (&self.out_bufs[0], &self.out_bufs[1]);
            for (i, f) in buf.iter_mut().enumerate() {
                *f = [l[i], r[i]];
            }
        } else {
            let m = &self.out_bufs[0];
            for (i, f) in buf.iter_mut().enumerate() {
                *f = [m[i], m[i]];
            }
        }
    }
}

impl Effect for Vst3Effect {
    fn set_param(&mut self, index: usize, value: f32) {
        if let Some(&id) = self.param_ids.get(index) {
            if let Some(p) = self.pending.iter_mut().find(|(i, _)| *i == id) {
                p.1 = value as f64;
            } else if self.pending.len() < self.pending.capacity() {
                self.pending.push((id, value as f64));
            }
        }
    }

    fn process(&mut self, buf: &mut [Frame], ctx: &FxCtx) {
        if self.failed {
            return;
        }
        if !self.started {
            unsafe {
                self.core.processor.setProcessing(1);
            }
            self.started = true;
        }
        let mut off = 0;
        while off < buf.len() {
            let n = (buf.len() - off).min(self.max_block);
            let beats = off as f64 * ctx.beats_per_frame;
            self.run(&mut buf[off..off + n], ctx, beats);
            if self.failed {
                return;
            }
            off += n;
        }
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn failed(&self) -> bool {
        self.failed
    }

    fn reset(&mut self) {
        self.failed = false;
    }
}

/// Keep `CString` import used on all platforms.
#[allow(dead_code)]
fn _unused(_: CString) {}
