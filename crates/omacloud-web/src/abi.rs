//! What the page calls, with no generated glue: it copies bytes into memory
//! from `alloc`, calls, and reads the answer: a length (4 bytes, little
//! endian), a status byte (0 done, 1 failed) and the content or the error's
//! words. The page frees both with `dealloc`.

use std::cell::RefCell;

use crate::{Reader, Result};

thread_local! {
    static READER: RefCell<Option<Reader>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc(len: usize) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len.max(1));
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// # Safety
///
/// `ptr` and `len` are an `alloc` of that length, or an answer and its whole
/// length (content plus 5).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn dealloc(ptr: *mut u8, len: usize) {
    drop(unsafe { Vec::from_raw_parts(ptr, 0, len.max(1)) });
}

fn answer(result: Result<Vec<u8>>) -> *mut u8 {
    let (status, content) = match result {
        Ok(content) => (0, content),
        Err(e) => (1, e.into_bytes()),
    };
    let ptr = alloc(content.len() + 5);
    let len = u32::try_from(content.len()).unwrap_or(u32::MAX).to_le_bytes();
    // within the allocation just made
    unsafe {
        ptr.copy_from_nonoverlapping(len.as_ptr(), 4);
        ptr.add(4).write(status);
        ptr.add(5).copy_from_nonoverlapping(content.as_ptr(), content.len());
    }
    ptr
}

/// The page's bytes; it keeps them alive for the call.
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

unsafe fn text<'a>(ptr: *const u8, len: usize) -> Result<&'a str> {
    std::str::from_utf8(unsafe { bytes(ptr, len) }).map_err(|_| "not text".to_string())
}

fn with<T>(f: impl FnOnce(&mut Reader) -> Result<T>) -> Result<T> {
    READER.with_borrow_mut(|r| f(r.as_mut().ok_or("the repository isn't open")?))
}

/// # Safety
///
/// Each pointer and length is memory the page filled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn open(password: *const u8, password_len: usize, key: *const u8, key_len: usize) -> *mut u8 {
    answer((|| {
        let reader = Reader::open(unsafe { text(password, password_len) }?, unsafe { bytes(key, key_len) })?;
        READER.set(Some(reader));
        Ok(Vec::new())
    })())
}

/// # Safety
///
/// As [`open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn file(stored: *const u8, len: usize) -> *mut u8 {
    answer(with(|r| r.file(unsafe { bytes(stored, len) })))
}

/// # Safety
///
/// As [`open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn add_index(stored: *const u8, len: usize) -> *mut u8 {
    answer(with(|r| {
        Ok(r.add_index(unsafe { bytes(stored, len) })?.to_string().into_bytes())
    }))
}

/// # Safety
///
/// As [`open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn locate(blob: *const u8, len: usize) -> *mut u8 {
    answer(with(|r| {
        let at = r.locate(unsafe { text(blob, len) }?).ok_or("not in the index")?;
        serde_json::to_vec(at).map_err(|e| e.to_string())
    }))
}

/// # Safety
///
/// As [`open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn blob(blob: *const u8, len: usize, stored: *const u8, stored_len: usize) -> *mut u8 {
    answer(with(|r| {
        r.blob(unsafe { text(blob, len) }?, unsafe { bytes(stored, stored_len) })
    }))
}

/// Open what a drive link's computer left for it; needs no open repository.
///
/// # Safety
///
/// As [`open`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn unseal(seed: *const u8, seed_len: usize, sealed: *const u8, sealed_len: usize) -> *mut u8 {
    answer(crate::link::unseal(unsafe { bytes(seed, seed_len) }, unsafe {
        bytes(sealed, sealed_len)
    }))
}
