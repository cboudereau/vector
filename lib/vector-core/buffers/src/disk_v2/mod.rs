//! Design specification for our yet-to-be-named SPSC disk buffer implementation:
//!
//! We provide a single writer/single reader interface to an underlying set of files that
//! conceptually represent a ring buffer.  Unlike a typical ring buffer, we block writes when the
//! total size of all unread records reaches the configured limit.  It may be possible to alter the
//! design in the future such that we can provide a "drop oldest" operation mode, but that is
//! out-of-scope for version 1 of this design.
//!
//! Design constraints / invariants:
//! - buffer can be a maximum of 2TB in total size
//! - data files do not exceed 128MB
//! - all headers (ledger, data file) are written in network byte order (big endian) when integers
//!   are involved
//!
//! At a high-level, records that are written end up in one of many underlying data files, while the
//! ledger file -- number of records, writer and reader positions, etc -- is stored in a separate
//! file.  Data files function primarily with a "last process who touched it" ownership model: the
//! writer always creates new files, and the reader deletes files when they have been fully read.
//!
//! Internally, data files consist of a simplified structure that is optimized for the ring buffer
//! use case.  Records are packed together with a minimalistic layout:
//!
//!   record:
//!     checksum: uint32 // CRC32C of ID + payload
//!     length: uint32
//!     id: uint64
//!     payload: uint8[length]
//!
//! The record ID/length/data superblocks repeat infinitely until adding another would exceed the
//! configured data file size limit, in which case a new data file is started. A record cannot
//! exceed the maximum size of a data file.  Attempting to buffer such a record will result in an error.
//!
//! Records are added to a data file sequentially, and contiguously, with no gaps or data alignment
//! adjustments. The record checksum is a CRC32C checksum over the record ID and data specifically.
//! The record length only refers to the number of bytes in the payload.
//!
//! Records are limited to payloads of 8MB or smaller.  Trying to write a record with a payload
//! larger than that will result in an error.  IF a record -- header and payload -- cannot be
//! written to a data file due to insufficient remaining space in the data file, the current data
//! file will be flushed and synchronized to disk, and a new data file will be open.
//!
//! Likewise, the bookkeeping file consists of a simplified structure that is optimized for being
//! shared via a memory-mapped file interface between the writer and reader:
//!
//!   buffer.db:
//!     [total record count - unsigned 64-bit integer]
//!     [total buffer size - unsigned 64-bit integer]
//!     [next record ID - unsigned 64-bit integer]
//!     [writer current data file ID - unsigned 16-bit integer]
//!     [reader current data file ID - unsigned 16-bit integer]
//!     [reader last record ID - unsigned 64-bit integer]
//!
//! As this buffer is meant to emulate a ring buffer, most of the bookkeeping resolves around the
//! writer and reader being able to quickly figure out where they left off.  Record and data file
//! IDs are simply rolled over when they reach the maximum of their data type, and are incremented
//! indiscriminately rather than reused if one is retired within the 0 - N range.
//!
//! TODO: think through whether or not we can use total file size to ensure that we never try to
//! open more than 4096 files (2TB max buffer size / 256MB max data file size) total, so that we can
//! avoid needing an array/bitmap/etc tracking which files are in use.

use std::{
    convert::TryInto,
    io::{self, ErrorKind},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU16, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use crc32fast::Hasher;
use crossbeam_utils::atomic::AtomicCell;
use memmap2::{MmapMut, MmapOptions};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    sync::{Mutex, Notify},
};

const LEDGER_FILE_SIZE: usize = 36;
// We don't want data files to be bigger than 128MB, but we might end up overshooting slightly.
const DATA_FILE_TARGET_MAX_SIZE: u64 = 128 * 1024 * 1024;
// There's no particular reason that _has_ to be 8MB, it's just a simple default we've chosen here.
const DATA_FILE_MAX_RECORD_SIZE: u32 = 8 * 1024 * 1024;
// Record header: record checksum (u32) + record length (u32) + record ID (u64).
const DATA_FILE_RECORD_HEADER_SIZE: u64 = 16;

#[derive(Debug)]
struct LedgerState {
    // Total number of records persisted in this buffer.
    total_records: AtomicU64,
    // Total size of all data files used by this buffer.
    total_buffer_size: AtomicU64,
    // Next record ID to use when writing a record.
    writer_next_record_id: AtomicU64,
    // The current data file ID being written to.
    writer_current_data_file_id: AtomicU16,
    // The current data file ID being read from.
    reader_current_data_file_id: AtomicU16,
    // The last record ID read by the reader.
    reader_last_record_id: AtomicU64,
}

