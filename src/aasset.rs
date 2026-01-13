//Explanation: Aasset is NOT thread-safe anyways so we will not try adding thread safety either
#![allow(static_mut_refs)]
use crate::{
    loader::{Buffer, FileLoader},
    LockResultExt,
};
use crate::config::{is_no_fog_enabled, is_particles_disabler_enabled};
use libc::{c_char, c_int, c_void, off64_t, off_t, size_t};
use ndk_sys::{AAsset, AAssetManager};
use once_cell::sync::Lazy;
use std::{
    cell::UnsafeCell,
    collections::HashMap,
    ffi::{CStr, CString, OsStr},
    io::{self, Cursor, Read, Seek, Write},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    //    ptr,
    fs,
    sync::{LazyLock, Mutex, Arc, OnceLock},
};
use serde_json::{Value, Map};

static MC_FILELOADER: LazyLock<Mutex<FileLoader>> = LazyLock::new(|| Mutex::new(FileLoader::new()));
// This makes me feel wrong... but all we will do is compare the pointer
// and the struct will be used in a mutex so this is safe??
#[derive(PartialEq, Eq, Hash)]
struct AAssetPtr(*const ndk_sys::AAsset);
unsafe impl Send for AAssetPtr {}

// The assets we have registered to replace data about
static mut WANTED_ASSETS: LazyLock<UnsafeCell<HashMap<AAssetPtr, Buffer>>> =
    LazyLock::new(|| UnsafeCell::new(HashMap::new()));

static WANTED_ASSETS_MUTEX: Lazy<Mutex<HashMap<AAssetPtr, Cursor<Vec<u8>>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
    
// Xelo constants start
const NO_FOG_MATERIAL: &[u8] = include_bytes!("utils/no_fog/RenderChunk.material.bin");

// Xelo constants end

// Xelo fn start

fn get_no_fog_material_data(filename: &str) -> Option<&'static [u8]> {
    if !is_no_fog_enabled() {
        return None;
    }

    match filename {
        "RenderChunk.material.bin" => Some(NO_FOG_MATERIAL),
        _ => None,
    }
}

// Xelo fn end

pub unsafe extern "C" fn open(
    man: *mut AAssetManager,
    fname: *const c_char,
    mode: c_int,
) -> *mut AAsset {
    // This is where UB can happen, but we are merely a hook.
    let aasset = unsafe { ndk_sys::AAssetManager_open(man, fname, mode) };
    let pointer = match std::ptr::NonNull::new(man) {
        Some(yay) => yay,
        None => {
            log::warn!("AssetManager is null?, preposterous, mc detection failed");
            return aasset;
        }
    };
    let manager = unsafe { ndk::asset::AssetManager::from_ptr(pointer) };
    let c_str = unsafe { CStr::from_ptr(fname) };
    let raw_cstr = c_str.to_bytes();
    let os_str = OsStr::from_bytes(raw_cstr);
    let c_path: &Path = Path::new(os_str);
    let Some(os_filename) = c_path.file_name() else {
        log::warn!("Path had no filename: {c_path:?}");
        return aasset;
    };
    
// Xelo Start
        
    // Material replacements
    let filename_str = os_filename.to_string_lossy();
        if let Some(no_fog_data) = get_no_fog_material_data(&filename_str) {
        log::info!("Intercepting {} with no-fog material (no-fog enabled)", filename_str);
        let buffer = no_fog_data.to_vec();
        let mut wanted_lock = WANTED_ASSETS_MUTEX.lock().unwrap();
        wanted_lock.insert(AAssetPtr(aasset), Cursor::new(buffer));
        return aasset;
    }
    
// Xelo end
    
    let mut sus = MC_FILELOADER.lock().ignore_poison();
    if let Some(yay) = sus.get_file(c_path, manager) {
        unsafe { WANTED_ASSETS.get_mut() }.insert(AAssetPtr(aasset), yay);
    }
    aasset
}
macro_rules! handle_result {
    ($expr:expr) => {
        match $expr {
            Ok(val) => val,
            Err(e) => {
                log::error!("{e}");
                return -1;
            }
        }
    };
}

