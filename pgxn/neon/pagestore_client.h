/*-------------------------------------------------------------------------
 *
 * pagestore_client.h
 *
 *
 * Portions Copyright (c) 1996-2021, PostgreSQL Global Development Group
 * Portions Copyright (c) 1994, Regents of the University of California
 *
 * contrib/neon/pagestore_client.h
 *
 *-------------------------------------------------------------------------
 */
#ifndef pageserver_h
#define pageserver_h

#include "postgres.h"

#include "access/slru.h"
#include "access/xlogdefs.h"
#include "storage/relfilenode.h"
#include "storage/block.h"
#include "storage/smgr.h"
#include "lib/stringinfo.h"
#include "libpq/pqformat.h"
#include "utils/memutils.h"

#include "pg_config.h"

typedef enum
{
	/* pagestore_client -> pagestore */
	T_NeonExistsRequest = 0,
	T_NeonNblocksRequest,
	T_NeonGetPageRequest,
	T_NeonDbSizeRequest,
	T_NeonGetSlruPageRequest,

	/* pagestore -> pagestore_client */
	T_NeonExistsResponse = 100,
	T_NeonNblocksResponse,
	T_NeonGetPageResponse,
	T_NeonGetSlruPageResponse,
	T_NeonErrorResponse,
	T_NeonDbSizeResponse,
}			NeonMessageTag;



/* base struct for c-style inheritance */
typedef struct
{
	NeonMessageTag tag;
}			NeonMessage;

#define messageTag(m) (((const NeonMessage *)(m))->tag)

/*
 * supertype of all the Neon*Request structs below
 *
 * If 'latest' is true, we are requesting the latest page version, and 'lsn'
 * is just a hint to the server that we know there are no versions of the page
 * (or relation size, for exists/nblocks requests) later than the 'lsn'.
 */
typedef struct
{
	NeonMessageTag tag;
	bool		latest;			/* if true, request latest page version */
	XLogRecPtr	lsn;			/* request page version @ this LSN */
	int			region;			/* region to fetch page from */
}			NeonRequest;

typedef struct
{
	NeonRequest req;
	RelFileNode rnode;
	ForkNumber	forknum;
}			NeonExistsRequest;

typedef struct
{
	NeonRequest req;
	RelFileNode rnode;
	ForkNumber	forknum;
}			NeonNblocksRequest;

typedef struct
{
	NeonRequest req;
	Oid			dbNode;
}			NeonDbSizeRequest;

typedef struct
{
	NeonRequest req;
	RelFileNode rnode;
	ForkNumber	forknum;
	BlockNumber blkno;
}			NeonGetPageRequest;

typedef enum
{
	NEON_CLOG = 0,
	NEON_MULTI_XACT_MEMBERS,
	NEON_MULTI_XACT_OFFSETS,
	NEON_CSNLOG,
} NeonSlruKind;

typedef struct
{
	NeonRequest req;
	NeonSlruKind kind;
	int segno;
	BlockNumber blkno;
	bool check_exists_only;
} NeonGetSlruPageRequest;

/* supertype of all the Neon*Response structs below */
typedef struct
{
	NeonMessageTag tag;
}			NeonResponse;

typedef struct
{
	NeonMessageTag tag;
	XLogRecPtr	lsn;
	bool		exists;
}			NeonExistsResponse;

typedef struct
{
	NeonMessageTag tag;
	XLogRecPtr	lsn;
	uint32		n_blocks;
}			NeonNblocksResponse;

typedef struct
{
	NeonMessageTag tag;
	XLogRecPtr	lsn;
	char		page[FLEXIBLE_ARRAY_MEMBER];
}			NeonGetPageResponse;

typedef struct
{
	NeonMessageTag tag;
	int64		db_size;
}			NeonDbSizeResponse;

typedef struct
{
	NeonMessageTag tag;
	XLogRecPtr	lsn;
	bool		seg_exists;
	bool		page_exists;
	char		page[FLEXIBLE_ARRAY_MEMBER];
} NeonGetSlruPageResponse;

typedef struct
{
	NeonMessageTag tag;
	char		message[FLEXIBLE_ARRAY_MEMBER]; /* null-terminated error
												 * message */
}			NeonErrorResponse;

extern StringInfoData nm_pack_request(NeonRequest * msg);
extern NeonResponse * nm_unpack_response(StringInfo s);
extern char *nm_to_string(NeonMessage * msg);

/*
 * API
 */

typedef struct
{
	NeonResponse *(*request) (NeonRequest * request);
	void		(*send) (NeonRequest * request);
	NeonResponse *(*receive) (void);
	void		(*flush) (void);
}			page_server_api;

extern page_server_api * page_server;

