//!
//! Generate a tarball with files needed to bootstrap ComputeNode.
//!
//! TODO: this module has nothing to do with PostgreSQL pg_basebackup.
//! It could use a better name.
//!
use crate::ZTimelineId;
use bytes::{BufMut, BytesMut};
use log::*;
use std::io::Write;
use std::sync::Arc;
use std::time::SystemTime;
use tar::{Builder, Header};
use walkdir::WalkDir;

use crate::repository::{DatabaseTag, ObjectTag, Timeline};
use crc32c::*;
use postgres_ffi::relfile_utils::*;
use postgres_ffi::xlog_utils::*;
use postgres_ffi::*;
use zenith_utils::lsn::Lsn;

pub struct Basebackup<'a> {
    ar: Builder<&'a mut dyn Write>,
    timeline: &'a Arc<dyn Timeline>,
    lsn: Lsn,
    snappath: String,
    slru_buf: [u8; pg_constants::SLRU_SEG_SIZE],
    slru_segno: u32,
    slru_path: &'static str,
}

impl<'a> Basebackup<'a> {
    pub fn new(
        write: &'a mut dyn Write,
        timelineid: ZTimelineId,
        timeline: &'a Arc<dyn Timeline>,
        lsn: Lsn,
        snapshot_lsn: Lsn,
    ) -> Basebackup<'a> {
        Basebackup {
            ar: Builder::new(write),
            timeline,
            lsn,
            snappath: format!("timelines/{}/snapshots/{:016X}", timelineid, snapshot_lsn.0),
            slru_path: "",
            slru_segno: u32::MAX,
            slru_buf: [0u8; pg_constants::SLRU_SEG_SIZE],
        }
    }

	#[rustfmt::skip]
    pub fn send_tarball(&mut self) -> anyhow::Result<()> {
        debug!("sending tarball of snapshot in {}", self.snappath);
        for entry in WalkDir::new(&self.snappath) {
            let entry = entry?;
            let fullpath = entry.path();
            let relpath = entry.path().strip_prefix(&self.snappath).unwrap();

            if relpath.to_str().unwrap() == "" {
                continue;
            }

            if entry.file_type().is_dir() {
                trace!(
                    "sending dir {} as {}",
                    fullpath.display(),
                    relpath.display()
                );
                self.ar.append_dir(relpath, fullpath)?;
            } else if entry.file_type().is_symlink() {
                error!("ignoring symlink in snapshot dir");
            } else if entry.file_type().is_file() {
                // Shared catalogs are exempt
                if relpath.starts_with("global/") {
                    trace!("sending shared catalog {}", relpath.display());
                    self.ar.append_path_with_name(fullpath, relpath)?;
                } else if !is_rel_file_path(relpath.to_str().unwrap()) {
                    if entry.file_name() != "pg_filenode.map"
                        && entry.file_name() != "pg_control"
                        && !relpath.starts_with("pg_xact/")
                        && !relpath.starts_with("pg_multixact/")
                    {
                        trace!("sending {}", relpath.display());
                        self.ar.append_path_with_name(fullpath, relpath)?;
                    }
                } else {
                    trace!("not sending {}", relpath.display());
                }
            } else {
                error!("unknown file type: {}", fullpath.display());
            }
        }

        for obj in self.timeline.list_nonrels(self.lsn)? {
            match obj {
                ObjectTag::Clog(slru) =>
					self.add_slru_segment("pg_xact", &obj, slru.blknum)?,
                ObjectTag::MultiXactMembers(slru) =>
                    self.add_slru_segment("pg_multixact/members", &obj, slru.blknum)?,
                ObjectTag::MultiXactOffsets(slru) =>
                    self.add_slru_segment("pg_multixact/offsets", &obj, slru.blknum)?,
                ObjectTag::FileNodeMap(db) =>
					self.add_relmap_file(&obj, &db)?,
                ObjectTag::TwoPhase(prepare) =>
					self.add_twophase_file(&obj, prepare.xid)?,
                _ => {}
            }
        }
        self.finish_slru_segment()?;
		self.add_pgcontrol_file()?;
        self.ar.finish()?;
        debug!("all tarred up!");
        Ok(())
    }

    //
    // Generate SRLU segment files from repository
    //
    fn add_slru_segment(
        &mut self,
        path: &'static str,
        tag: &ObjectTag,
        page: u32,
    ) -> anyhow::Result<()> {
        let img = self.timeline.get_page_at_lsn_nowait(*tag, self.lsn)?;
        // Zero length image indicates truncated segment: just skip it
        if !img.is_empty() {
            assert!(img.len() == pg_constants::BLCKSZ as usize);
            let segno = page / pg_constants::SLRU_PAGES_PER_SEGMENT;
            if self.slru_path != "" && (self.slru_segno != segno || self.slru_path != path) {
                let segname = format!("{}/{:>04X}", self.slru_path, self.slru_segno);
                let header = new_tar_header(&segname, pg_constants::SLRU_SEG_SIZE as u64)?;
                self.ar.append(&header, &self.slru_buf[..])?;
                self.slru_buf = [0u8; pg_constants::SLRU_SEG_SIZE];
            }
            self.slru_segno = segno;
            self.slru_path = path;
            let offs_start = (page % pg_constants::SLRU_PAGES_PER_SEGMENT) as usize
                * pg_constants::BLCKSZ as usize;
            let offs_end = offs_start + pg_constants::BLCKSZ as usize;
            self.slru_buf[offs_start..offs_end].copy_from_slice(&img);
        }
        Ok(())
    }

    fn finish_slru_segment(&mut self) -> anyhow::Result<()> {
        if self.slru_path != "" {
            let segname = format!("{}/{:>04X}", self.slru_path, self.slru_segno);
            let header = new_tar_header(&segname, pg_constants::SLRU_SEG_SIZE as u64)?;
            self.ar.append(&header, &self.slru_buf[..])?;
        }
        Ok(())
    }

    //
    // Extract pg_filenode.map files from repository
    //
    fn add_relmap_file(&mut self, tag: &ObjectTag, db: &DatabaseTag) -> anyhow::Result<()> {
        let img = self.timeline.get_page_at_lsn_nowait(*tag, self.lsn)?;
        info!("add_relmap_file {:?}", db);
        let path = if db.spcnode == pg_constants::GLOBALTABLESPACE_OID {
            String::from("global/pg_filenode.map")
        } else {
            // User defined tablespaces are not supported
            assert!(db.spcnode == pg_constants::DEFAULTTABLESPACE_OID);
            let src_path = format!("{}/base/1/PG_VERSION", self.snappath);
            let dst_path = format!("base/{}/PG_VERSION", db.dbnode);
            self.ar.append_path_with_name(&src_path, &dst_path)?;
            format!("base/{}/pg_filenode.map", db.dbnode)
        };
        assert!(img.len() == 512);
        let header = new_tar_header(&path, img.len() as u64)?;
        self.ar.append(&header, &img[..])?;
        Ok(())
    }

    //
    // Extract twophase state files
    //
    fn add_twophase_file(&mut self, tag: &ObjectTag, xid: TransactionId) -> anyhow::Result<()> {
        let img = self.timeline.get_page_at_lsn_nowait(*tag, self.lsn)?;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&img[..]);
        let crc = crc32c::crc32c(&img[..]);
        buf.put_u32_le(crc);
        let path = format!("pg_twophase/{:>08X}", xid);
        let header = new_tar_header(&path, buf.len() as u64)?;
        self.ar.append(&header, &buf[..])?;
        Ok(())
    }

    //
    // Add generated pg_control file
    //
    fn add_pgcontrol_file(&mut self) -> anyhow::Result<()> {
        let most_recent_lsn = Lsn(0);
        let checkpoint_bytes = self
            .timeline
            .get_page_at_lsn_nowait(ObjectTag::Checkpoint, most_recent_lsn)?;
        let pg_control_bytes = self
            .timeline
            .get_page_at_lsn_nowait(ObjectTag::ControlFile, most_recent_lsn)?;
        let mut pg_control = postgres_ffi::decode_pg_control(pg_control_bytes)?;
        let mut checkpoint = postgres_ffi::decode_checkpoint(checkpoint_bytes)?;
        // Here starts pg_resetwal inspired magic
        // Generate new pg_control and WAL needed for bootstrap
        let new_segno = self.lsn.segment_number(pg_constants::WAL_SEGMENT_SIZE) + 1;

        let new_lsn = XLogSegNoOffsetToRecPtr(
            new_segno,
            SizeOfXLogLongPHD as u32,
            pg_constants::WAL_SEGMENT_SIZE,
        );
        checkpoint.redo = new_lsn;

        //reset some fields we don't want to preserve
        checkpoint.oldestActiveXid = 0;

        //save new values in pg_control
        pg_control.checkPoint = new_lsn;
        pg_control.checkPointCopy = checkpoint;

        //send pg_control
        let pg_control_bytes = postgres_ffi::encode_pg_control(pg_control);
        let header = new_tar_header("global/pg_control", pg_control_bytes.len() as u64)?;
        self.ar.append(&header, &pg_control_bytes[..])?;

        //send wal segment
        let wal_file_name = XLogFileName(
            1, // FIXME: always use Postgres timeline 1
            new_segno,
            pg_constants::WAL_SEGMENT_SIZE,
        );
        let wal_file_path = format!("pg_wal/{}", wal_file_name);
        let header = new_tar_header(&wal_file_path, pg_constants::WAL_SEGMENT_SIZE as u64)?;

        let mut seg_buf = BytesMut::with_capacity(pg_constants::WAL_SEGMENT_SIZE as usize);

        let hdr = XLogLongPageHeaderData {
            std: {
                XLogPageHeaderData {
                    xlp_magic: XLOG_PAGE_MAGIC as u16,
                    xlp_info: pg_constants::XLP_LONG_HEADER,
                    xlp_tli: 1, // FIXME: always use Postgres timeline 1
                    xlp_pageaddr: pg_control.checkPointCopy.redo - SizeOfXLogLongPHD as u64,
                    xlp_rem_len: 0,
                }
            },
            xlp_sysid: pg_control.system_identifier,
            xlp_seg_size: pg_constants::WAL_SEGMENT_SIZE as u32,
            xlp_xlog_blcksz: XLOG_BLCKSZ as u32,
        };

        let hdr_bytes = encode_xlog_long_phd(hdr);
        seg_buf.extend_from_slice(&hdr_bytes);

        let rec_hdr = XLogRecord {
            xl_tot_len: (XLOG_SIZE_OF_XLOG_RECORD
                + SIZE_OF_XLOG_RECORD_DATA_HEADER_SHORT
                + SIZEOF_CHECKPOINT) as u32,
            xl_xid: 0, //0 is for InvalidTransactionId
            xl_prev: 0,
            xl_info: pg_constants::XLOG_CHECKPOINT_SHUTDOWN,
            xl_rmid: pg_constants::RM_XLOG_ID,
            xl_crc: 0,
        };

        let mut rec_shord_hdr_bytes = BytesMut::new();
        rec_shord_hdr_bytes.put_u8(pg_constants::XLR_BLOCK_ID_DATA_SHORT);
        rec_shord_hdr_bytes.put_u8(SIZEOF_CHECKPOINT as u8);

        let rec_bytes = encode_xlog_record(rec_hdr);
        let checkpoint_bytes = encode_checkpoint(pg_control.checkPointCopy);

        //calculate record checksum
        let mut crc = 0;
        crc = crc32c_append(crc, &rec_shord_hdr_bytes[..]);
        crc = crc32c_append(crc, &checkpoint_bytes[..]);
        crc = crc32c_append(crc, &rec_bytes[0..XLOG_RECORD_CRC_OFFS]);

        seg_buf.extend_from_slice(&rec_bytes[0..XLOG_RECORD_CRC_OFFS]);
        seg_buf.put_u32_le(crc);
        seg_buf.extend_from_slice(&rec_shord_hdr_bytes);
        seg_buf.extend_from_slice(&checkpoint_bytes);

        //zero out remainig file
        seg_buf.resize(pg_constants::WAL_SEGMENT_SIZE, 0);

        self.ar.append(&header, &seg_buf[..])?;
        Ok(())
    }
}

