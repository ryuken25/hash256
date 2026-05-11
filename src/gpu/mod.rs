//! OpenCL GPU backend wrapper. The kernel accepts a 192-bit high nonce prefix
//! plus a 64-bit counter so workers can safely partition the full uint256 PoW
//! nonce space across machines and devices.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{keccak256, B256, U256};
use eyre::{eyre, Result};
use ocl::{flags, Buffer, Context, Device, Kernel, Platform, Program, Queue};

use crate::nonce_space::{pow_nonce_from_parts, NoncePrefix};

const KERNEL_SRC: &str = include_str!("kernel.cl");
const DEFAULT_BATCH: usize = 1 << 26;

pub struct GpuMiner {
    _context: Context,
    queue: Queue,
    program: Program,
    device_name: String,
    device_index: usize,
    batch_size: usize,
    auto_tune: bool,
    target_dispatch_ms: u64,
    local_size: usize,
}

impl GpuMiner {
    pub fn new(batch_size: Option<usize>) -> Result<Self> {
        Self::new_for_index(0, batch_size, true, 250, 256)
    }

    pub fn new_for_index(
        index: usize,
        batch_size: Option<usize>,
        auto_tune: bool,
        target_dispatch_ms: u64,
        local_size: usize,
    ) -> Result<Self> {
        let batch_size = batch_size.unwrap_or(DEFAULT_BATCH).max(1 << 16);
        let (platform, device) = pick_device(index)?;
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".into());
        let context = Context::builder()
            .platform(platform)
            .devices(device)
            .build()?;
        let queue = Queue::new(&context, device, None)?;
        let program = Program::builder()
            .src(KERNEL_SRC)
            .devices(device)
            .build(&context)?;
        Ok(Self {
            _context: context,
            queue,
            program,
            device_name,
            device_index: index,
            batch_size,
            auto_tune,
            target_dispatch_ms,
            local_size: local_size.max(1),
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn device_index(&self) -> usize {
        self.device_index
    }

    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    pub fn self_test(&self) -> Result<()> {
        let challenge = B256::from(*b"abcdefghijklmnopqrstuvwxyz012345");
        let prefix = NoncePrefix { high192: [0u8; 24] };
        let cases = [
            0u64,
            1,
            0xdead_beef,
            u32::MAX as u64 + 7,
            0x1122_3344_5566_7788,
        ];
        for c in cases {
            let target = U256::MAX;
            let stop = Arc::new(AtomicBool::new(false));
            let attempts = Arc::new(AtomicU64::new(0));
            let got = self.dispatch_once(challenge, target, prefix, c, 1, &stop, &attempts)?;
            if got != Some(pow_nonce_from_parts(prefix, c)) {
                return Err(eyre!("GPU self-test failed for counter {c}"));
            }
        }
        Ok(())
    }

    /// Continuously search for a nonce starting from `start_counter`. Returns
    /// `Ok(Some(nonce, counter_consumed))` on hit, `Ok(None)` if interrupted by
    /// `stop_flag` or `epoch_token` change.
    pub fn mine_until(
        &mut self,
        challenge: B256,
        target: U256,
        prefix: NoncePrefix,
        start_counter: u64,
        stop_flag: Arc<AtomicBool>,
        attempts_counter: Arc<AtomicU64>,
        epoch_token: Option<(Arc<AtomicU64>, u64)>,
    ) -> Result<MineOutcome> {
        let mut counter = start_counter;
        loop {
            if stop_flag.load(Ordering::Relaxed) {
                return Ok(MineOutcome::Aborted { last_counter: counter });
            }
            if let Some((tok, snap)) = &epoch_token {
                if tok.load(Ordering::Relaxed) != *snap {
                    return Ok(MineOutcome::Aborted { last_counter: counter });
                }
            }
            let dispatch = self.batch_size as u64;
            let t = Instant::now();
            let result = self.dispatch_once(
                challenge,
                target,
                prefix,
                counter,
                dispatch,
                &stop_flag,
                &attempts_counter,
            )?;
            let elapsed_ms = t.elapsed().as_millis() as u64;
            counter = counter.wrapping_add(dispatch);
            if let Some(nonce) = result {
                return Ok(MineOutcome::Found {
                    nonce,
                    last_counter: counter,
                });
            }
            if self.auto_tune && elapsed_ms > 0 {
                self.adjust_batch(elapsed_ms);
            }
        }
    }

    fn adjust_batch(&mut self, elapsed_ms: u64) {
        let target = self.target_dispatch_ms.max(50);
        let ratio = target as f64 / elapsed_ms.max(1) as f64;
        let clamped = ratio.clamp(0.5, 1.5);
        let next = ((self.batch_size as f64) * clamped) as usize;
        let lo = 1 << 16;
        let hi = 1 << 30;
        self.batch_size = next.clamp(lo, hi);
        // Keep multiple of local_size for clean global_work_size.
        let ls = self.local_size.max(1);
        self.batch_size = (self.batch_size / ls).max(1) * ls;
    }

    fn dispatch_once(
        &self,
        challenge: B256,
        target: U256,
        prefix: NoncePrefix,
        start_counter: u64,
        dispatch: u64,
        stop_flag: &Arc<AtomicBool>,
        attempts_counter: &Arc<AtomicU64>,
    ) -> Result<Option<U256>> {
        let cw = split_challenge_le(&challenge);
        let pw = split_prefix_lanes(prefix);
        let dw = split_u256_be(target);
        let found_counter = Buffer::<u64>::builder()
            .queue(self.queue.clone())
            .flags(flags::MEM_READ_WRITE)
            .len(1)
            .copy_host_slice(&[0u64])
            .build()?;
        let found_flag = Buffer::<i32>::builder()
            .queue(self.queue.clone())
            .flags(flags::MEM_READ_WRITE)
            .len(1)
            .copy_host_slice(&[0i32])
            .build()?;
        let global = dispatch.max(1) as usize;
        let kernel = Kernel::builder()
            .program(&self.program)
            .name("mine_keccak_u256")
            .queue(self.queue.clone())
            .global_work_size(global)
            .arg(cw[0])
            .arg(cw[1])
            .arg(cw[2])
            .arg(cw[3])
            .arg(pw[0])
            .arg(pw[1])
            .arg(pw[2])
            .arg(dw[0])
            .arg(dw[1])
            .arg(dw[2])
            .arg(dw[3])
            .arg(start_counter)
            .arg(&found_counter)
            .arg(&found_flag)
            .build()?;
        unsafe {
            kernel.enq()?;
        }
        self.queue.finish()?;
        attempts_counter.fetch_add(dispatch, Ordering::Relaxed);
        if stop_flag.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let mut flag = [0i32];
        found_flag.read(&mut flag[..]).enq()?;
        if flag[0] != 0 {
            let mut got = [0u64];
            found_counter.read(&mut got[..]).enq()?;
            let nonce = pow_nonce_from_parts(prefix, got[0]);
            // CPU re-verify to drop false positives caused by GPU edge-cases.
            let h = cpu_hash(&challenge, nonce);
            if U256::from_be_bytes::<32>(h.0) < target {
                return Ok(Some(nonce));
            }
        }
        Ok(None)
    }

    /// Backwards-compat helper used by `bench`. Runs one continuous mining
    /// session for up to `duration` and returns total attempts.
    pub fn mine(
        &mut self,
        challenge: B256,
        target: U256,
        start_nonce: u64,
        stop_flag: Arc<AtomicBool>,
        attempts_counter: Arc<AtomicU64>,
    ) -> Result<Option<u64>> {
        let prefix = NoncePrefix { high192: [0u8; 24] };
        let outcome = self.mine_until(
            challenge,
            target,
            prefix,
            start_nonce,
            stop_flag,
            attempts_counter,
            None,
        )?;
        Ok(match outcome {
            MineOutcome::Found { nonce, .. } => {
                let b = nonce.to_be_bytes::<32>();
                Some(u64::from_be_bytes(b[24..32].try_into().unwrap()))
            }
            MineOutcome::Aborted { .. } => None,
        })
    }
}

#[derive(Debug, Clone)]
pub enum MineOutcome {
    Found { nonce: U256, last_counter: u64 },
    Aborted { last_counter: u64 },
}

pub fn list_devices() -> Result<Vec<(usize, String)>> {
    let mut all = Vec::new();
    let mut idx = 0usize;
    for platform in Platform::list() {
        let pname = platform.name().unwrap_or_else(|_| "<unknown>".into());
        println!("Platform: {pname}");
        for d in Device::list_all(platform).unwrap_or_default().iter() {
            let dname = d.name().unwrap_or_else(|_| "<unknown>".into());
            println!("  [{idx}] {dname}");
            all.push((idx, dname));
            idx += 1;
        }
    }
    if all.is_empty() {
        println!("(no OpenCL devices found)");
    }
    Ok(all)
}

fn pick_device(index: usize) -> Result<(Platform, Device)> {
    let mut idx = 0usize;
    for platform in Platform::list() {
        let devices = Device::list_all(platform).unwrap_or_default();
        for d in devices {
            if idx == index {
                return Ok((platform, d));
            }
            idx += 1;
        }
    }
    // Fallback: use first device of default platform.
    let platform = Platform::default();
    let devices = Device::list_all(platform)?;
    let d = *devices
        .get(index)
        .or_else(|| devices.first())
        .ok_or_else(|| eyre!("no OpenCL device for index {index}"))?;
    Ok((platform, d))
}

fn split_challenge_le(challenge: &B256) -> [u64; 4] {
    let b = challenge.as_slice();
    [
        u64::from_le_bytes(b[0..8].try_into().unwrap()),
        u64::from_le_bytes(b[8..16].try_into().unwrap()),
        u64::from_le_bytes(b[16..24].try_into().unwrap()),
        u64::from_le_bytes(b[24..32].try_into().unwrap()),
    ]
}

fn split_prefix_lanes(prefix: NoncePrefix) -> [u64; 3] {
    let b = prefix.high192;
    [
        u64::from_le_bytes(b[0..8].try_into().unwrap()),
        u64::from_le_bytes(b[8..16].try_into().unwrap()),
        u64::from_le_bytes(b[16..24].try_into().unwrap()),
    ]
}

fn split_u256_be(d: U256) -> [u64; 4] {
    let b = d.to_be_bytes::<32>();
    [
        u64::from_be_bytes(b[0..8].try_into().unwrap()),
        u64::from_be_bytes(b[8..16].try_into().unwrap()),
        u64::from_be_bytes(b[16..24].try_into().unwrap()),
        u64::from_be_bytes(b[24..32].try_into().unwrap()),
    ]
}

fn cpu_hash(challenge: &B256, nonce: U256) -> B256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(challenge.as_slice());
    buf[32..].copy_from_slice(&nonce.to_be_bytes::<32>());
    keccak256(buf)
}

#[allow(dead_code)]
pub fn brief_pause() {
    std::thread::sleep(Duration::from_micros(1));
}
