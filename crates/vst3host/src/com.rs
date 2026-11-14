//! Host-side COM objects handed to plugins.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{c_void, CStr, CString};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Mutex;

use vst3::Steinberg::Vst::*;
use vst3::Steinberg::Vst::IAttributeList_::AttrID;
use vst3::Steinberg::*;
use vst3::{Class, ComWrapper, Interface};

pub fn copy_wstring(src: &str, dst: &mut [TChar]) {
    let mut len = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        len += 1;
    }
    if len < dst.len() {
        dst[len] = 0;
    } else if let Some(last) = dst.last_mut() {
        *last = 0;
    }
}

pub fn wstring(src: &[TChar]) -> String {
    let end = src.iter().position(|&c| c == 0).unwrap_or(src.len());
    String::from_utf16_lossy(&src[..end].iter().map(|&c| c as u16).collect::<Vec<_>>())
}

pub fn cstring(src: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = src.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

unsafe fn guid_eq(a: *const TUID, b: &[u8; 16]) -> bool {
    if a.is_null() {
        return false;
    }
    let a = &*(a as *const [u8; 16]);
    a == b
}

// ------------------------------------------------------------ IHostApplication

pub struct HostApplication;

impl Class for HostApplication {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        copy_wstring("Collective", &mut *name);
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if obj.is_null() {
            return kInvalidArgument;
        }
        *obj = std::ptr::null_mut();
        let unknown = if guid_eq(cid, &IMessage::IID) {
            ComWrapper::new(HostMessage::default()).to_com_ptr::<FUnknown>()
        } else if guid_eq(cid, &IAttributeList::IID) {
            ComWrapper::new(HostAttributes::default()).to_com_ptr::<FUnknown>()
        } else {
            None
        };
        match unknown {
            Some(u) => {
                let p = u.as_ptr();
                ((*(*p).vtbl).queryInterface)(p, iid as *const TUID, obj)
            }
            None => kNoInterface,
        }
    }
}

// ------------------------------------------------------------ IComponentHandler

/// Receives notifications from the edit controller (any thread).
#[derive(Default)]
pub struct ComponentHandler {
    pub restart_flags: AtomicI32,
    pub edits: Mutex<Vec<(u32, f64)>>,
}

impl Class for ComponentHandler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        if let Ok(mut e) = self.edits.lock() {
            if let Some(x) = e.iter_mut().find(|(i, _)| *i == id) {
                x.1 = value;
            } else {
                e.push((id, value));
            }
        }
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        self.restart_flags.fetch_or(flags, Ordering::AcqRel);
        kResultOk
    }
}

// ------------------------------------------------------------ IBStream

#[derive(Default)]
pub struct MemoryStream {
    pub data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl MemoryStream {
    pub fn from_bytes(b: &[u8]) -> Self {
        Self { data: RefCell::new(b.to_vec()), pos: Cell::new(0) }
    }
}

impl Class for MemoryStream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for MemoryStream {
    unsafe fn read(&self, buffer: *mut c_void, num_bytes: int32, num_read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (num_bytes.max(0) as usize).min(data.len() - pos);
        if n > 0 && !buffer.is_null() {
            std::ptr::copy_nonoverlapping(data.as_ptr().add(pos), buffer as *mut u8, n);
        }
        self.pos.set(pos + n);
        if !num_read.is_null() {
            *num_read = n as int32;
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, num_bytes: int32, num_written: *mut int32) -> tresult {
        if buffer.is_null() || num_bytes < 0 {
            return kInvalidArgument;
        }
        let n = num_bytes as usize;
        let src = std::slice::from_raw_parts(buffer as *const u8, n);
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        data[pos..pos + n].copy_from_slice(src);
        self.pos.set(pos + n);
        if !num_written.is_null() {
            *num_written = n as int32;
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.data.borrow().len() as i64;
        let base = match mode {
            0 => 0,
            1 => self.pos.get() as i64,
            2 => len,
            _ => return kInvalidArgument,
        };
        let p = (base + pos).clamp(0, len);
        self.pos.set(p as usize);
        if !result.is_null() {
            *result = p;
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        *pos = self.pos.get() as i64;
        kResultOk
    }
}

// ------------------------------------------------------------ parameter changes

/// A single-point parameter queue (one value per block is plenty for
/// knob-rate control). Preallocated; never allocates on the audio thread.
pub struct ParamQueue {
    id: Cell<u32>,
    offset: Cell<i32>,
    value: Cell<f64>,
    points: Cell<i32>,
}

impl Class for ParamQueue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for ParamQueue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.get()
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 || self.points.get() == 0 {
            return kInvalidArgument;
        }
        if !offset.is_null() {
            *offset = self.offset.get();
        }
        if !value.is_null() {
            *value = self.value.get();
        }
        kResultTrue
    }

    unsafe fn addPoint(&self, offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        self.offset.set(offset);
        self.value.set(value);
        self.points.set(1);
        if !index.is_null() {
            *index = 0;
        }
        kResultTrue
    }
}

pub struct ParamChanges {
    queues: Vec<ComWrapper<ParamQueue>>,
    count: Cell<usize>,
}

impl ParamChanges {
    pub fn new(capacity: usize) -> Self {
        Self {
            queues: (0..capacity)
                .map(|_| {
                    ComWrapper::new(ParamQueue {
                        id: Cell::new(0),
                        offset: Cell::new(0),
                        value: Cell::new(0.0),
                        points: Cell::new(0),
                    })
                })
                .collect(),
            count: Cell::new(0),
        }
    }