pub unsafe extern "C" fn seek64(aasset: *mut AAsset, off: off64_t, whence: c_int) -> off64_t {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = WANTED_ASSETS.get_mut().get_mut(&ptr) {
        handle_result!(seek_facade(off, whence, file).try_into())
    } else if let Ok(mut cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get_mut(&ptr) {
            let offset = match whence {
                libc::SEEK_SET => io::SeekFrom::Start(off.max(0) as u64),
                libc::SEEK_CUR => io::SeekFrom::Current(off),
                libc::SEEK_END => io::SeekFrom::End(off),
                _ => return ndk_sys::AAsset_seek64(aasset, off, whence),
            };
            match cursor.seek(offset) {
                Ok(pos) => pos as off64_t,
                Err(_) => ndk_sys::AAsset_seek64(aasset, off, whence),
            }
        } else {
            ndk_sys::AAsset_seek64(aasset, off, whence)
        }
    } else {
        ndk_sys::AAsset_seek64(aasset, off, whence)
    }
}

pub unsafe extern "C" fn seek(aasset: *mut AAsset, off: off_t, whence: c_int) -> off_t {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = WANTED_ASSETS.get_mut().get_mut(&ptr) {
        handle_result!(seek_facade(off.into(), whence, file).try_into())
    } else if let Ok(mut cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get_mut(&ptr) {
            let offset = match whence {
                libc::SEEK_SET => io::SeekFrom::Start(off.max(0) as u64),
                libc::SEEK_CUR => io::SeekFrom::Current(off.into()),
                libc::SEEK_END => io::SeekFrom::End(off.into()),
                _ => return ndk_sys::AAsset_seek(aasset, off, whence),
            };
            match cursor.seek(offset) {
                Ok(pos) => pos as off_t,
                Err(_) => ndk_sys::AAsset_seek(aasset, off, whence),
            }
        } else {
            ndk_sys::AAsset_seek(aasset, off, whence)
        }
    } else {
        ndk_sys::AAsset_seek(aasset, off, whence)
    }
}

pub unsafe extern "C" fn read(aasset: *mut AAsset, buf: *mut c_void, count: size_t) -> c_int {
    let ptr = AAssetPtr(aasset);
    let rs_buffer = core::slice::from_raw_parts_mut(buf as *mut u8, count);
    
    if let Some(file) = WANTED_ASSETS.get_mut().get_mut(&ptr) {
        let read_total = handle_result!((*file).read(rs_buffer));
        handle_result!(read_total.try_into())
    } else if let Ok(mut cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get_mut(&ptr) {
            let read_total = handle_result!(cursor.read(rs_buffer));
            handle_result!(read_total.try_into())
        } else {
            ndk_sys::AAsset_read(aasset, buf, count)
        }
    } else {
        ndk_sys::AAsset_read(aasset, buf, count)
    }
}

pub unsafe extern "C" fn len(aasset: *mut AAsset) -> off_t {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = unsafe { WANTED_ASSETS.get_mut() }.get(&ptr) {
        handle_result!(file.get_ref().len().try_into())
    } else if let Ok(cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get(&ptr) {
            handle_result!(cursor.get_ref().len().try_into())
        } else {
            ndk_sys::AAsset_getLength(aasset)
        }
    } else {
        ndk_sys::AAsset_getLength(aasset)
    }
}

pub unsafe extern "C" fn len64(aasset: *mut AAsset) -> off64_t {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = unsafe { WANTED_ASSETS.get_mut() }.get(&ptr) {
        handle_result!(file.get_ref().len().try_into())
    } else if let Ok(cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get(&ptr) {
            handle_result!(cursor.get_ref().len().try_into())
        } else {
            ndk_sys::AAsset_getLength64(aasset)
        }
    } else {
        ndk_sys::AAsset_getLength64(aasset)
    }
}

pub unsafe extern "C" fn rem(aasset: *mut AAsset) -> off_t {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = unsafe { WANTED_ASSETS.get_mut() }.get(&ptr) {
        handle_result!((file.get_ref().len() - file.position() as usize).try_into())
    } else if let Ok(cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get(&ptr) {
            handle_result!((cursor.get_ref().len() - cursor.position() as usize).try_into())
        } else {
            ndk_sys::AAsset_getRemainingLength(aasset)
        }
    } else {
        ndk_sys::AAsset_getRemainingLength(aasset)
    }
}

