//! A bandwidth cap around any backend: bucket, service or path.
//!
//! Each transfer reserves its slot in time before it starts, so the rate
//! holds on average across every thread, for small transfers as for packs
//! of hundreds of MiB (OpenDAL's throttle refuses any single write larger
//! than its burst, so it had to allow huge bursts). Up to a second's worth
//! goes out at once after a quiet spell.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use bytes::Bytes;
use rustic_core::{BytesList, FileType, Id, ReadBackend, RusticResult, WriteBackend};

/// Parse a rate such as `5MiB`, `500KiB`, `2MB` or `100000` (bytes per
/// second).
///
/// # Errors
///
/// If it isn't a positive number with one of those units.
pub fn parse_rate(text: &str) -> Result<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let num: f64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("`{text}` isn't a rate like 5MiB"))?;
    let unit = unit.trim().trim_end_matches("/s");
    let scale: f64 = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "kib" => 1024.0,
        "m" | "mb" => 1e6,
        "mib" => 1024.0 * 1024.0,
        "g" | "gb" => 1e9,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        _ => bail!("`{text}`: unknown unit `{unit}`"),
    };
    let rate = (num * scale) as u64;
    anyhow::ensure!(rate > 0, "`{text}` isn't a positive rate");
    Ok(rate)
}

/// Bytes that went through a throttle since the process started; for
/// measuring.
pub static THROTTLED_READ_WRITE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The shared schedule: when the link is next free.
struct Bucket {
    rate: u64,
    next: Mutex<Instant>,
}

impl Bucket {
    /// Wait for the slot of a transfer of `bytes`.
    fn take(&self, bytes: usize) {
        THROTTLED_READ_WRITE.fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
        let now = Instant::now();
        let start = {
            let mut next = self.next.lock().unwrap();
            // a quiet spell earns at most a second of credit
            let start = (*next).max(now.checked_sub(Duration::from_secs(1)).unwrap_or(now));
            *next = start + Duration::from_secs_f64(bytes as f64 / self.rate as f64);
            start
        };
        if start > now {
            std::thread::sleep(start - now);
        }
    }
}

pub struct Throttled {
    inner: Arc<dyn WriteBackend>,
    bucket: Arc<Bucket>,
}

impl Throttled {
    pub fn new(inner: Arc<dyn WriteBackend>, rate: u64) -> Self {
        Self {
            inner,
            bucket: Arc::new(Bucket {
                rate,
                next: Mutex::new(Instant::now()),
            }),
        }
    }
}

impl ReadBackend for Throttled {
    fn location(&self) -> String {
        self.inner.location()
    }

    fn list_with_size(&self, tpe: FileType) -> RusticResult<Vec<(Id, u32)>> {
        self.inner.list_with_size(tpe)
    }

    fn read_full(&self, tpe: FileType, id: &Id) -> RusticResult<Bytes> {
        let data = self.inner.read_full(tpe, id)?;
        // the size is known only afterwards; the next transfer waits for it
        self.bucket.take(data.len());
        Ok(data)
    }

    fn read_partial(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        offset: u32,
        length: u32,
    ) -> RusticResult<Bytes> {
        self.bucket.take(length as usize);
        self.inner.read_partial(tpe, id, cacheable, offset, length)
    }

    fn warmup_path(&self, tpe: FileType, id: &Id) -> String {
        self.inner.warmup_path(tpe, id)
    }

    fn needs_warm_up(&self) -> bool {
        self.inner.needs_warm_up()
    }

    fn warm_up(&self, tpe: FileType, id: &Id) -> RusticResult<()> {
        self.inner.warm_up(tpe, id)
    }
}

impl WriteBackend for Throttled {
    fn create(&self) -> RusticResult<()> {
        self.inner.create()
    }

    fn write_bytes(
        &self,
        tpe: FileType,
        id: &Id,
        cacheable: bool,
        buf: BytesList,
    ) -> RusticResult<()> {
        self.bucket.take(buf.size());
        self.inner.write_bytes(tpe, id, cacheable, buf)
    }

    fn remove(&self, tpe: FileType, id: &Id, cacheable: bool) -> RusticResult<()> {
        self.inner.remove(tpe, id, cacheable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_parse() -> Result<()> {
        assert_eq!(parse_rate("5MiB")?, 5 * 1024 * 1024);
        assert_eq!(parse_rate("2MB")?, 2_000_000);
        assert_eq!(parse_rate("500 KiB/s")?, 500 * 1024);
        assert_eq!(parse_rate("1.5M")?, 1_500_000);
        assert_eq!(parse_rate("100000")?, 100_000);
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("0MiB").is_err());
        assert!(parse_rate("5 parsecs").is_err());
        Ok(())
    }

    #[test]
    fn small_transfers_add_up_to_the_rate() {
        // 100 KB/s: after a second of credit, 30 transfers of 10 KB take
        // about two seconds more
        let b = Bucket {
            rate: 100_000,
            next: Mutex::new(Instant::now()),
        };
        let t = Instant::now();
        for _ in 0..30 {
            b.take(10_000);
        }
        let took = t.elapsed();
        assert!(took >= Duration::from_millis(1900), "took {took:?}");
        assert!(took < Duration::from_millis(3200), "took {took:?}");
    }
}
