//! Windows default terminal support.
//!
//! When Alacritty is selected as the default terminal application, the console host starts this
//! executable with `-Embedding` and activates the COM class registered in the MSIX manifest. The
//! host then calls `ITerminalHandoff::EstablishPtyHandoff` with the handles of a pseudoconsole that
//! is already attached to the client process, and Alacritty takes over the terminal side of that
//! pseudoconsole.
//!
//! The class factory is implemented with the `windows` crate, while the handoff object implements
//! the interfaces by hand. This is required because `ITerminalHandoff`, `ITerminalHandoff2` and
//! `ITerminalHandoff3` all derive directly from `IUnknown` and have different signatures for the
//! same method, which is not something the generated bindings can express.

use std::ffi::c_void;
use std::fs::File;
use std::io;
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use alacritty_terminal::tty::{Handoff, HandoffHandles};
use log::{error, info};
use windows::Win32::Foundation::{
    BOOL, CLASS_E_NOAGGREGATION, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_POINTER,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::core::{
    Error, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, Result as WindowsResult, implement,
};
use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, RPC_E_CHANGED_MODE,
};
use windows_sys::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoInitializeEx, CoRegisterClassObject,
    CoRevokeClassObject, CoUninitialize, REGCLS_MULTIPLEUSE,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_QUERY_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SYNCHRONIZE,
    PROCESS_VM_READ,
};
use winit::event_loop::EventLoopProxy;

use crate::event::{Event, EventType};

/// Class ID registered by the MSIX package for the terminal handoff server.
const CLASS_ID: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0xE8A0_E476,
    data2: 0x80E2,
    data3: 0x4C1B,
    data4: [0x87, 0x1B, 0x65, 0x00, 0xF9, 0x51, 0xB9, 0xD1],
};

const IID_UNKNOWN: GUID = GUID::from_u128(0x0000_0000_0000_0000_c000_0000_0000_0046);
/// ITerminalHandoff (v1 IID requested by specification).
pub const IID_TERMINAL_HANDOFF: GUID = GUID::from_u128(0x4181_47f9_5369_4569_87d7_03b5_ae5c_5a87);
/// ITerminalHandoff (v1 IID defined in OpenConsole / Windows Terminal).
pub const IID_TERMINAL_HANDOFF_MS: GUID = GUID::from_u128(0x59d5_5cce_fc8a_48b4_ace8_0a92_86c6_557f);
pub const IID_TERMINAL_HANDOFF2: GUID = GUID::from_u128(0xaa6b_364f_4a50_4176_9002_0ae7_55e7_b5ef);
/// ITerminalHandoff3 (v3 IID).
pub const IID_TERMINAL_HANDOFF3: GUID = GUID::from_u128(0x6f23_da90_15c5_4203_9db0_64e7_3f1b_1b00);
pub const IID_DEFAULT_TERMINAL_MARKER: GUID =
    GUID::from_u128(0x746e_6bc0_ab05_4e38_ab14_71e8_6763_141f);

#[windows::core::interface("418147F9-5369-4569-87D7-03B5AE5C5A87")]
pub unsafe trait ITerminalHandoff: IUnknown {
    unsafe fn establish_pty_handoff(
        &self,
        in_pipe: HANDLE,
        out_pipe: HANDLE,
        signal: HANDLE,
        reference: HANDLE,
        server: HANDLE,
        client: HANDLE,
    ) -> HRESULT;
}

/// Access rights used when duplicating the console host and client process handles.
const PROCESS_ACCESS: u32 =
    PROCESS_QUERY_INFORMATION | PROCESS_VM_READ | PROCESS_SET_INFORMATION | PROCESS_SYNCHRONIZE;

/// The console host expects the terminal to stay alive until it receives a handoff.
const EMBEDDING_TIMEOUT: Duration = Duration::from_secs(10);

/// `TERMINAL_STARTUP_INFO` from Windows Terminal's `ITerminalHandoff.idl`.
#[repr(C)]
struct TerminalStartupInfo {
    title: *const u16,
    icon_path: *const u16,
    icon_index: i32,
    x: u32,
    y: u32,
    x_size: u32,
    y_size: u32,
    x_count_chars: u32,
    y_count_chars: u32,
    fill_attribute: u32,
    flags: u32,
    show_window: u16,
}