pub unsafe extern "C" fn rem64(aasset: *mut AAsset) -> off64_t {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = unsafe { WANTED_ASSETS.get_mut() }.get(&ptr) {
        handle_result!((file.get_ref().len() - file.position() as usize).try_into())
    } else if let Ok(cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get(&ptr) {
            handle_result!((cursor.get_ref().len() - cursor.position() as usize).try_into())
        } else {
            ndk_sys::AAsset_getRemainingLength64(aasset)
        }
    } else {
        ndk_sys::AAsset_getRemainingLength64(aasset)
    }
}

pub unsafe extern "C" fn close(aasset: *mut AAsset) {
    let ptr = AAssetPtr(aasset);
    
    if let Some(buffer) = unsafe { WANTED_ASSETS.get_mut() }.remove(&ptr) {
        MC_FILELOADER.lock().ignore_poison().last_buffer = Some(buffer);
    }
    WANTED_ASSETS_MUTEX.lock().unwrap().remove(&ptr);
    
    ndk_sys::AAsset_close(aasset);
}

pub unsafe extern "C" fn get_buffer(aasset: *mut AAsset) -> *const c_void {
    let ptr = AAssetPtr(aasset);
    
    if let Some(file) = unsafe { WANTED_ASSETS.get_mut() }.get(&ptr) {
        file.get_ref().as_ptr().cast()
    } else if let Ok(cursor_map) = WANTED_ASSETS_MUTEX.lock() {
        if let Some(cursor) = cursor_map.get(&ptr) {
            cursor.get_ref().as_ptr().cast()
        } else {
            ndk_sys::AAsset_getBuffer(aasset)
        }
    } else {
        ndk_sys::AAsset_getBuffer(aasset)
    }
}

pub unsafe extern "C" fn fd_dummy(
    aasset: *mut AAsset,
    out_start: *mut off_t,
    out_len: *mut off_t,
) -> c_int {
    let ptr = AAssetPtr(aasset);
    
    if unsafe { WANTED_ASSETS.get_mut() }.contains_key(&ptr) {
        log::error!("WE GOT BUSTED NOOO");
        -1
    } else if WANTED_ASSETS_MUTEX.lock().is_ok_and(|map| map.contains_key(&ptr)) {
        log::error!("WE GOT BUSTED NOOO");
        -1
    } else {
        ndk_sys::AAsset_openFileDescriptor(aasset, out_start, out_len)
    }
}

pub unsafe extern "C" fn fd_dummy64(
    aasset: *mut AAsset,
    out_start: *mut off64_t,
    out_len: *mut off64_t,
) -> c_int {
    let ptr = AAssetPtr(aasset);
    
    if unsafe { WANTED_ASSETS.get_mut() }.contains_key(&ptr) {
        log::error!("WE GOT BUSTED NOOO");
        -1
    } else if WANTED_ASSETS_MUTEX.lock().is_ok_and(|map| map.contains_key(&ptr)) {
        log::error!("WE GOT BUSTED NOOO");
        -1
    } else {
        ndk_sys::AAsset_openFileDescriptor64(aasset, out_start, out_len)
    }
}

pub unsafe extern "C" fn is_alloc(aasset: *mut AAsset) -> c_int {
    let ptr = AAssetPtr(aasset);
    
    if unsafe { WANTED_ASSETS.get_mut() }.contains_key(&ptr) {
        false as c_int
    } else if WANTED_ASSETS_MUTEX.lock().is_ok_and(|map| map.contains_key(&ptr)) {
        false as c_int
    } else {
        ndk_sys::AAsset_isAllocated(aasset)
    }
}

fn seek_facade(offset: i64, whence: c_int, file: &mut Buffer) -> i64 {
    let offset = match whence {
        libc::SEEK_SET => {
            //Let's check this so we don't mess up
            let u64_off = handle_result!(u64::try_from(offset));
            io::SeekFrom::Start(u64_off)
        }
        libc::SEEK_CUR => io::SeekFrom::Current(offset),
        libc::SEEK_END => io::SeekFrom::End(offset),
        _ => {
            log::error!("Invalid seek whence");
            return -1;
        }
    };
    match file.seek(offset) {
        Ok(new_offset) => handle_result!(new_offset.try_into()),
        Err(err) => {
            log::error!("seek Error: {err}");
            return -1;
        }
    }
}