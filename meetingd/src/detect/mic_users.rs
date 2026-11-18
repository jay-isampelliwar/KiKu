//! Ask macOS (14+) which processes are capturing audio input right now. This records nothing,
//! so it does not turn on the orange mic dot.

use std::ffi::c_void;
use std::path::Path;

use anyhow::{Result, bail};

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        out_size: *mut u32,
    ) -> i32;

    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        io_size: *mut u32,
        out_data: *mut c_void,
    ) -> i32;
}

unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
}

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

const SYSTEM_OBJECT: u32 = 1;
const SCOPE_GLOBAL: u32 = fourcc(b"glob");
const ELEMENT_MAIN: u32 = 0;
const PROCESS_OBJECT_LIST: u32 = fourcc(b"prs#");
const PROCESS_PID: u32 = fourcc(b"ppid");
const PROCESS_IS_RUNNING_INPUT: u32 = fourcc(b"piri");

/// A process that is capturing audio input.
#[derive(Debug, Clone)]
pub struct MicUser {
    pub pid: i32,
    /// App name, like "Google Chrome", "zoom.us" or "Visual Studio Code".
    pub app: String,
}

/// Every other process capturing audio input. Our own process is left out.
pub fn mic_users() -> Result<Vec<MicUser>> {
    let own_pid = std::process::id() as i32;
    let mut users = Vec::new();

    for object in process_objects()? {
        if read_u32(object, PROCESS_IS_RUNNING_INPUT)? == 0 {
            continue;
        }

        let pid = read_u32(object, PROCESS_PID)? as i32;
        if pid == own_pid {
            continue;
        }

        users.push(MicUser { pid, app: app_name(pid) });
    }

    Ok(users)
}

fn address(selector: u32) -> PropertyAddress {
    PropertyAddress { selector, scope: SCOPE_GLOBAL, element: ELEMENT_MAIN }
}

/// All audio process objects macOS knows about.
fn process_objects() -> Result<Vec<u32>> {
    let addr = address(PROCESS_OBJECT_LIST);
    let mut size = 0u32;

    let status = unsafe {
        AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &addr, 0, std::ptr::null(), &mut size)
    };
    if status != 0 {
        bail!("CoreAudio process list size failed ({status}). Needs macOS 14 or newer.");
    }

    let mut ids = vec![0u32; size as usize / size_of::<u32>()];
    let status = unsafe {
        AudioObjectGetPropertyData(
            SYSTEM_OBJECT,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            ids.as_mut_ptr().cast(),
        )
    };
    if status != 0 {
        bail!("CoreAudio process list failed ({status})");
    }

    ids.truncate(size as usize / size_of::<u32>());
    Ok(ids)
}

fn read_u32(object: u32, selector: u32) -> Result<u32> {
    let addr = address(selector);
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;

    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &addr,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    if status != 0 {
        bail!("CoreAudio property read failed ({status})");
    }

    Ok(value)
}

/// The outermost `.app` name in the process path, so helpers count as their main app.
/// "/Applications/Google Chrome.app/.../Google Chrome Helper.app/..." gives "Google Chrome".
fn app_name(pid: i32) -> String {
    let mut buffer = vec![0u8; 4096];
    let len = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };

    if len <= 0 {
        return format!("pid {pid}");
    }

    let path = String::from_utf8_lossy(&buffer[..len as usize]).into_owned();

    Path::new(&path)
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .find_map(|part| part.strip_suffix(".app"))
        .map(str::to_string)
        .unwrap_or_else(|| {
            Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or(path)
        })
}