///
/// Parse a path, relative to the root of PostgreSQL data directory, as
/// a PostgreSQL relation data file.
///
fn parse_rel_file_path(path: &str) -> Result<(), FilePathError> {
    /*
     * Relation data files can be in one of the following directories:
     *
     * global/
     *		shared relations
     *
     * base/<db oid>/
     *		regular relations, default tablespace
     *
     * pg_tblspc/<tblspc oid>/<tblspc version>/
     *		within a non-default tablespace (the name of the directory
     *		depends on version)
     *
     * And the relation data files themselves have a filename like:
     *
     * <oid>.<segment number>
     */
    if let Some(fname) = path.strip_prefix("global/") {
        let (_relnode, _forknum, _segno) = parse_relfilename(fname)?;

        Ok(())
    } else if let Some(dbpath) = path.strip_prefix("base/") {
        let mut s = dbpath.split('/');
        let dbnode_str = s.next().ok_or(FilePathError::InvalidFileName)?;
        let _dbnode = dbnode_str.parse::<u32>()?;
        let fname = s.next().ok_or(FilePathError::InvalidFileName)?;
        if s.next().is_some() {
            return Err(FilePathError::InvalidFileName);
        };

        let (_relnode, _forknum, _segno) = parse_relfilename(fname)?;

        Ok(())
    } else if path.strip_prefix("pg_tblspc/").is_some() {
        // TODO
        error!("tablespaces not implemented yet");
        Err(FilePathError::InvalidFileName)
    } else {
        Err(FilePathError::InvalidFileName)
    }
}

fn is_rel_file_path(path: &str) -> bool {
    parse_rel_file_path(path).is_ok()
}

fn new_tar_header(path: &str, size: u64) -> anyhow::Result<Header> {
    let mut header = Header::new_gnu();
    header.set_size(size);
    header.set_path(path)?;
    header.set_mode(0b110000000);
    header.set_mtime(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    );
    header.set_cksum();
    Ok(header)
}
