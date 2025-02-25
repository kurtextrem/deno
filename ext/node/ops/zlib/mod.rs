// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use deno_core::error::bad_resource_id;
use deno_core::error::type_error;
use deno_core::error::AnyError;
use deno_core::op;
use deno_core::op2;
use deno_core::OpState;
use libz_sys::*;
use std::borrow::Cow;
use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

mod alloc;
pub mod brotli;
mod mode;
mod stream;

use mode::Flush;
use mode::Mode;

use self::stream::StreamWrapper;

#[inline]
fn check(condition: bool, msg: &str) -> Result<(), AnyError> {
  if condition {
    Ok(())
  } else {
    Err(type_error(msg.to_string()))
  }
}

#[inline]
fn zlib(state: &mut OpState, handle: u32) -> Result<Rc<Zlib>, AnyError> {
  state
    .resource_table
    .get::<Zlib>(handle)
    .map_err(|_| bad_resource_id())
}

#[derive(Default)]
struct ZlibInner {
  dictionary: Option<Vec<u8>>,
  err: i32,
  flush: Flush,
  init_done: bool,
  level: i32,
  mem_level: i32,
  mode: Mode,
  strategy: i32,
  window_bits: i32,
  write_in_progress: bool,
  pending_close: bool,
  gzib_id_bytes_read: u32,
  strm: StreamWrapper,
}

const GZIP_HEADER_ID1: u8 = 0x1f;
const GZIP_HEADER_ID2: u8 = 0x8b;

impl ZlibInner {
  #[allow(clippy::too_many_arguments)]
  fn start_write(
    &mut self,
    input: &[u8],
    in_off: u32,
    in_len: u32,
    out: &mut [u8],
    out_off: u32,
    out_len: u32,
    flush: Flush,
  ) -> Result<(), AnyError> {
    check(self.init_done, "write before init")?;
    check(!self.write_in_progress, "write already in progress")?;
    check(!self.pending_close, "close already in progress")?;

    self.write_in_progress = true;

    let next_in = input
      .get(in_off as usize..in_off as usize + in_len as usize)
      .ok_or_else(|| type_error("invalid input range"))?
      .as_ptr() as *mut _;
    let next_out = out
      .get_mut(out_off as usize..out_off as usize + out_len as usize)
      .ok_or_else(|| type_error("invalid output range"))?
      .as_mut_ptr();

    self.strm.avail_in = in_len;
    self.strm.next_in = next_in;
    self.strm.avail_out = out_len;
    self.strm.next_out = next_out;

    self.flush = flush;
    Ok(())
  }

  fn do_write(&mut self, flush: Flush) -> Result<(), AnyError> {
    self.flush = flush;
    match self.mode {
      Mode::Deflate | Mode::Gzip | Mode::DeflateRaw => {
        self.err = self.strm.deflate(flush);
      }
      // Auto-detect mode.
      Mode::Unzip if self.strm.avail_in > 0 => 'blck: {
        let mut next_expected_header_byte = Some(0);
        // SAFETY: `self.strm.next_in` is valid pointer to the input buffer.
        // `self.strm.avail_in` is the length of the input buffer that is only set by
        // `start_write`.
        let strm = unsafe {
          std::slice::from_raw_parts(
            self.strm.next_in,
            self.strm.avail_in as usize,
          )
        };

        if self.gzib_id_bytes_read == 0 {
          if strm[0] == GZIP_HEADER_ID1 {
            self.gzib_id_bytes_read = 1;
            next_expected_header_byte = Some(1);

            // Not enough.
            if self.strm.avail_in == 1 {
              break 'blck;
            }
          } else {
            self.mode = Mode::Inflate;
            next_expected_header_byte = None;
          }
        }

        if self.gzib_id_bytes_read == 1 {
          let byte = match next_expected_header_byte {
            Some(i) => strm[i],
            None => break 'blck,
          };
          if byte == GZIP_HEADER_ID2 {
            self.gzib_id_bytes_read = 2;
            self.mode = Mode::Gunzip;
          } else {
            self.mode = Mode::Inflate;
          }
        } else if next_expected_header_byte.is_some() {
          return Err(type_error(
            "invalid number of gzip magic number bytes read",
          ));
        }
      }
      _ => {}
    }