type QueryInterfaceFn =
    unsafe extern "system" fn(*mut HandoffInterface, *const GUID, *mut *mut c_void) -> HRESULT;
type AddRefFn = unsafe extern "system" fn(*mut HandoffInterface) -> u32;
type ReleaseFn = unsafe extern "system" fn(*mut HandoffInterface) -> u32;

#[repr(C)]
struct HandoffVtable {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    establish_pty_handoff: unsafe extern "system" fn(
        *mut HandoffInterface,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
    ) -> HRESULT,
}

#[repr(C)]
struct HandoffVtable2 {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    establish_pty_handoff: unsafe extern "system" fn(
        *mut HandoffInterface,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        TerminalStartupInfo,
    ) -> HRESULT,
}

#[repr(C)]
struct HandoffVtable3 {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
    establish_pty_handoff: unsafe extern "system" fn(
        *mut HandoffInterface,
        *mut HANDLE,
        *mut HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        HANDLE,
        *const TerminalStartupInfo,
    ) -> HRESULT,
}

#[repr(C)]
struct UnknownVtable {
    query_interface: QueryInterfaceFn,
    add_ref: AddRefFn,
    release: ReleaseFn,
}

/// Header shared by all interfaces implemented by [`HandoffObject`].
///
/// Every interface has its own vtable and points back to the same object, so reference counting and
/// identity are shared between all interfaces of an object.
#[repr(C)]
struct HandoffInterface {
    vtable: *const c_void,
    object: *const HandoffObject,
}

impl HandoffInterface {
    fn new(vtable: *const c_void) -> Self {
        Self { vtable, object: ptr::null() }
    }
}

/// COM object implementing `ITerminalHandoff`, `ITerminalHandoff2`, `ITerminalHandoff3` and
/// `IDefaultTerminalMarker`.
#[repr(C)]
struct HandoffObject {
    ref_count: AtomicU32,
    proxy: Mutex<EventLoopProxy<Event>>,
    v1: HandoffInterface,
    v2: HandoffInterface,
    v3: HandoffInterface,
    marker: HandoffInterface,
}

impl HandoffObject {
    fn create(proxy: EventLoopProxy<Event>) -> *mut Self {
        let mut object = Box::new(Self {
            ref_count: AtomicU32::new(1),
            proxy: Mutex::new(proxy),
            v1: HandoffInterface::new(&VTABLE_V1 as *const _ as *const c_void),
            v2: HandoffInterface::new(&VTABLE_V2 as *const _ as *const c_void),
            v3: HandoffInterface::new(&VTABLE_V3 as *const _ as *const c_void),
            marker: HandoffInterface::new(&VTABLE_MARKER as *const _ as *const c_void),
        });

        // All interfaces share the same object, so fix up the back pointers once the allocation
        // has a stable address.
        let object_ptr = &*object as *const Self;
        object.v1.object = object_ptr;
        object.v2.object = object_ptr;
        object.v3.object = object_ptr;
        object.marker.object = object_ptr;

        Box::into_raw(object)
    }
}

static VTABLE_V1: HandoffVtable = HandoffVtable {
    query_interface,
    add_ref,
    release,
    establish_pty_handoff: establish_pty_handoff_v1,
};

static VTABLE_V2: HandoffVtable2 = HandoffVtable2 {
    query_interface,
    add_ref,
    release,
    establish_pty_handoff: establish_pty_handoff_v2,
};

static VTABLE_V3: HandoffVtable3 = HandoffVtable3 {
    query_interface,
    add_ref,
    release,
    establish_pty_handoff: establish_pty_handoff_v3,
};

static VTABLE_MARKER: UnknownVtable = UnknownVtable { query_interface, add_ref, release };

