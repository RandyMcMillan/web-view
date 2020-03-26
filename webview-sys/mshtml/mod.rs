#![cfg(target_os = "windows")]
#![allow(unused_variables)]

mod interface;
mod web_view;
mod window;

use crate::mshtml::window::WM_WEBVIEW_DISPATCH;
use std::ffi::{CStr, OsStr};
use std::ffi::{CString, OsString};
use std::mem;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::ptr;
use winapi::shared::minwindef::BOOL;
use winapi::shared::windef::DPI_AWARENESS_CONTEXT;
use winapi::shared::windef::DPI_AWARENESS_CONTEXT_SYSTEM_AWARE;
use winapi::um::libloaderapi::GetModuleHandleW;
use winapi::um::libloaderapi::GetProcAddress;

use libc::{c_char, c_int, c_void};

use percent_encoding::percent_decode_str;
use winapi::{shared::windef::RECT, um::winuser::*};

use web_view::WebView;
use window::DispatchData;
use window::Window;

pub(crate) type ExternalInvokeCallback = extern "C" fn(webview: *mut CWebView, arg: *const c_char);
type ErasedDispatchFn = extern "C" fn(webview: *mut CWebView, arg: *mut c_void);

extern "system" {
    fn OleUninitialize();
}

#[repr(C)]
pub(crate) struct CWebView {
    window: Window,
    webview: Box<WebView>,
    external_invoke_cb: ExternalInvokeCallback,
    userdata: *mut c_void,
}

const KEY_FEATURE_BROWSER_EMULATION: &str =
    "Software\\Microsoft\\Internet Explorer\\Main\\FeatureControl\\FEATURE_BROWSER_EMULATION";

fn fix_ie_compat_mode() -> bool {
    use winreg::{enums, RegKey};

    let result = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_name().map(|s| s.to_os_string()));

    if result.is_none() {
        eprintln!("could not get executable name");
        return false;
    }

    let exe_name = result.unwrap();

    let hkcu = RegKey::predef(enums::HKEY_CURRENT_USER);
    let result = hkcu.create_subkey(KEY_FEATURE_BROWSER_EMULATION);

    if result.is_err() {
        eprintln!("could not create regkey {:?}", result);
        return false;
    }

    let (key, _) = result.unwrap();

    let result = key.set_value(&exe_name, &11000u32);
    if result.is_err() {
        eprintln!("could not set regkey value {:?}", result);
        return false;
    }

    true
}

const DATA_URL_PREFIX: &str = "data:text/html,";

#[no_mangle]
extern "C" fn webview_new(
    title: *const c_char,
    url: *const c_char,
    width: c_int,
    height: c_int,
    resizable: c_int,
    debug: c_int,
    frameless: c_int,
    external_invoke_cb: ExternalInvokeCallback,
    userdata: *mut c_void,
) -> *mut CWebView {
    if !fix_ie_compat_mode() {
        return ptr::null_mut();
    }

    let mut cwebview = Box::new(CWebView {
        window: Window::new(),
        webview: WebView::new(),
        external_invoke_cb,
        userdata,
    });

    cwebview.webview.initialize(
        cwebview.window.handle(),
        RECT {
            left: 0,
            right: width,
            top: 0,
            bottom: height,
        },
    );

    let url = unsafe { CStr::from_ptr(url) };
    let url = url.to_str().expect("url is not valid utf8");

    println!("url {}", url);
    if url.starts_with(DATA_URL_PREFIX) {
        let content = percent_decode_str(&url[DATA_URL_PREFIX.len()..])
            .decode_utf8()
            .unwrap();
        println!("{}", &content);
        cwebview.webview.navigate("about:blank");
        cwebview.webview.write(&content);
    } else {
        cwebview.webview.navigate(url);
    }

    unsafe {
        ShowWindow(cwebview.window.handle(), SW_SHOWDEFAULT);
    }

    let wv_ptr = Box::into_raw(cwebview);

    unsafe {
        (*wv_ptr).webview.set_callback(Some(Box::new(move |result| {
            println!("result {}", result);
            let c_result = CString::new(result).unwrap();
            external_invoke_cb(wv_ptr, c_result.as_ptr());
        })));
    }

    wv_ptr
}