extern char *page_server_connstring;
extern char *neon_timeline;
extern char *neon_tenant;
extern bool wal_redo;
extern int32 max_cluster_size;
extern bool neon_slru_clog;
extern bool neon_slru_multixact;
extern bool neon_slru_csnlog;

extern const f_smgr *smgr_neon(BackendId backend, RelFileNode rnode);
extern void smgr_init_neon(void);

extern const f_smgr *smgr_inmem(BackendId backend, RelFileNode rnode);
extern void smgr_init_inmem(void);
extern void smgr_shutdown_inmem(void);

/* Neon storage manager functionality */

extern void neon_init(void);
extern void neon_open(SMgrRelation reln);
extern void neon_close(SMgrRelation reln, ForkNumber forknum);
extern void neon_create(SMgrRelation reln, ForkNumber forknum, bool isRedo);
extern bool neon_exists(SMgrRelation reln, ForkNumber forknum);
extern void neon_unlink(RelFileNodeBackend rnode, ForkNumber forknum, bool isRedo);
extern void neon_extend(SMgrRelation reln, ForkNumber forknum,
						BlockNumber blocknum, char *buffer, bool skipFsync);
extern bool neon_prefetch(SMgrRelation reln, ForkNumber forknum,
						  BlockNumber blocknum);
extern void neon_reset_prefetch(SMgrRelation reln);
extern void neon_read(SMgrRelation reln, ForkNumber forknum, BlockNumber blocknum,
					  char *buffer);

extern void neon_read_at_lsn(RelFileNode rnode, ForkNumber forkNum, BlockNumber blkno,
							 XLogRecPtr request_lsn, bool request_latest, char *buffer);

extern void neon_write(SMgrRelation reln, ForkNumber forknum,
					   BlockNumber blocknum, char *buffer, bool skipFsync);
extern void neon_writeback(SMgrRelation reln, ForkNumber forknum,
						   BlockNumber blocknum, BlockNumber nblocks);
extern BlockNumber neon_nblocks(SMgrRelation reln, ForkNumber forknum);
extern int64 neon_dbsize(Oid dbNode);
extern void neon_truncate(SMgrRelation reln, ForkNumber forknum,
						  BlockNumber nblocks);
extern void neon_immedsync(SMgrRelation reln, ForkNumber forknum);

/* neon wal-redo storage manager functionality */

extern void inmem_init(void);
extern void inmem_open(SMgrRelation reln);
extern void inmem_close(SMgrRelation reln, ForkNumber forknum);
extern void inmem_create(SMgrRelation reln, ForkNumber forknum, bool isRedo);
extern bool inmem_exists(SMgrRelation reln, ForkNumber forknum);
extern void inmem_unlink(RelFileNodeBackend rnode, ForkNumber forknum, bool isRedo);
extern void inmem_extend(SMgrRelation reln, ForkNumber forknum,
						 BlockNumber blocknum, char *buffer, bool skipFsync);
extern bool inmem_prefetch(SMgrRelation reln, ForkNumber forknum,
						   BlockNumber blocknum);
extern void inmem_read(SMgrRelation reln, ForkNumber forknum, BlockNumber blocknum,
					   char *buffer);
extern void inmem_write(SMgrRelation reln, ForkNumber forknum,
						BlockNumber blocknum, char *buffer, bool skipFsync);
extern void inmem_writeback(SMgrRelation reln, ForkNumber forknum,
							BlockNumber blocknum, BlockNumber nblocks);
extern BlockNumber inmem_nblocks(SMgrRelation reln, ForkNumber forknum);
extern void inmem_truncate(SMgrRelation reln, ForkNumber forknum,
						   BlockNumber nblocks);
extern void inmem_immedsync(SMgrRelation reln, ForkNumber forknum);

/* utils for neon relsize cache */
extern void relsize_hash_init(void);
extern bool get_cached_relsize(RelFileNode rnode, ForkNumber forknum, BlockNumber *size);
extern void set_cached_relsize(RelFileNode rnode, ForkNumber forknum, BlockNumber size);
extern void update_cached_relsize(RelFileNode rnode, ForkNumber forknum, BlockNumber size);
extern void forget_cached_relsize(RelFileNode rnode, ForkNumber forknum);

/* neon SLRU functionality */
extern const char *slru_kind_to_string(NeonSlruKind kind);
extern bool slru_kind_from_string(const char* str, NeonSlruKind* kind);
extern bool neon_slru_kind_check(SlruCtl ctl);
extern bool neon_slru_read_page(SlruCtl ctl, int segno, off_t offset, char *buffer);
extern bool neon_slru_page_exists(SlruCtl ctl, int segno, off_t offset);

#endif