unsafe extern "system" fn query_interface(
    this: *mut HandoffInterface,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if ppv.is_null() {
        return E_POINTER;
    }

    unsafe {
        *ppv = ptr::null_mut();
    }

    if riid.is_null() {
        return E_POINTER;
    }

    let iid = unsafe { &*riid };
    let object = unsafe { &*(*this).object };

    let interface = if *iid == IID_UNKNOWN || *iid == IID_TERMINAL_HANDOFF || *iid == IID_TERMINAL_HANDOFF_MS {
        &object.v1
    } else if *iid == IID_TERMINAL_HANDOFF2 {
        &object.v2
    } else if *iid == IID_TERMINAL_HANDOFF3 {
        &object.v3
    } else if *iid == IID_DEFAULT_TERMINAL_MARKER {
        &object.marker
    } else {
        return E_NOINTERFACE;
    };

    object.ref_count.fetch_add(1, Ordering::Relaxed);

    unsafe {
        *ppv = interface as *const HandoffInterface as *mut c_void;
    }

    HRESULT(0)
}

unsafe extern "system" fn add_ref(this: *mut HandoffInterface) -> u32 {
    let object = unsafe { &*(*this).object };
    object.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: *mut HandoffInterface) -> u32 {
    let object = unsafe { (*this).object };
    let ref_count = unsafe { (*object).ref_count.fetch_sub(1, Ordering::AcqRel) } - 1;

    if ref_count == 0 {
        drop(unsafe { Box::from_raw(object as *mut HandoffObject) });
    }

    ref_count
}

/// Handles that are not used directly by the PTY but still need to be kept alive.
struct HandoffAux {
    signal: File,
    reference: OwnedHandle,
    server: OwnedHandle,
    client: OwnedHandle,
    title: Option<String>,
}