    pub fn clear(&self) {
        self.count.set(0);
    }

    pub fn len(&self) -> usize {
        self.count.get()
    }

    /// Host-side add (input changes).
    pub fn push(&self, id: u32, value: f64) {
        let n = self.count.get();
        for q in &self.queues[..n] {
            if q.id.get() == id {
                q.value.set(value);
                return;
            }
        }
        if n < self.queues.len() {
            let q = &self.queues[n];
            q.id.set(id);
            q.offset.set(0);
            q.value.set(value);
            q.points.set(1);
            self.count.set(n + 1);
        }
    }

    pub fn get(&self, i: usize) -> Option<(u32, f64)> {
        (i < self.count.get()).then(|| {
            let q = &self.queues[i];
            (q.id.get(), q.value.get())
        })
    }

    fn queue_ptr(&self, i: usize) -> *mut IParamValueQueue {
        self.queues[i].as_com_ref::<IParamValueQueue>().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut())
    }
}

impl Class for ParamChanges {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for ParamChanges {
    unsafe fn getParameterCount(&self) -> int32 {
        self.count.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.count.get() {
            return std::ptr::null_mut();
        }
        self.queue_ptr(index as usize)
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return std::ptr::null_mut();
        }
        let id = *id;
        let n = self.count.get();
        let i = match self.queues[..n].iter().position(|q| q.id.get() == id) {
            Some(i) => i,
            None if n < self.queues.len() => {
                let q = &self.queues[n];
                q.id.set(id);
                q.points.set(0);
                self.count.set(n + 1);
                n
            }
            None => return std::ptr::null_mut(),
        };
        if !index.is_null() {
            *index = i as int32;
        }
        self.queue_ptr(i)
    }
}

// ------------------------------------------------------------ IMessage / IAttributeList

enum Attr {
    Int(i64),
    Float(f64),
    Str(Vec<TChar>),
    Bin(Vec<u8>),
}

#[derive(Default)]
pub struct HostAttributes {
    map: RefCell<HashMap<String, Attr>>,
}

unsafe fn key(id: AttrID) -> Option<String> {
    (!id.is_null()).then(|| CStr::from_ptr(id).to_string_lossy().into_owned())
}

impl Class for HostAttributes {
    type Interfaces = (IAttributeList,);
}

impl IAttributeListTrait for HostAttributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        self.map.borrow_mut().insert(k, Attr::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match key(id).and_then(|k| match self.map.borrow().get(&k) {
            Some(Attr::Int(v)) => Some(*v),
            _ => None,
        }) {
            Some(v) if !value.is_null() => {
                *value = v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        self.map.borrow_mut().insert(k, Attr::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match key(id).and_then(|k| match self.map.borrow().get(&k) {
            Some(Attr::Float(v)) => Some(*v),
            _ => None,
        }) {
            Some(v) if !value.is_null() => {
                *value = v;
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut v = Vec::new();
        let mut i = 0;
        while *string.add(i) != 0 {
            v.push(*string.add(i));
            i += 1;
        }
        v.push(0);
        self.map.borrow_mut().insert(k, Attr::Str(v));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let map = self.map.borrow();
        let Some(Attr::Str(s)) = map.get(&k) else { return kResultFalse };
        let cap = size_in_bytes as usize / std::mem::size_of::<TChar>();
        if cap == 0 || string.is_null() {
            return kInvalidArgument;
        }
        let n = s.len().min(cap);
        std::ptr::copy_nonoverlapping(s.as_ptr(), string, n);
        *string.add(n - 1) = 0;
        kResultOk
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size: uint32) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let v = if data.is_null() { Vec::new() } else { std::slice::from_raw_parts(data as *const u8, size as usize).to_vec() };
        self.map.borrow_mut().insert(k, Attr::Bin(v));
        kResultOk
    }

    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size: *mut uint32) -> tresult {
        let Some(k) = key(id) else { return kInvalidArgument };
        let map = self.map.borrow();
        let Some(Attr::Bin(b)) = map.get(&k) else { return kResultFalse };
        if !data.is_null() {
            *data = b.as_ptr() as *const c_void;
        }
        if !size.is_null() {
            *size = b.len() as u32;
        }
        kResultOk
    }
}

pub struct HostMessage {
    id: RefCell<Option<CString>>,
    attrs: ComWrapper<HostAttributes>,
}

impl Default for HostMessage {
    fn default() -> Self {
        Self { id: RefCell::new(None), attrs: ComWrapper::new(HostAttributes::default()) }
    }
}

impl Class for HostMessage {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for HostMessage {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ref().map(|s| s.as_ptr()).unwrap_or(std::ptr::null())
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        *self.id.borrow_mut() = (!id.is_null()).then(|| CStr::from_ptr(id).to_owned());
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        self.attrs.as_com_ref::<IAttributeList>().map(|r| r.as_ptr()).unwrap_or(std::ptr::null_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::ComPtr;

    #[test]
    fn memory_stream_roundtrip() {
        let s = ComWrapper::new(MemoryStream::default());
        let p: ComPtr<IBStream> = s.to_com_ptr().unwrap();
        unsafe {
            let mut data = *b"hello world";
            let mut n = 0;
            p.write(data.as_mut_ptr() as *mut c_void, 11, &mut n);
            assert_eq!(n, 11);
            let mut r = 0i64;
            p.seek(6, 0, &mut r);
            let mut out = [0u8; 5];
            p.read(out.as_mut_ptr() as *mut c_void, 5, &mut n);
            assert_eq!(&out, b"world");
        }
    }

    #[test]
    fn param_changes_coalesce() {
        let pc = ParamChanges::new(4);
        pc.push(7, 0.1);
        pc.push(7, 0.4);
        pc.push(9, 1.0);
        assert_eq!(pc.len(), 2);
        assert_eq!(pc.get(0), Some((7, 0.4)));
        unsafe {
            let q = ComPtr::<IParamValueQueue>::from_raw(pc.getParameterData(1)).map(|q| {
                let mut off = -1;
                let mut v = 0.0;
                q.getPoint(0, &mut off, &mut v);
                // from_raw takes ownership; balance the refcount
                std::mem::forget(q);
                (off, v)
            });
            assert_eq!(q, Some((0, 1.0)));
        }
    }

    #[test]
    fn host_creates_messages() {
        let host = ComWrapper::new(HostApplication);
        let h: ComPtr<IHostApplication> = host.to_com_ptr().unwrap();
        unsafe {
            let mut cid = *(&IMessage::IID as *const [u8; 16] as *const TUID);
            let mut iid = cid;
            let mut obj = std::ptr::null_mut();
            assert_eq!(h.createInstance(&mut cid, &mut iid, &mut obj), kResultOk);
            let msg = ComPtr::<IMessage>::from_raw(obj as *mut IMessage).unwrap();
            let id = CString::new("hello").unwrap();
            msg.setMessageID(id.as_ptr());
            assert_eq!(CStr::from_ptr(msg.getMessageID()).to_str().unwrap(), "hello");
            let attrs = msg.getAttributes();
            assert!(!attrs.is_null());
        }
    }
}