impl Default for LedgerState {
    fn default() -> Self {
        Self {
            // First record written is always 1, so that our defualt of 0 for
            // `reader_last_record_id` ensures we start up in a state of "alright, waiting to read
            // record #1 next".
            writer_next_record_id: 1.into(),
            ..Default::default()
        }
    }
}

impl LedgerState {
    pub fn get_next_writer_record_id(&self) -> u64 {
        self.writer_next_record_id.fetch_add(1, Ordering::Acquire)
    }

    pub fn get_last_reader_record_id(&self) -> u64 {
        self.reader_last_record_id.load(Ordering::Acquire)
    }

    pub fn increment_last_reader_record_id(&self) {
        self.reader_last_record_id.fetch_add(1, Ordering::AcqRel);
    }

    /// Gets the current write file ID.
    pub fn current_writer_file_id(&self) -> u16 {
        self.writer_current_data_file_id.load(Ordering::Acquire)
    }

    /// Increments the current writer file ID.
    ///
    /// Returns the previous writer file ID.
    pub fn increment_writer_file_id(&self) -> u16 {
        self.writer_current_data_file_id
            .fetch_add(1, Ordering::AcqRel)
    }

    /// Increments the current reader file ID.
    ///
    /// Returns the previous reader file ID.
    pub fn increment_reader_file_id(&self) -> u16 {
        self.reader_current_data_file_id
            .fetch_add(1, Ordering::AcqRel)
    }

    pub fn serialize_to(&self, dst: &mut [u8]) {
        // CLARITY TODO: This is very ugly, and fragile due to field offsets.  It'd be nice if we
        // had a macro or something to make this a little more programmatic/repeatable/machine
        // checkable.  Given that we only have three structs which we serialize in this fashion,
        // though, that could be overkill.
        //
        // PERFORMANCE TODO: This is a nice, safe variant of pushing the state into the data file,
        // but I'm not sure if doing a pointer-level `memcpy` action would be meaningfully faster.
        let total_records = self.total_records.load(Ordering::SeqCst).to_be_bytes();
        let total_buffer_size = self.total_buffer_size.load(Ordering::SeqCst).to_be_bytes();
        let next_record_id = self
            .writer_next_record_id
            .load(Ordering::SeqCst)
            .to_be_bytes();
        let writer_current_data_file_id = self
            .writer_current_data_file_id
            .load(Ordering::SeqCst)
            .to_be_bytes();
        let reader_current_data_file_id = self
            .reader_current_data_file_id
            .load(Ordering::SeqCst)
            .to_be_bytes();
        let reader_last_record_id = self
            .reader_last_record_id
            .load(Ordering::SeqCst)
            .to_be_bytes();

        let mut src = Vec::new();
        src.extend_from_slice(&total_records[..]);
        src.extend_from_slice(&total_buffer_size[..]);
        src.extend_from_slice(&next_record_id[..]);
        src.extend_from_slice(&writer_current_data_file_id[..]);
        src.extend_from_slice(&reader_current_data_file_id[..]);
        src.extend_from_slice(&reader_last_record_id[..]);

        debug_assert!(dst.len() == LEDGER_FILE_SIZE);
        debug_assert!(src.len() == LEDGER_FILE_SIZE);
        dst.copy_from_slice(&src[..]);
    }

    pub fn deserialize_from(&mut self, src: &[u8]) {
        // CLARITY TODO: This is very ugly, and fragile due to field offsets.  It'd be nice if we
        // had a macro or something to make this a little more programmatic/repeatable/machine
        // checkable.  Given that we only have three structs which we serialize in this fashion,
        // though, that could be overkill.
        debug_assert!(src.len() == LEDGER_FILE_SIZE);

        self.total_records = src[..8]
            .try_into()
            .map(u64::from_be_bytes)
            .map(AtomicU64::new)
            .expect("should have had 8 bytes");
        self.total_buffer_size = src[8..16]
            .try_into()
            .map(u64::from_be_bytes)
            .map(AtomicU64::new)
            .expect("should have had 8 bytes");
        self.writer_next_record_id = src[16..24]
            .try_into()
            .map(u64::from_be_bytes)
            .map(AtomicU64::new)
            .expect("should have had 8 bytes");
        self.writer_current_data_file_id = src[24..26]
            .try_into()
            .map(u16::from_be_bytes)
            .map(AtomicU16::new)
            .expect("should have had 2 bytes");
        self.reader_current_data_file_id = src[26..28]
            .try_into()
            .map(u16::from_be_bytes)
            .map(AtomicU16::new)
            .expect("should have had 2 bytes");
        self.reader_last_record_id = src[28..36]
            .try_into()
            .map(u64::from_be_bytes)
            .map(AtomicU64::new)
            .expect("should have had 8 bytes");
    }
}

