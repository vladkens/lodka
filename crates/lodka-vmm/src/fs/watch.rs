// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! Changes on the Mac under a folder, from FSEvents: the path of each file changed and how, as
//! they happen, without those this process made (the guest's own).
//! https://developer.apple.com/documentation/coreservices/file_system_events

use std::ffi::{CStr, c_char, c_void};
use std::io;
use std::ptr::{null, null_mut};

/// kFSEventStreamCreateFlag*: deliver each event at once, not this process's, per file.
const NO_DEFER: u32 = 0x02;
const IGNORE_SELF: u32 = 0x08;
const FILE_EVENTS: u32 = 0x10;
const SINCE_NOW: u64 = u64::MAX;
const UTF8: u32 = 0x0800_0100;

type Ref = *const c_void;

#[repr(C)]
struct Context {
  version: isize,
  info: *mut c_void,
  retain: Ref,
  release: Ref,
  copy_description: Ref,
}

/// CFArrayCallBacks, used only by its address.
#[repr(C)]
struct ArrayCallBacks([usize; 5]);

type Callback = extern "C" fn(Ref, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
  fn FSEventStreamCreate(
    alloc: Ref,
    callback: Callback,
    context: *const Context,
    paths: Ref,
    since: u64,
    latency: f64,
    flags: u32,
  ) -> Ref;
  fn FSEventStreamSetDispatchQueue(stream: Ref, queue: *mut c_void);
  fn FSEventStreamStart(stream: Ref) -> u8;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
  static kCFTypeArrayCallBacks: ArrayCallBacks;
  fn CFStringCreateWithBytes(alloc: Ref, b: *const u8, len: isize, enc: u32, ext: u8) -> Ref;
  fn CFArrayCreate(
    alloc: Ref,
    values: *const Ref,
    n: isize,
    callbacks: *const ArrayCallBacks,
  ) -> Ref;
  fn CFRelease(x: Ref);
}

unsafe extern "C" {
  fn dispatch_queue_create(label: *const c_char, attr: *mut c_void) -> *mut c_void;
}

/// Gets each change's path and its kFSEventStreamEventFlag* bits.
pub type Handler = Box<dyn Fn(&[u8], u32) + Send + Sync>;

/// Watches `root` for the rest of the process's life, calling `f` on a queue of its own.
pub fn start(root: &CStr, f: Handler) -> io::Result<()> {
  // The stream is never stopped, so its handler is never freed.
  let info = Box::into_raw(Box::new(f));
  let flags = NO_DEFER | IGNORE_SELF | FILE_EVENTS;
  // SAFETY: CoreFoundation and FSEvents calls on objects made here; the stream copies the
  // context and keeps the paths it needs.
  unsafe {
    let len = root.to_bytes().len() as isize;
    let path = CFStringCreateWithBytes(null(), root.as_ptr().cast(), len, UTF8, 0);
    let paths = CFArrayCreate(null(), &path, 1, &raw const kCFTypeArrayCallBacks);
    let ctx = Context {
      version: 0,
      info: info.cast(),
      retain: null(),
      release: null(),
      copy_description: null(),
    };
    let stream = FSEventStreamCreate(null(), changed, &ctx, paths, SINCE_NOW, 0.0, flags);

    CFRelease(paths);
    CFRelease(path);
    if stream.is_null() {
      drop(Box::from_raw(info));
      return Err(io::Error::other("FSEvents: no stream"));
    }

    let queue = dispatch_queue_create(c"lodka.fsevents".as_ptr(), null_mut());
    FSEventStreamSetDispatchQueue(stream, queue);
    if FSEventStreamStart(stream) == 0 {
      return Err(io::Error::other("FSEvents: the stream did not start"));
    }
  }

  Ok(())
}

extern "C" fn changed(
  _stream: Ref,
  info: *mut c_void,
  n: usize,
  paths: *mut c_void,
  flags: *const u32,
  _ids: *const u64,
) {
  // SAFETY: FSEvents passes `n` C strings and as many flags, and the `info` that `start` leaked.
  unsafe {
    let f = &*(info as *const Handler);
    let paths = std::slice::from_raw_parts(paths as *const *const c_char, n);
    let flags = std::slice::from_raw_parts(flags, n);
    for (p, x) in paths.iter().zip(flags) {
      f(CStr::from_ptr(*p).to_bytes(), *x);
    }
  }
}
