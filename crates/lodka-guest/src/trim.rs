// Copyright (c) vladkens | MIT License | https://github.com/vladkens/lodka
//! The guest trims its own page cache once idle, and the balloon's free page reporting gives
//! what it frees back to the Mac: a step of file pages every tick through the root cgroup's
//! `memory.reclaim`, down to a floor, then one compaction, which gathers what is free into whole
//! 2 MiB pieces the Mac takes back at once. When what a trim took comes back as refaults, while
//! it runs or in the five minutes after, trims keep off for an hour. Never `drop_caches`: it
//! takes hot pages too, and the inodes of virtio-fs, whose names and attributes from the Mac stay
//! cached for an hour.

use std::time::Duration;
use std::{fs, thread};

use crate::log;

const TICK: Duration = Duration::from_secs(10);
/// Ticks the guest stays idle before a trim starts: two minutes.
const IDLE_TICKS: u32 = 12;
/// Ticks a trim whose pages came back keeps off: an hour.
const HOLD_TICKS: u32 = 360;
/// Ticks after its last step that a trim's pages coming back still count: five minutes.
const WATCH_TICKS: u32 = 30;
/// The page cache a trim leaves.
const FLOOR: u64 = 256 << 20;
/// Idle: busy for less than this share of all vCPUs' time, with less disk I/O and memory stall
/// than this in a tick.
const IDLE_CPU: f64 = 0.01;
const IDLE_IO: u64 = 4 << 20;
const IDLE_STALL: Duration = Duration::from_millis(10);
const PAGE: u64 = 4096;

pub fn start() {
  thread::spawn(|| {
    // The lowest priority, for this thread only. Not SCHED_IDLE: a step could then wait long
    // for the CPU holding locks and vfs_cache_pressure 0 (and musl's call for it is a stub).
    // SAFETY: a plain call on the calling thread.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 19) };

    let mut trim = Trim::default();
    let mut last = Counters::read();
    loop {
      thread::sleep(TICK);
      let now = Counters::read();
      let act = trim.tick(&now.since(&last));
      run(&mut trim, act);
      last = now;
    }
  });
}

fn run(trim: &mut Trim, act: Act) {
  match act {
    Act::Wait => {}
    // EAGAIN: less to take than asked, the cache is down to what is in use.
    Act::Step(n) => match reclaim(n) {
      Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => trim.end(true),
      Err(e) => {
        log!("memory.reclaim: {e}");
        trim.end(false);
      }
      Ok(()) => {}
    },
    Act::Compact(taken) => {
      log!("trimmed {} MiB of page cache", taken >> 20);
      if let Err(e) = fs::write("/proc/sys/vm/compact_memory", "1") {
        log!("compact_memory: {e}");
      }
    }
  }
}

/// Takes `n` bytes of file pages, keeping the dentries and inodes: memory.reclaim shrinks every
/// slab cache in proportion, and vfs_cache_pressure 0 leaves those two out meanwhile.
fn reclaim(n: u64) -> std::io::Result<()> {
  const PRESSURE: &str = "/proc/sys/vm/vfs_cache_pressure";
  let pressure = fs::read_to_string(PRESSURE)?;
  fs::write(PRESSURE, "0")?;
  let r = fs::write("/sys/fs/cgroup/memory.reclaim", format!("{n} swappiness=0"));
  fs::write(PRESSURE, pressure.trim())?;
  r
}

// MARK: Counters

/// The guest's counters a trim decides by, as totals since boot.
#[derive(Default)]
struct Counters {
  /// Busy and all CPU time, in ticks.
  busy: u64,
  total: u64,
  /// Disk I/O, in KiB: pgpgin and pgpgout.
  io: u64,
  /// Time some task waited on memory, in µs.
  stall: u64,
  /// Pages read again soon after reclaim took them.
  refaults: u64,
  ram: u64,
  file: u64,
}

impl Counters {
  fn read() -> Counters {
    let text = |x| fs::read_to_string(x).unwrap_or_default();
    let mut c = Counters::default();
    c.parse_stat(&text("/proc/stat"));
    c.parse_vmstat(&text("/proc/vmstat"));
    c.parse_meminfo(&text("/proc/meminfo"));
    c.parse_pressure(&text("/proc/pressure/memory"));
    c
  }

  /// cpu  user nice system idle iowait irq softirq steal
  fn parse_stat(&mut self, text: &str) {
    let Some(line) = text.lines().find(|x| x.starts_with("cpu ")) else { return };
    let v: Vec<u64> = line.split_whitespace().skip(1).take(8).flat_map(|x| x.parse()).collect();
    if v.len() == 8 {
      self.total = v.iter().sum();
      self.busy = self.total - v[3] - v[4];
    }
  }

  fn parse_vmstat(&mut self, text: &str) {
    for (k, v) in fields(text) {
      match k {
        "pgpgin" | "pgpgout" => self.io += v,
        "workingset_refault_file" => self.refaults = v,
        _ => {}
      }
    }
  }