#[derive(Debug)]
struct Ledger {
    // Path to the data directory.
    data_dir: PathBuf,
    // Handle to the memory-mapped ledger file.
    ledger_mmap: Mutex<MmapMut>,
    // Ledger state.
    state: LedgerState,
    // Notifier for reader-related progress.
    reader_notify: Notify,
    // Notifier for writer-related progress.
    writer_notify: Notify,
    // Last flush of all unflushed files: ledger, data file, etc.
    last_flush: AtomicCell<Instant>,
    // How often flushes should occur.
    //
    // Flushes may occur more often as a data file filling up forcefully triggers a flush so that
    // all data is on-disk before moving on to the next data file.
    flush_interval: Duration,
}

impl Ledger {
    pub fn allocate_record_id(&self) -> u64 {
        self.state.get_next_writer_record_id()
    }

    pub fn get_last_reader_record_id(&self) -> u64 {
        self.state.get_last_reader_record_id()
    }

    pub fn increment_last_reader_record_id(&self) {
        self.state.increment_last_reader_record_id()
    }

    pub fn get_writer_current_data_file_path(&self) -> PathBuf {
        self.get_data_file_path(self.state.current_writer_file_id())
    }

    pub fn increment_writer_file_id(&self) -> u16 {
        self.state.increment_writer_file_id()
    }

    pub fn get_reader_current_data_file_path(&self) -> PathBuf {
        self.get_data_file_path(
            self.state
                .reader_current_data_file_id
                .load(Ordering::Acquire),
        )
    }

    pub fn get_data_file_path(&self, file_id: u16) -> PathBuf {
        self.data_dir.join(format!("buffer-data-{}.dat", file_id))
    }

    /// Waits for a signal from the reader that an entire data file has been read and subsequently deleted.
    pub async fn wait_for_reader(&self) {
        self.reader_notify.notified().await
    }

    /// Waits for a signal from the writer that data has been written to a data file, or that a new
    /// data file has been created.
    pub async fn wait_for_writer(&self) {
        self.writer_notify.notified().await
    }

    /// Notifies all tasks waiting on progress by the reader.
    pub fn notify_reader_waiters(&self) {
        self.reader_notify.notify_waiters()
    }

    /// Notifies all tasks waiting on progress by the writer.
    pub fn notify_writer_waiters(&self) {
        self.writer_notify.notify_waiters()
    }

    /// Determines whether or not all files should be flushed/fsync'd to disk.
    ///
    /// In the case of concurrent callers when the flush deadline has been exceeded, only one caller
    /// will get a return value of `true`, and the others will receive `false`.  The caller that
    /// receives `true` is responsible for flushing the necessary files.
    fn should_flush(&self) -> bool {
        let last_flush = self.last_flush.load();
        if last_flush.elapsed() > self.flush_interval {
            if let Ok(_) = self.last_flush.compare_exchange(last_flush, Instant::now()) {
                return true;
            }
        }

        false
    }

    async fn read_from_disk(&mut self) -> io::Result<()> {
        // TODO: this theoretically doesn't need to return a Result right now, let alone an
        // io::Result, but at some point we should likely be adding checksums and doing other
        // checks, so loading our state from disk would become a fallible operation

        // INVARIANT: We always create the ledger file with a size of LEDGER_FILE_SIZE, which never
        // changes over time.  We can be sure that the slice we take to read the ledger state will
        // be exactly LEDGER_FILE_SIZE bytes.
        let ledger_mmap = self.ledger_mmap.lock().await;
        let ledger_region = &ledger_mmap[..];
        debug_assert_eq!(ledger_region.len(), LEDGER_FILE_SIZE);
        self.state.deserialize_from(ledger_region);

        Ok(())
    }

