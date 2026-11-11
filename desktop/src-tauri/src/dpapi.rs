//! 用 Windows DPAPI 加密保存密码。
//!
//! 直接用 crypt32 的 `CryptProtectData` / `CryptUnprotectData`，不引第三方库。
//! 密文只绑定当前 Windows 用户：拷到别的机器/别的账号解不开，属于预期行为。

use std::ffi::c_void;

#[repr(C)]
struct DataBlob {
    cb: u32,
    pb: *mut u8,
}

#[link(name = "crypt32")]
extern "system" {
    fn CryptProtectData(
        p_data_in: *const DataBlob,
        desc: *const u16,
        entropy: *const DataBlob,
        reserved: *mut c_void,
        prompt: *mut c_void,
        flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;

    fn CryptUnprotectData(
        p_data_in: *const DataBlob,
        desc: *mut *mut u16,
        entropy: *const DataBlob,
        reserved: *mut c_void,
        prompt: *mut c_void,
        flags: u32,
        p_data_out: *mut DataBlob,
    ) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(h: *mut c_void) -> *mut c_void;
}

/// 不弹任何 UI（服务/后台进程必须带这个标志）
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

fn to_hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len() / 2);
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    for i in (0..b.len()).step_by(2) {
        out.push((val(b[i])? << 4) | val(b[i + 1])?);
    }
    Some(out)
}

/// 明文 → 十六进制密文。失败返回空串（调用方按"没存密码"处理）。
pub fn encrypt(plain: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let mut buf = plain.as_bytes().to_vec();
    let input = DataBlob {
        cb: buf.len() as u32,
        pb: buf.as_mut_ptr(),
    };
    let mut out = DataBlob {
        cb: 0,
        pb: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 || out.pb.is_null() {
        return String::new();
    }
    let cipher = unsafe { std::slice::from_raw_parts(out.pb, out.cb as usize) }.to_vec();
    unsafe {
        LocalFree(out.pb as *mut c_void);
    }
    to_hex(&cipher)
}

/// 十六进制密文 → 明文。解不开（换机器/换账号/密文损坏）返回空串。
pub fn decrypt(hex: &str) -> String {
    let Some(mut data) = from_hex(hex) else {
        return String::new();
    };
    if data.is_empty() {
        return String::new();
    }
    let input = DataBlob {
        cb: data.len() as u32,
        pb: data.as_mut_ptr(),
    };
    let mut out = DataBlob {
        cb: 0,
        pb: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        )
    };
    if ok == 0 || out.pb.is_null() {
        return String::new();
    }
    let plain = unsafe { std::slice::from_raw_parts(out.pb, out.cb as usize) }.to_vec();
    unsafe {
        LocalFree(out.pb as *mut c_void);
    }
    String::from_utf8_lossy(&plain).to_string()
}