impl HandoffAux {
    fn into_handles(self, input: File, output: File) -> HandoffHandles {
        HandoffHandles {
            input,
            output,
            signal: self.signal,
            reference: Mutex::new(Some(self.reference)),
            server: self.server,
            client: self.client,
            title: self.title,
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn establish_pty_handoff_v1(
    this: *mut HandoffInterface,
    input: HANDLE,
    output: HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
) -> HRESULT {
    let result =
        unsafe { handoff_from_existing(input, output, signal, reference, server, client, None) };
    let object = unsafe { &*(*this).object };
    complete_handoff(object, result)
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn establish_pty_handoff_v2(
    this: *mut HandoffInterface,
    input: HANDLE,
    output: HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup: TerminalStartupInfo,
) -> HRESULT {
    let result = unsafe {
        handoff_from_existing(input, output, signal, reference, server, client, Some(&startup))
    };
    let object = unsafe { &*(*this).object };
    complete_handoff(object, result)
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn establish_pty_handoff_v3(
    this: *mut HandoffInterface,
    input: *mut HANDLE,
    output: *mut HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup: *const TerminalStartupInfo,
) -> HRESULT {
    unsafe {
        if input.is_null() || output.is_null() {
            return E_POINTER;
        }

        *input = ptr::null_mut();
        *output = ptr::null_mut();

        if startup.is_null() {
            return E_INVALIDARG;
        }

        // The terminal owns the pipes it hands to the console host, which allows choosing the
        // buffer size and whether the pipes should be overlapped.
        let (input_read, input_write) = match create_pipe() {
            Ok(pipe) => pipe,
            Err(err) => return hresult_from_io(&err),
        };
        let (output_read, output_write) = match create_pipe() {
            Ok(pipe) => pipe,
            Err(err) => return hresult_from_io(&err),
        };

        info!(
            "Received v3 terminal handoff: input_read={:?} output_write={:?}",
            input_read, output_write
        );

        let aux = match duplicate_handoff_aux(signal, reference, server, client, Some(&*startup)) {
            Ok(aux) => aux,
            Err(err) => {
                error!("Failed to duplicate terminal handoff handles: {err}");
                return hresult_from_io(&err);
            },
        };

        // The console host reads the terminal's input from `input_read` and writes the terminal's
        // output to `output_write`, so those ends are handed over after the PTY has been created.
        let object = &*(*this).object;
        let handles = aux.into_handles(File::from(input_write), File::from(output_read));
        let result = dispatch_handoff(object, handles);
        if result.is_err() {
            return result;
        }

        *input = input_read.into_raw_handle() as HANDLE;
        *output = output_write.into_raw_handle() as HANDLE;

        info!("Handed off pseudoconsole pipes to the console host");

        HRESULT(0)
    }
}

/// Create the handoff for interfaces that receive pipes owned by the console host.
unsafe fn handoff_from_existing(
    input: HANDLE,
    output: HANDLE,
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup: Option<&TerminalStartupInfo>,
) -> io::Result<HandoffHandles> {
    if input.is_null() || output.is_null() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid handoff pipe handle"));
    }

    // Handles passed as `[in] system_handle` parameters are closed by COM once the call returns, so
    // duplicate everything that Alacritty needs to keep using.
    let input = File::from(unsafe { duplicate_handle(input, 0)? });
    let output = File::from(unsafe { duplicate_handle(output, 0)? });
    let aux = unsafe { duplicate_handoff_aux(signal, reference, server, client, startup)? };

    Ok(aux.into_handles(input, output))
}

fn complete_handoff(
    object: &HandoffObject,
    result: io::Result<HandoffHandles>,
) -> HRESULT {
    let handles = match result {
        Ok(handles) => handles,
        Err(err) => {
            error!("Failed to set up terminal handoff: {err}");
            return hresult_from_io(&err);
        },
    };

    dispatch_handoff(object, handles)
}

fn dispatch_handoff(object: &HandoffObject, handles: HandoffHandles) -> HRESULT {
    info!("Dispatching Windows terminal handoff to the event loop");
    let proxy = object.proxy.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let event = Event::new(EventType::TerminalHandoff(Handoff::new(handles)), None);

    match proxy.send_event(event) {
        Ok(()) => HRESULT(0),
        Err(_) => {
            error!("Failed to dispatch Windows terminal handoff event");
            E_FAIL
        },
    }
}

unsafe fn duplicate_handoff_aux(
    signal: HANDLE,
    reference: HANDLE,
    server: HANDLE,
    client: HANDLE,
    startup: Option<&TerminalStartupInfo>,
) -> io::Result<HandoffAux> {
    if signal.is_null() || reference.is_null() || server.is_null() || client.is_null() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid handoff handle"));
    }

    let signal = File::from(unsafe { duplicate_handle(signal, 0)? });
    let reference = unsafe { duplicate_handle(reference, 0)? };
    let server = unsafe { duplicate_handle(server, PROCESS_ACCESS)? };
    let client = unsafe { duplicate_handle(client, PROCESS_ACCESS)? };
    let title = startup.and_then(|startup| unsafe { bstr_to_string(startup.title) });

    Ok(HandoffAux { signal, reference, server, client, title })
}

/// Duplicate a handle into the current process.
///
/// If the requested access rights cannot be granted, the handle is duplicated with the same access
/// as the source handle.
unsafe fn duplicate_handle(handle: HANDLE, access: u32) -> io::Result<OwnedHandle> {
    match unsafe { duplicate_handle_with_access(handle, access) } {
        Ok(handle) => Ok(handle),
        Err(_) if access != 0 => unsafe { duplicate_handle_with_access(handle, 0) },
        Err(err) => Err(err),
    }
}

unsafe fn duplicate_handle_with_access(handle: HANDLE, access: u32) -> io::Result<OwnedHandle> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate: HANDLE = ptr::null_mut();

    let (access, options) = if access == 0 {
        (0, DUPLICATE_SAME_ACCESS)
    } else {
        (access, 0)
    };

    let result = unsafe {
        DuplicateHandle(process, handle, process, &mut duplicate, access, 0, options)
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate as RawHandle) })
}

/// Buffer size for the pipes handed to the console host.
///
/// This matches the buffer size Windows Terminal uses for its handoff pipes.
const PIPE_BUFFER_SIZE: u32 = 128 * 1024;

fn create_pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let mut read: HANDLE = ptr::null_mut();
    let mut write: HANDLE = ptr::null_mut();

    let result = unsafe { CreatePipe(&mut read, &mut write, ptr::null(), PIPE_BUFFER_SIZE) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    let read = unsafe { OwnedHandle::from_raw_handle(read as RawHandle) };
    let write = unsafe { OwnedHandle::from_raw_handle(write as RawHandle) };

    Ok((read, write))
}