    pub fn track_write(&self, bytes_written: u64) {
        self.state.total_records.fetch_add(1, Ordering::Release);
        self.state
            .total_buffer_size
            .fetch_add(bytes_written, Ordering::Release);
    }

    pub async fn flush(&self) -> io::Result<()> {
        // INVARIANT: We always create the ledger file with a size of LEDGER_FILE_SIZE, which never
        // changes over time.  We can be sure that the slice we take to write the ledger state will
        // be exactly LEDGER_FILE_SIZE bytes.
        let mut ledger_mmap = self.ledger_mmap.lock().await;
        let ledger_region = &mut ledger_mmap[..];
        debug_assert_eq!(ledger_region.len(), LEDGER_FILE_SIZE);
        self.state.serialize_to(ledger_region);

        ledger_mmap.flush()
    }

    pub async fn load_or_create<P>(data_dir: P, flush_interval: Duration) -> io::Result<Ledger>
    where
        P: AsRef<Path>,
    {
        let ledger_path = data_dir.as_ref().join("buffer.db");
        let ledger_handle = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&ledger_path)
            .await?;
        let _ = ledger_handle.set_len(LEDGER_FILE_SIZE as u64).await?;

        let ledger_handle = ledger_handle.into_std().await;
        let ledger_mmap = unsafe { MmapOptions::new().map_mut(&ledger_handle)? };
        let mut ledger = Ledger {
            data_dir: data_dir.as_ref().to_owned(),
            ledger_mmap: Mutex::new(ledger_mmap),
            state: LedgerState::default(),
            reader_notify: Notify::const_new(),
            writer_notify: Notify::const_new(),
            last_flush: AtomicCell::new(Instant::now()),
            flush_interval,
        };

        let _ = ledger.read_from_disk().await?;
        Ok(ledger)
    }
}

struct WriteState {
    ledger: Arc<Ledger>,
    data_file: Option<BufWriter<File>>,
    data_file_size: u64,
    checksummer: Hasher,
}

impl WriteState {
    fn track_write(&mut self, bytes_written: u64) {
        self.data_file_size += bytes_written;
        self.ledger.track_write(bytes_written);
    }

    fn can_write(&mut self) -> bool {
        self.data_file_size < DATA_FILE_TARGET_MAX_SIZE
    }

    fn reset(&mut self) {
        self.data_file = None;
        self.data_file_size = 0;
    }

    pub async fn ensure_ready_for_write(&mut self) -> io::Result<()> {
        // If our data file is already open, and it has room left, then we're good here.  Otherwise,
        // flush everything and reset ourselves so that we can open the next data file for writing.
        if self.data_file.is_some() {
            if self.can_write() {
                return Ok(());
            } else {
                // Increment our writer information and flush our current data file/ledger.
                self.ledger.increment_writer_file_id();
                let _ = self.flush().await?;

                self.reset();
            }
        }

        loop {
            // Normally, readers will keep up with the writers, and so there will only ever be a
            // single data file or two on disk.  If there was an issue with a sink reading from this
            // buffer, though, we could conceivably have a stalled reader while the writer
            // progresses and continues to create new data file.
            //
            // At some point, the file ID will wrap around and the writer will want to open a "new"
            // file for writing that already exists: a previously-written file that has not been
            // read yet.
            //
            // In order to handle this situation, we loop here, trying to create the file.  Readers
            // are responsible deleting a file once they have read it entirely, so our first loop
            // iteration is the happy path, trying to create the new file.  If we can't create it,
            // we explicitly wait for the reader to signal that it has made writer-relevant
            // progress: in other words, that it has fully read and deleted a data file, in case we
            // were waiting for that to happen.
            let data_file_path = self.ledger.get_writer_current_data_file_path();
            let maybe_data_file = OpenOptions::new()
                .append(true)
                .create_new(true)
                .open(&data_file_path)
                .await;

            let file = match maybe_data_file {
                // We were able to create the file, so we're good to proceed.
                Ok(data_file) => Some((data_file, 0)),
                // We got back an error trying to open the file: might be that it already exists,
                // might be something else.
                Err(e) => match e.kind() {
                    // The file already exists, so it might have been a file we left off writing
                    // to, or it might be full.  Figure out which.
                    ErrorKind::AlreadyExists => {
                        // We open the file again, without the atomic "create new" behavior.  If we
                        // can do that successfully, we check its length.  Anything less than our
                        // target max file size indicates that it's either a partially-filled data
                        // file that we can pick back up, _or_ that the reader finished and deleted
                        // the file between our initial open attempt and this one.
                        //
                        // If the file is indeed "full", though, then we hand back `None`, which
                        // will force a wait on reader progress before trying again.
                        let data_file = OpenOptions::new()
                            .append(true)
                            .create(true)
                            .open(&data_file_path)
                            .await?;
                        let metadata = data_file.metadata().await?;
                        let file_len = metadata.len();
                        if file_len >= DATA_FILE_TARGET_MAX_SIZE {
                            None
                        } else {
                            Some((data_file, file_len))
                        }
                    }
                    // Legitimate I/O error with the operation, bubble this up.
                    ek => return Err(e),
                },
            };

            match file {
                // We successfully opened the file and it can be written to.
                Some((data_file, data_file_size)) => {
                    // Make sure the file is flushed to disk, especially if we just created it.
                    let _ = data_file.sync_all().await?;

                    self.data_file = Some(BufWriter::new(data_file));
                    self.data_file_size = data_file_size;
                    return Ok(());
                }
                // The file is still present and waiting for a reader to finish reading it in order
                // to delete it.  Wait until the reader signals progress and try again.
                None => self.ledger.wait_for_reader().await,
            }
        }
    }