  fn parse_meminfo(&mut self, text: &str) {
    for (k, v) in fields(text) {
      match k {
        "MemTotal:" => self.ram = v << 10,
        "Active(file):" | "Inactive(file):" => self.file += v << 10,
        _ => {}
      }
    }
  }

  /// some avg10=0.00 avg60=0.00 avg300=0.00 total=123
  fn parse_pressure(&mut self, text: &str) {
    let Some(line) = text.lines().find(|x| x.starts_with("some ")) else { return };
    let total = line.split_whitespace().find_map(|x| x.strip_prefix("total="));
    self.stall = total.and_then(|x| x.parse().ok()).unwrap_or(0);
  }

  /// What changed since `old`, for a tick.
  fn since(&self, old: &Counters) -> Sample {
    let total = self.total.saturating_sub(old.total).max(1);
    Sample {
      cpu: self.busy.saturating_sub(old.busy) as f64 / total as f64,
      io: self.io.saturating_sub(old.io) << 10,
      stall: Duration::from_micros(self.stall.saturating_sub(old.stall)),
      refaults: self.refaults,
      ram: self.ram,
      file: self.file,
    }
  }
}

/// The first two words of each line, the second a number.
fn fields(text: &str) -> impl Iterator<Item = (&str, u64)> {
  text.lines().filter_map(|x| {
    let mut w = x.split_whitespace();
    Some((w.next()?, w.next()?.parse().ok()?))
  })
}

// MARK: Policy

/// One tick of the guest: what changed in it, and where it stands.
#[derive(Default)]
struct Sample {
  cpu: f64,
  io: u64,
  stall: Duration,
  refaults: u64,
  ram: u64,
  file: u64,
}

#[derive(Debug, PartialEq)]
enum Act {
  Wait,
  /// Take this many bytes of file pages.
  Step(u64),
  /// The trim is over, having taken this many bytes: gather what is free.
  Compact(u64),
}

#[derive(Default)]
struct Trim {
  /// Idle ticks in a row.
  idle: u32,
  /// Ticks left before a trim may start again.
  hold: u32,
  /// A trim is taking pages.
  running: bool,
  /// The last trim: the bytes it took, its last step's, the refaults when it started, and the
  /// ticks since its last step.
  taken: u64,
  last: u64,
  refaults: u64,
  after: u32,
  /// The last trim found less to take than asked: the rest is in use, until the guest is busy.
  out: bool,
}

impl Trim {
  fn tick(&mut self, s: &Sample) -> Act {
    self.hold = self.hold.saturating_sub(1);
    self.after = self.after.saturating_add(1);
    // What a trim took coming back as refaults, while it runs or a while after, busy or not:
    // it took pages in use.
    let back = s.refaults.saturating_sub(self.refaults) * PAGE;
    if self.taken > 0 && self.after <= WATCH_TICKS && back > self.taken / 4 {
      self.hold = HOLD_TICKS;
      let act = if self.running { self.finish() } else { Act::Wait };
      self.taken = 0;
      return act;
    }

    // A busy guest ends a trim without compacting: it is using the memory again.
    if s.cpu >= IDLE_CPU || s.io >= IDLE_IO || s.stall >= IDLE_STALL {
      (self.idle, self.running, self.out) = (0, false, false);
      return Act::Wait;
    }

    self.idle += 1;
    let step = (s.ram / 64).clamp(64 << 20, 256 << 20);
    if self.running {
      if self.out || s.file <= FLOOR {
        return self.finish();
      }

      return self.step(step.min(s.file - FLOOR));
    }

    // A new trim, once the guest has been idle long enough and has a step to give over the
    // floor; the floor alone isn't worth a compaction.
    if self.idle < IDLE_TICKS || self.hold > 0 || self.out || s.file < FLOOR + step {
      return Act::Wait;
    }

    (self.running, self.taken, self.refaults) = (true, 0, s.refaults);
    self.step(step)
  }

  fn step(&mut self, n: u64) -> Act {
    (self.taken, self.last, self.after) = (self.taken + n, n, 0);
    Act::Step(n)
  }

  /// A step failed, and counts for nothing: what memory.reclaim took of it isn't known. With
  /// less to take than asked (`out`) the trim ends; otherwise it is dropped and keeps off for an
  /// hour.
  fn end(&mut self, out: bool) {
    self.taken = self.taken.saturating_sub(self.last);
    match out {
      true => self.out = true,
      false => (self.running, self.hold) = (false, HOLD_TICKS),
    }
  }