/// Convert a `BSTR` to a `String` without taking ownership of the string.
unsafe fn bstr_to_string(bstr: *const u16) -> Option<String> {
    if bstr.is_null() {
        return None;
    }

    // The length prefix of a BSTR is the byte length stored right before the string data.
    let len = unsafe { *bstr.cast::<u32>().sub(1) } as usize / 2;
    let slice = unsafe { std::slice::from_raw_parts(bstr, len) };

    Some(String::from_utf16_lossy(slice))
}

fn hresult_from_io(err: &io::Error) -> HRESULT {
    match err.raw_os_error() {
        Some(code) => HRESULT::from_win32(code as u32),
        None if err.kind() == io::ErrorKind::InvalidInput => E_INVALIDARG,
        None => E_FAIL,
    }
}

/// Class factory for [`HandoffObject`].
#[implement(IClassFactory)]
struct HandoffFactory {
    proxy: Mutex<EventLoopProxy<Event>>,
}

// The `#[implement]` macro generates a wrapper type that holds the user-provided struct. The
// interface implementation has to live on that wrapper, since its vtable is what gets registered.
impl IClassFactory_Impl for HandoffFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Option<&IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> WindowsResult<()> {
        unsafe {
            if punkouter.is_some() {
                return Err(CLASS_E_NOAGGREGATION.into());
            }

            if riid.is_null() || ppvobject.is_null() {
                return Err(E_POINTER.into());
            }

            *ppvobject = ptr::null_mut();

            let proxy = self
                .this
                .proxy
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            let object = HandoffObject::create(proxy);

            let result = query_interface(ptr::addr_of_mut!((*object).v1), riid, ppvobject);

            // Drop the initial reference held by the factory. On success the caller owns the
            // reference added by `QueryInterface`; on failure this destroys the object.
            release(ptr::addr_of_mut!((*object).v1));

            if result.is_err() {
                return Err(result.into());
            }

            Ok(())
        }
    }

    fn LockServer(&self, _flock: BOOL) -> WindowsResult<()> {
        Ok(())
    }
}

/// Registered COM class object for the terminal handoff server.
///
/// The registration is revoked and COM is uninitialized when this is dropped.
#[must_use = "dropping the COM server revokes the class registration"]
pub struct ComServer {
    cookie: u32,
    factory: Option<IClassFactory>,
    initialized: bool,
}

impl ComServer {
    /// Register the terminal handoff class object on the current thread.
    ///
    /// This must be called on the thread that runs the window event loop, so COM can dispatch the
    /// handoff calls to it.
    pub fn register(proxy: EventLoopProxy<Event>) -> WindowsResult<Self> {
        unsafe {
            let result = CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32);
            let initialized = result >= 0;

            // Another part of the application may have initialized COM as multi-threaded already.
            // The class factory and handoff object are thread-safe, so this still works.
            if result < 0 && result != RPC_E_CHANGED_MODE {
                return Err(Error::from_hresult(HRESULT(result)));
            }

            let factory: IClassFactory = HandoffFactory { proxy: Mutex::new(proxy) }.into();

            let mut cookie = 0;
            let result = CoRegisterClassObject(
                &CLASS_ID,
                factory.as_raw(),
                CLSCTX_LOCAL_SERVER,
                REGCLS_MULTIPLEUSE as u32,
                &mut cookie,
            );

            if result < 0 {
                drop(factory);

                if initialized {
                    CoUninitialize();
                }

                return Err(Error::from_hresult(HRESULT(result)));
            }

            info!("Registered Windows terminal handoff COM server");

            Ok(Self { cookie, factory: Some(factory), initialized })
        }
    }
}

impl Drop for ComServer {
    fn drop(&mut self) {
        unsafe {
            let _ = CoRevokeClassObject(self.cookie);
        }

        // Release the class factory before uninitializing COM.
        drop(self.factory.take());

        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Exit the process if COM never hands off a terminal.
pub fn spawn_embedding_timeout(proxy: EventLoopProxy<Event>) {
    let result = std::thread::Builder::new().name("embedding-timeout".into()).spawn(move || {
        std::thread::sleep(EMBEDDING_TIMEOUT);
        let _ = proxy.send_event(Event::new(EventType::EmbeddingTimeout, None));
    });

    if let Err(err) = result {
        error!("Failed to spawn terminal handoff timeout thread: {err}");
    }
}