    pub async fn write_record(&mut self, record_buf: &[u8]) -> io::Result<()> {
        let _ = self.ensure_ready_for_write().await?;

        let record_id = self.ledger.allocate_record_id().to_be_bytes();
        let record_checksum = self
            .generate_checksum(&record_id[..], record_buf)
            .to_be_bytes();
        let record_length = record_buf.len().to_be_bytes();

        // Write our record header and data.
        let data_file = self.data_file.as_mut().expect("data file should be open");
        let _ = data_file.write_all(&record_checksum[..]).await?;
        let _ = data_file.write_all(&record_length[..]).await?;
        let _ = data_file.write_all(&record_id[..]).await?;
        let _ = data_file.write_all(record_buf).await?;

        // Update the metadata now that we've written the record.
        let bytes_written =
            record_buf.len() + record_checksum.len() + record_length.len() + record_id.len();
        self.track_write(bytes_written as u64);

        Ok(())
    }

    pub async fn flush(&mut self) -> io::Result<()> {
        if let Some(data_file) = self.data_file.as_mut() {
            let _ = data_file.flush().await?;
            let _ = data_file.get_mut().flush().await?;
            let _ = data_file.get_mut().sync_all().await?;
        }

        self.ledger.flush().await
    }

    pub async fn maybe_flush(&mut self) -> io::Result<()> {
        if self.ledger.should_flush() {
            self.flush().await
        } else {
            Ok(())
        }
    }

    fn generate_checksum(&mut self, id: &[u8], payload: &[u8]) -> u32 {
        let mut checksummer = self.checksummer.clone();
        checksummer.reset();

        // Record ID is always in network order.
        checksummer.update(&id[..]);
        checksummer.update(payload);

        checksummer.finalize()
    }
}

impl From<Arc<Ledger>> for WriteState {
    fn from(ledger: Arc<Ledger>) -> Self {
        WriteState {
            ledger,
            data_file: None,
            data_file_size: 0,
            checksummer: Hasher::new(),
        }
    }
}

pub struct WriteTransaction<'a> {
    inner: &'a mut WriteState,
}

impl<'a> WriteTransaction<'a> {
    pub async fn write<R>(&mut self, record: R) -> io::Result<()>
    where
        R: AsRef<[u8]>,
    {
        let record_buf = record.as_ref();

        // Check that the record isn't bigger than the maximum record size.  This isn't a limitation
        // of writing to files, but mostly just common sense to have some reasonable upper bound.
        let buf_len = u32::try_from(record_buf.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "record buf size bigger than 2^32 bytes!",
            )
        })?;
        if buf_len > DATA_FILE_MAX_RECORD_SIZE {
            return Err(io::Error::new(io::ErrorKind::Other, "record too large"));
        }

        self.inner.write_record(record_buf).await
    }

    pub async fn commit(self) -> io::Result<()> {
        self.inner.maybe_flush().await
    }
}

pub struct Writer {
    ledger: Arc<Ledger>,
    state: WriteState,
}