#[no_mangle]
unsafe extern "C" fn webview_loop(_webview: *mut CWebView, blocking: c_int) -> c_int {
    let mut msg: MSG = Default::default();
    if blocking > 0 {
        if GetMessageW(&mut msg, 0 as _, 0 as _, 0 as _) < 0 {
            return 0;
        }
    } else {
        if PeekMessageW(&mut msg, 0 as _, 0 as _, 0 as _, PM_REMOVE) < 0 {
            return 0;
        }
    }

    if msg.message == WM_QUIT {
        return 1;
    }
    TranslateMessage(&msg);
    DispatchMessageW(&msg);

    0
}

#[no_mangle]
unsafe extern "C" fn webview_eval(webview: *mut CWebView, js: *const c_char) -> c_int {
    let js = CStr::from_ptr(js);
    let js = js.to_str().expect("js is not valid utf8");
    println!("eval {}", js);
    (*webview).webview.eval(js);
    return 0;
}

#[no_mangle]
unsafe extern "C" fn webview_exit(webview: *mut CWebView) {
    println!("exit");
    DestroyWindow((*webview).window.handle());
    OleUninitialize();
}

#[no_mangle]
unsafe extern "C" fn webview_free(webview: *mut CWebView) {
    let _ = Box::from_raw(webview);
}

#[no_mangle]
unsafe extern "C" fn webview_get_user_data(webview: *mut CWebView) -> *mut c_void {
    (*webview).userdata
}

#[no_mangle]
unsafe extern "C" fn webview_dispatch(
    webview: *mut CWebView,
    f: Option<ErasedDispatchFn>,
    arg: *mut c_void,
) {
    let data = Box::new(DispatchData {
        target: webview,
        func: f.unwrap(),
        arg,
    });
    PostMessageW(
        (*webview).window.handle(),
        WM_WEBVIEW_DISPATCH,
        0,
        Box::into_raw(data) as _,
    );
}

fn enable_dpi_awareness() -> bool {
    type FnSetThreadDpiAwarenessContext =
        extern "system" fn(dpi_context: DPI_AWARENESS_CONTEXT) -> DPI_AWARENESS_CONTEXT;

    type FnSetProcessDpiAware = extern "system" fn() -> BOOL;

    let user32 = "user32.dll";
    let user32 = to_wstring(user32);

    unsafe {
        let hmodule = GetModuleHandleW(user32.as_ptr());
        if hmodule.is_null() {
            return false;
        }

        let set_thread_dpi_awareness = CString::new("SetThreadDpiAwarenessContext").unwrap();
        let set_thread_dpi_awareness = GetProcAddress(hmodule, set_thread_dpi_awareness.as_ptr());

        if !set_thread_dpi_awareness.is_null() {
            let set_thread_dpi_awareness: FnSetThreadDpiAwarenessContext =
                mem::transmute(set_thread_dpi_awareness);
            if !set_thread_dpi_awareness(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE).is_null() {
                return true;
            }
        }

        let set_process_dpi_aware = CString::new("SetProcessDPIAware").unwrap();
        let set_process_dpi_aware = GetProcAddress(hmodule, set_process_dpi_aware.as_ptr());

        if set_process_dpi_aware.is_null() {
            return false;
        }

        let set_process_dpi_aware: FnSetProcessDpiAware = mem::transmute(set_process_dpi_aware);
        set_process_dpi_aware() != 0
    }
}

fn to_wstring(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(Some(0).into_iter())
        .collect()
}

unsafe fn from_wstring(wide: *const u16) -> OsString {
    assert!(!wide.is_null());
    for i in 0.. {
        if *wide.offset(i) == 0 {
            return OsStringExt::from_wide(std::slice::from_raw_parts(wide, i as usize));
        }
    }
    unreachable!()
}