  /// The trim is over: a compaction, if it took anything.
  fn finish(&mut self) -> Act {
    self.running = false;
    match self.taken {
      0 => Act::Wait,
      x => Act::Compact(x),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const GIB: u64 = 1 << 30;

  fn idle(file: u64, refaults: u64) -> Sample {
    Sample { ram: 8 * GIB, file, refaults, ..Default::default() }
  }

  #[test]
  fn parses_the_counters() {
    let mut c = Counters::default();
    c.parse_stat("cpu  10 0 5 960 20 2 3 0 0 0\ncpu0 1 0 0 0 0 0 0 0 0 0\n");
    assert_eq!((c.busy, c.total), (20, 1000));
    c.parse_vmstat(
      "pgpgin 100\npgpgout 50\nworkingset_refault_file 7\nworkingset_refault_anon 9\n",
    );
    assert_eq!((c.io, c.refaults), (150, 7));
    c.parse_meminfo("MemTotal:  8000 kB\nActive(file):  300 kB\nInactive(file):  200 kB\n");
    assert_eq!((c.ram, c.file), (8000 << 10, 500 << 10));
    c.parse_pressure("some avg10=0.00 avg60=0.00 avg300=0.00 total=4500\nfull total=1\n");
    assert_eq!(c.stall, 4500);

    let s = c.since(&Counters { busy: 10, total: 0, io: 100, stall: 500, ..Default::default() });
    assert_eq!((s.cpu, s.io, s.stall), (0.01, 50 << 10, Duration::from_millis(4)));
  }

  #[test]
  fn trims_an_idle_guest_down_to_the_floor() {
    let mut t = Trim::default();
    for _ in 1..IDLE_TICKS {
      assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Wait);
    }

    // 8 GiB of RAM: steps of 128 MiB, then what is left over the floor.
    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Step(128 << 20));
    assert_eq!(t.tick(&idle(FLOOR + (100 << 20), 0)), Act::Step(100 << 20));
    assert_eq!(t.tick(&idle(FLOOR, 0)), Act::Compact(228 << 20));
    assert_eq!(t.tick(&idle(FLOOR + (10 << 20), 0)), Act::Wait);

    // Busy for a tick: idle as long again before the next trim.
    t.tick(&Sample { cpu: 0.5, ..idle(2 * GIB, 0) });
    for _ in 1..IDLE_TICKS {
      assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Wait);
    }

    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Step(128 << 20));
    // Busy mid-trim: it ends, with no compaction.
    assert_eq!(t.tick(&Sample { io: 64 << 20, ..idle(2 * GIB, 0) }), Act::Wait);
    assert!(!t.running);
  }

  #[test]
  fn stops_for_an_hour_when_pages_come_back() {
    let mut t = Trim { idle: IDLE_TICKS, ..Default::default() };
    assert_eq!(t.tick(&idle(2 * GIB, 1000)), Act::Step(128 << 20));
    assert_eq!(t.tick(&idle(2 * GIB, 1000 + 1000)), Act::Step(128 << 20));
    // 64 MiB back of 256 taken: a quarter is still fine; more is not.
    assert_eq!(t.tick(&idle(2 * GIB, 1000 + 16384)), Act::Step(128 << 20));
    assert_eq!(t.tick(&idle(2 * GIB, 1000 + 24577)), Act::Compact(384 << 20));
    for _ in 1..HOLD_TICKS {
      assert_eq!(t.tick(&idle(2 * GIB, 30000)), Act::Wait);
    }

    assert_eq!(t.tick(&idle(2 * GIB, 30000)), Act::Step(128 << 20));
  }

  #[test]
  fn counts_pages_coming_back_after_the_trim_too() {
    let mut t = Trim { idle: IDLE_TICKS, ..Default::default() };
    assert_eq!(t.tick(&idle(FLOOR + (128 << 20), 0)), Act::Step(128 << 20));
    assert_eq!(t.tick(&idle(FLOOR, 0)), Act::Compact(128 << 20));
    // The guest busy again, reading back more than a quarter of what went.
    let busy = Sample { io: 64 << 20, ..idle(FLOOR + (64 << 20), 8193) };
    assert_eq!(t.tick(&busy), Act::Wait);
    assert_eq!(t.hold, HOLD_TICKS);

    // Past five minutes, what comes back is the guest's own business.
    let mut t = Trim { idle: IDLE_TICKS, ..Default::default() };
    t.tick(&idle(FLOOR + (128 << 20), 0));
    t.tick(&idle(FLOOR, 0));
    for _ in 0..WATCH_TICKS {
      t.tick(&idle(FLOOR, 0));
    }

    assert_eq!((t.tick(&busy), t.hold), (Act::Wait, 0));
  }

  #[test]
  fn a_step_with_less_to_take_ends_the_trim() {
    // On the first step: nothing taken, nothing to compact.
    let mut t = Trim { idle: IDLE_TICKS, ..Default::default() };
    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Step(128 << 20));
    t.end(true);
    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Wait);

    // Later: the steps before it count; the rest stays until the guest has been busy.
    let mut t = Trim { idle: IDLE_TICKS, ..Default::default() };
    t.tick(&idle(2 * GIB, 0));
    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Step(128 << 20));
    t.end(true);
    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Compact(128 << 20));
    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Wait);
    t.tick(&Sample { cpu: 0.5, ..idle(2 * GIB, 0) });
    for _ in 1..IDLE_TICKS {
      assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Wait);
    }

    assert_eq!(t.tick(&idle(2 * GIB, 0)), Act::Step(128 << 20));
    // Another error: the trim is dropped, and keeps off for an hour.
    t.end(false);
    assert_eq!((t.tick(&idle(2 * GIB, 0)), t.running), (Act::Wait, false));
    assert_eq!(t.hold, HOLD_TICKS - 1);
  }
}