impl Writer {
    fn new(ledger: Arc<Ledger>) -> Self {
        let state = WriteState::from(Arc::clone(&ledger));
        Writer { ledger, state }
    }

    pub fn total_records(&self) -> u64 {
        self.ledger.state.total_records.load(Ordering::Relaxed)
    }

    pub async fn ensure_ready_for_write(&mut self) -> io::Result<()> {
        self.state.ensure_ready_for_write().await
    }

    pub async fn maybe_flush(&mut self) -> io::Result<()> {
        self.state.maybe_flush().await
    }

    pub fn transaction(&mut self) -> WriteTransaction<'_> {
        WriteTransaction {
            inner: &mut self.state,
        }
    }
}

pub struct Reader {
    ledger: Arc<Ledger>,
    data_file: Option<BufReader<File>>,
    last_reader_record_id: u64,
    buf: BytesMut,
    checksummer: Hasher,
}

impl Reader {
    fn new(ledger: Arc<Ledger>) -> Self {
        Reader {
            ledger,
            data_file: None,
            last_reader_record_id: 0,
            buf: BytesMut::with_capacity(8192),
            checksummer: Hasher::new(),
        }
    }

    fn increment_last_reader_record_id(&mut self) {
        self.last_reader_record_id += 1;
        self.ledger.increment_last_reader_record_id();
    }

    async fn try_read_atleast(&mut self, n: usize) -> io::Result<Option<BytesMut>> {
        // If our buffer already has enough bytes to fulfill the request, we remove those from the
        // buffer and hand them, back to the caller.
        if self.buf.len() >= n {
            return Ok(Some(self.buf.split_to(n)));
        }

        // Make sure our buffer is big enough to hold `n`.  We assume that `n` doesn't not change
        // overall until it is fulfilled, so we only reserve enough bytes such that the capacity of
        // the buffer is at least `n`.
        if self.buf.capacity() < n {
            self.buf.reserve(n - self.buf.len());
        }

        // Issue a read to try and fill our buffer.
        let data_file = self
            .data_file
            .as_mut()
            .expect("data file must be initialized");
        let _ = data_file.read_buf(&mut self.buf).await?;

        // Try one more time to see if we can fulfill the request after reading.
        if self.buf.len() >= n {
            Ok(Some(self.buf.split_to(n)))
        } else {
            Ok(None)
        }
    }

    pub async fn ensure_ready_for_read(&mut self) -> io::Result<()> {
        // Try to open the current reader data file.  This might not _yet_ exist, in which case
        // we'll simply wait for the writer to signal to us that progress has been made, which
        // implies a data file existing.
        loop {
            let data_file_path = self.ledger.get_reader_current_data_file_path();
            let data_file = match File::open(&data_file_path).await {
                Ok(data_file) => data_file,
                Err(e) => match e.kind() {
                    ErrorKind::NotFound => {
                        self.ledger.wait_for_writer().await;
                        continue;
                    }
                    // This is a valid I/O error, so bubble that back up.
                    ek => return Err(e),
                },
            };

            self.data_file = Some(BufReader::new(data_file));
            return Ok(());
        }
    }

    pub async fn seek_to_next_record(&mut self) -> io::Result<()> {
        // Under normal operation, the writer next/reader last record IDs are staggered, such that
        // in a fresh buffer, the "next" record ID for the writer to use when writing a record is
        // `1`, and the "last" record ID for the reader to use when reading a record is `0`.  No
        // seeking or adjusting of file cursors is necessary, as the writer/reader should move in
        // lockstep, including when new data files are created.
        //
        // In cases where Vector has restarted, but the reader hasn't yet finished a file, we would
        // open the correct data file for reading, but our file cursor would be at the very
        // beginning, essentially pointed at the wrong record.  We read out records here until we
        // reach a point where we've read up to the record right before `get_next_reader_record_id`.
        //
        // This ensures that a subsequent call to `next` is ready to read the correct record.

        // We rely on `next` to close out the data file if we've actually reached the end, and we
        // also rely on it to reset the data file before trying to read, and we _also_ rely on it to
        // update `self.last_reader_record_id`, so basically... just keep reading records until we
        // get to the one we left off with last time.
        let last_reader_record_id = self.ledger.get_last_reader_record_id();
        while self.last_reader_record_id < last_reader_record_id {
            let _ = self.next().await?;
        }

        Ok(())
    }