    match self.mode {
      Mode::Inflate
        | Mode::Gunzip
        | Mode::InflateRaw
        // We're still reading the header.
        | Mode::Unzip => {
        self.err = self.strm.inflate(self.flush);
        // TODO(@littledivy): Use if let chain when it is stable.
        // https://github.com/rust-lang/rust/issues/53667
        // 
        // Data was encoded with dictionary
        if let (Z_NEED_DICT, Some(dictionary)) = (self.err, &self.dictionary) {
          self.err = self.strm.inflate_set_dictionary(dictionary);

          if self.err == Z_OK {
            self.err = self.strm.inflate(flush);
          } else if self.err == Z_DATA_ERROR {
            self.err = Z_NEED_DICT;
          }
        }

        while self.strm.avail_in > 0
          && self.mode == Mode::Gunzip
          && self.err == Z_STREAM_END
          // SAFETY: `strm` is a valid pointer to zlib strm.
          // `strm.next_in` is initialized to the input buffer.
          && unsafe { *self.strm.next_in } != 0x00
        {
          self.err = self.strm.reset(self.mode);
          self.err = self.strm.inflate(flush);
        }
      }
      _ => {}
    }

    let done = self.strm.avail_out != 0 && self.flush == Flush::Finish;
    // We're are not done yet, but output buffer is full
    if self.err == Z_BUF_ERROR && !done {
      // Set to Z_OK to avoid reporting the error in JS.
      self.err = Z_OK;
    }

    self.write_in_progress = false;
    Ok(())
  }

  fn init_stream(&mut self) -> Result<(), AnyError> {
    match self.mode {
      Mode::Gzip | Mode::Gunzip => self.window_bits += 16,
      Mode::Unzip => self.window_bits += 32,
      Mode::DeflateRaw | Mode::InflateRaw => self.window_bits *= -1,
      _ => {}
    }

    self.err = match self.mode {
      Mode::Deflate | Mode::Gzip | Mode::DeflateRaw => self.strm.deflate_init(
        self.level,
        self.window_bits,
        self.mem_level,
        self.strategy,
      ),
      Mode::Inflate | Mode::Gunzip | Mode::InflateRaw | Mode::Unzip => {
        self.strm.inflate_init(self.window_bits)
      }
      Mode::None => return Err(type_error("Unknown mode")),
    };

    self.write_in_progress = false;
    self.init_done = true;

    Ok(())
  }

  fn close(&mut self) -> Result<bool, AnyError> {
    if self.write_in_progress {
      self.pending_close = true;
      return Ok(false);
    }

    self.pending_close = false;
    check(self.init_done, "close before init")?;

    self.strm.end(self.mode);
    self.mode = Mode::None;
    Ok(true)
  }

  fn reset_stream(&mut self) -> Result<(), AnyError> {
    self.err = self.strm.reset(self.mode);

    Ok(())
  }
}

struct Zlib {
  inner: RefCell<ZlibInner>,
}

impl deno_core::Resource for Zlib {
  fn name(&self) -> Cow<str> {
    "zlib".into()
  }
}

#[op2(fast)]
#[smi]
pub fn op_zlib_new(
  state: &mut OpState,
  #[smi] mode: i32,
) -> Result<u32, AnyError> {
  let mode = Mode::try_from(mode)?;

  let inner = ZlibInner {
    mode,
    ..Default::default()
  };

  Ok(state.resource_table.add(Zlib {
    inner: RefCell::new(inner),
  }))
}

#[op2(fast)]
pub fn op_zlib_close(
  state: &mut OpState,
  #[smi] handle: u32,
) -> Result<(), AnyError> {
  let resource = zlib(state, handle)?;
  let mut zlib = resource.inner.borrow_mut();

  // If there is a pending write, defer the close until the write is done.
  zlib.close()?;

  Ok(())
}

#[op]
pub fn op_zlib_write_async(
  state: Rc<RefCell<OpState>>,
  handle: u32,
  flush: i32,
  input: &[u8],
  in_off: u32,
  in_len: u32,
  out: &mut [u8],
  out_off: u32,
  out_len: u32,
) -> Result<
  impl Future<Output = Result<(i32, u32, u32), AnyError>> + 'static,
  AnyError,
