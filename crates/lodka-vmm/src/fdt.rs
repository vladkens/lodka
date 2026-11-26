// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! A flattened device tree writer: the guest kernel learns its memory, CPUs and devices from it.
//! https://devicetree-specification.readthedocs.io/en/stable/flattened-format.html

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const END: u32 = 9;
const HEADER_SIZE: usize = 40;
/// The empty memory reservation block: one zero entry of two u64s.
const RSVMAP_SIZE: usize = 16;

#[derive(Default)]
pub struct Fdt {
  data: Vec<u8>,
  strings: Vec<u8>,
  depth: usize,
}

impl Fdt {
  pub fn new() -> Fdt {
    Fdt::default()
  }

  pub fn begin(&mut self, name: &str) {
    self.u32(BEGIN_NODE);
    self.data.extend_from_slice(name.as_bytes());
    self.data.push(0);
    self.pad();
    self.depth += 1;
  }

  pub fn end(&mut self) {
    assert!(self.depth > 0, "fdt: end without begin");
    self.u32(END_NODE);
    self.depth -= 1;
  }

  pub fn prop(&mut self, name: &str, value: &[u8]) {
    let off = self.string(name);
    self.u32(PROP);
    self.u32(value.len() as u32);
    self.u32(off);
    self.data.extend_from_slice(value);
    self.pad();
  }

  pub fn prop_null(&mut self, name: &str) {
    self.prop(name, &[]);
  }

  pub fn prop_str(&mut self, name: &str, value: &str) {
    self.prop_strs(name, &[value]);
  }

  pub fn prop_strs(&mut self, name: &str, values: &[&str]) {
    let buf: Vec<u8> = values.iter().flat_map(|x| x.bytes().chain([0])).collect();
    self.prop(name, &buf);
  }

  pub fn prop_u32(&mut self, name: &str, value: u32) {
    self.prop_u32s(name, &[value]);
  }

  pub fn prop_u32s(&mut self, name: &str, values: &[u32]) {
    let buf: Vec<u8> = values.iter().flat_map(|x| x.to_be_bytes()).collect();
    self.prop(name, &buf);
  }

  pub fn prop_u64s(&mut self, name: &str, values: &[u64]) {
    let buf: Vec<u8> = values.iter().flat_map(|x| x.to_be_bytes()).collect();
    self.prop(name, &buf);
  }

  /// The blob, with its header and an empty memory reservation block.
  pub fn finish(mut self) -> Vec<u8> {
    assert_eq!(self.depth, 0, "fdt: unclosed node");
    self.u32(END);

    let off_struct = HEADER_SIZE + RSVMAP_SIZE;
    let off_strings = off_struct + self.data.len();
    let total = off_strings + self.strings.len();
    let header = [
      MAGIC,
      total as u32,
      off_struct as u32,
      off_strings as u32,
      HEADER_SIZE as u32, // off_mem_rsvmap
      17,                 // version
      16,                 // last_comp_version
      0,                  // boot_cpuid_phys
      self.strings.len() as u32,
      self.data.len() as u32,
    ];

    let mut out: Vec<u8> = header.iter().flat_map(|x| x.to_be_bytes()).collect();
    out.resize(off_struct, 0);
    out.extend_from_slice(&self.data);
    out.extend_from_slice(&self.strings);
    out
  }

  fn u32(&mut self, x: u32) {
    self.data.extend_from_slice(&x.to_be_bytes());
  }

  fn pad(&mut self) {
    self.data.resize(self.data.len().next_multiple_of(4), 0);
  }

  fn string(&mut self, name: &str) -> u32 {
    let mut at = 0;
    for x in self.strings.split(|&b| b == 0) {
      if x == name.as_bytes() {
        return at as u32;
      }

      at += x.len() + 1;
    }

    let off = self.strings.len() as u32;
    self.strings.extend_from_slice(name.as_bytes());
    self.strings.push(0);
    off
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn be32(buf: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(buf[at..at + 4].try_into().unwrap())
  }

  #[test]
  fn header_and_layout() {
    let mut fdt = Fdt::new();
    fdt.begin("");
    fdt.prop_u32("#address-cells", 2);
    fdt.prop_u32("#size-cells", 2);
    fdt.begin("memory@40000000");
    fdt.prop_str("device_type", "memory");
    fdt.prop_u64s("reg", &[0x4000_0000, 0x1000_0000]);
    fdt.end();
    fdt.end();
    let blob = fdt.finish();

    assert_eq!(be32(&blob, 0), MAGIC);
    assert_eq!(be32(&blob, 4) as usize, blob.len());
    let off_struct = be32(&blob, 8) as usize;
    let off_strings = be32(&blob, 12) as usize;
    assert_eq!(off_struct, HEADER_SIZE + RSVMAP_SIZE);
    assert_eq!(be32(&blob, off_struct), BEGIN_NODE);
    assert_eq!(off_strings + be32(&blob, 32) as usize, blob.len());
    assert_eq!(be32(&blob, off_strings - 4), END);
    assert_eq!((off_strings - off_struct) % 4, 0);
  }

  #[test]
  fn strings_are_shared() {
    let mut fdt = Fdt::new();
    fdt.begin("");
    fdt.prop_u32("reg", 1);
    fdt.prop_u32("reg", 2);
    fdt.prop_null("interrupt-controller");
    fdt.end();
    let blob = fdt.finish();

    let strings = &blob[be32(&blob, 12) as usize..];
    assert_eq!(strings, b"reg\0interrupt-controller\0");
  }
}