    pub async fn next(&mut self) -> io::Result<(u64, Bytes)> {
        // - try and open current reader file described by ledger
        // -- if doesnt exist yet, just wait until signalled to try again
        let _ = self.ensure_ready_for_read().await?;

        struct Header {
            checksum: u32,
            len: u32,
            id: u64,
        }

        enum State {
            NeedHeader,
            NeedPayload(Header),
            NeedChecksumVerify(Header, BytesMut),
            Verified(Header, BytesMut),
        }

        // Everything here is predicated on the idea that the file will return EOF when we try to
        // read more and there's no more data, and if there _is_ data, it will return "immediately",
        // so we should never actually await a read.
        //
        // TODO: In fact, we don't even really need to use the `File`/`BufReader` from `tokio`,
        // because we're explicitly waiting for wake-ups from the writer when there's not enough
        // data, but just using it for consistency at this point.
        let mut state = State::NeedHeader;
        loop {
            let (next_state, wait) = match state {
                State::NeedHeader => match self
                    .try_read_atleast(DATA_FILE_RECORD_HEADER_SIZE as usize)
                    .await?
                {
                    Some(buf) => {
                        let checksum = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                        let len = u32::from_be_bytes(buf[4..8].try_into().unwrap());
                        let id = u64::from_be_bytes(buf[8..16].try_into().unwrap());

                        let header = Header { checksum, len, id };
                        (State::NeedPayload(header), false)
                    }
                    None => (State::NeedHeader, true),
                },
                State::NeedPayload(header) => {
                    match self.try_read_atleast(header.len as usize).await? {
                        Some(buf) => (State::NeedChecksumVerify(header, buf), false),
                        None => (State::NeedPayload(header), true),
                    }
                }
                State::NeedChecksumVerify(header, payload) => {
                    let mut checksummer = self.checksummer.clone();
                    checksummer.reset();

                    let record_id = header.id.to_be_bytes();
                    checksummer.update(&record_id[..]);
                    checksummer.update(&payload);

                    let checksum = checksummer.finalize();

                    // TODO: we need to figure out the right logic for updating the last read
                    // record ID.
                    //
                    // ideally, we'd always be setting `last_reader_record_id` to the ID we got
                    // from the record itself, even though, realistically, we could just increment
                    // by one since record IDs should always be monotonic with no gaps.
                    //
                    // if the record checksum is invalid, our ID might also be invalid, so that
                    // becomes risky, and we just increment by one directly.
                    //
                    // if the checksum is valid, we could do the same thing, but it seems like we
                    // ought to actually check to make sure that `last_reader_record_id` + 1 ==
                    // `header.id`, since this would tell us if there were record ID gaps,
                    // indicating some real weirdness with the writer.
                    //
                    // for now, just increment by one and do a crude check, if the checksum is
                    // valid, to make sure the id we just got now equals `last_reader_record_id`.
                    self.increment_last_reader_record_id();

                    if checksum == header.checksum {
                        // update our last read record id for this record.
                        //
                        // TODO: should we emit a warning when we read a non-sequential record ID?
                        // we always increment the ID in the "checksum didn't match" path so it
                        // should always be valid, and we could actually just increment it instead
                        // of even having
                        //
                        // crude panic for now
                        if self.last_reader_record_id != header.id {
                            panic!(
                                "non-monotonic/sequential record ID: last => {}, currrent => {}",
                                self.last_reader_record_id, header.id
                            );
                        }

                        (State::Verified(header, payload), false)
                    } else {
                        // we should probably emit an error here to track corrupted records, but for
                        // the moment.  additionally, we may want to return an error per corrupted
                        // record.
                        //
                        // for now, though, we'll simply reset our state and try to read the next record.
                        (State::NeedHeader, false)
                    }
                }
                State::Verified(header, payload) => return Ok((header.id, payload.freeze())),
            };

            state = next_state;
            if wait {
                self.ledger.wait_for_writer().await;
            }
        }
    }
}

pub struct Buffer;

impl Buffer {
    pub async fn from_path<P>(data_dir: P) -> io::Result<(Writer, Reader)>
    where
        P: AsRef<Path>,
    {
        let ledger = Ledger::load_or_create(data_dir, Duration::from_secs(1)).await?;
        let ledger = Arc::new(ledger);

        let writer = Writer::new(Arc::clone(&ledger));
        let reader = Reader::new(ledger);

        Ok((writer, reader))
    }
}