> {
  let mut state_mut = state.borrow_mut();
  let resource = zlib(&mut state_mut, handle)?;

  let mut strm = resource.inner.borrow_mut();
  let flush = Flush::try_from(flush)?;
  strm.start_write(input, in_off, in_len, out, out_off, out_len, flush)?;

  let state = state.clone();
  Ok(async move {
    let mut state_mut = state.borrow_mut();
    let resource = zlib(&mut state_mut, handle)?;
    let mut zlib = resource.inner.borrow_mut();

    zlib.do_write(flush)?;
    Ok((zlib.err, zlib.strm.avail_out, zlib.strm.avail_in))
  })
}

// TODO(bartlomieju): op2 can't seem to handle clippy ignore here
#[op]
pub fn op_zlib_write(
  state: &mut OpState,
  handle: u32,
  flush: i32,
  input: &[u8],
  in_off: u32,
  in_len: u32,
  out: &mut [u8],
  out_off: u32,
  out_len: u32,
  result: &mut [u32],
) -> Result<i32, AnyError> {
  let resource = zlib(state, handle)?;

  let mut zlib = resource.inner.borrow_mut();
  let flush = Flush::try_from(flush)?;
  zlib.start_write(input, in_off, in_len, out, out_off, out_len, flush)?;
  zlib.do_write(flush)?;

  result[0] = zlib.strm.avail_out;
  result[1] = zlib.strm.avail_in;

  Ok(zlib.err)
}

// TODO(bartlomieju): op2 can't seem to handle clippy ignore here
#[op]
pub fn op_zlib_init(
  state: &mut OpState,
  handle: u32,
  level: i32,
  window_bits: i32,
  mem_level: i32,
  strategy: i32,
  dictionary: &[u8],
) -> Result<i32, AnyError> {
  let resource = zlib(state, handle)?;
  let mut zlib = resource.inner.borrow_mut();

  check((8..=15).contains(&window_bits), "invalid windowBits")?;
  check((-1..=9).contains(&level), "invalid level")?;

  check((1..=9).contains(&mem_level), "invalid memLevel")?;

  check(
    strategy == Z_DEFAULT_STRATEGY
      || strategy == Z_FILTERED
      || strategy == Z_HUFFMAN_ONLY
      || strategy == Z_RLE
      || strategy == Z_FIXED,
    "invalid strategy",
  )?;

  zlib.level = level;
  zlib.window_bits = window_bits;
  zlib.mem_level = mem_level;
  zlib.strategy = strategy;

  zlib.flush = Flush::None;
  zlib.err = Z_OK;

  zlib.init_stream()?;

  zlib.dictionary = if !dictionary.is_empty() {
    Some(dictionary.to_vec())
  } else {
    None
  };

  Ok(zlib.err)
}

#[op2(fast)]
#[smi]
pub fn op_zlib_reset(
  state: &mut OpState,
  #[smi] handle: u32,
) -> Result<i32, AnyError> {
  let resource = zlib(state, handle)?;

  let mut zlib = resource.inner.borrow_mut();
  zlib.reset_stream()?;

  Ok(zlib.err)
}

#[op2(fast)]
pub fn op_zlib_close_if_pending(
  state: &mut OpState,
  #[smi] handle: u32,
) -> Result<(), AnyError> {
  let resource = zlib(state, handle)?;
  let pending_close = {
    let mut zlib = resource.inner.borrow_mut();
    zlib.write_in_progress = false;
    zlib.pending_close
  };
  if pending_close {
    drop(resource);
    if let Ok(res) = state.resource_table.take_any(handle) {
      res.close();
    }
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn zlib_start_write() {
    // buffer, length, should pass
    type WriteVector = (&'static [u8], u32, u32, bool);
    const WRITE_VECTORS: [WriteVector; 8] = [
      (b"Hello", 5, 0, true),
      (b"H", 1, 0, true),
      (b"", 0, 0, true),
      // Overrun the buffer
      (b"H", 5, 0, false),
      (b"ello", 5, 0, false),
      (b"Hello", 5, 1, false),
      (b"H", 1, 1, false),
      (b"", 0, 1, false),
    ];

    for (input, len, offset, expected) in WRITE_VECTORS.iter() {
      let mut stream = ZlibInner {
        mode: Mode::Inflate,
        ..Default::default()
      };

      stream.init_stream().unwrap();
      assert_eq!(stream.err, Z_OK);
      assert_eq!(
        stream
          .start_write(input, *offset, *len, &mut [], 0, 0, Flush::None)
          .is_ok(),
        *expected
      );
      assert_eq!(stream.err, Z_OK);
      stream.close().unwrap();
    }
  }
}
