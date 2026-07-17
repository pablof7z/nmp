#include "nostrdb.h"
#include "lmdb.h"

#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct bench_ndb {
    struct ndb *db;
};

#define BENCH_LMDB_TABLES 12

struct bench_lmdb {
    MDB_env *env;
    MDB_dbi tables[BENCH_LMDB_TABLES];
};

struct bench_lmdb_record {
    uint32_t table;
    const unsigned char *key;
    size_t key_len;
    const unsigned char *value;
    size_t value_len;
};

static const char *bench_lmdb_table_names[BENCH_LMDB_TABLES] = {
    "events_v6",
    "event_ids_v6",
    "event_observations_v6",
    "relays_v6",
    "relay_keys_v6",
    "relay_refs_v6",
    "by_created_at_v6",
    "by_author_time_v6",
    "by_kind_time_v6",
    "by_author_kind_time_v6",
    "by_tag_v6",
    "index_cardinality_v1",
};

void *bench_lmdb_open(const char *path, uint64_t mapsize, int *error_out) {
    struct bench_lmdb *handle = calloc(1, sizeof(*handle));
    MDB_txn *txn = NULL;
    int rc = MDB_SUCCESS;
    if (handle == NULL) {
        rc = ENOMEM;
        goto fail;
    }
    if ((rc = mdb_env_create(&handle->env)) != MDB_SUCCESS)
        goto fail;
    if ((rc = mdb_env_set_maxdbs(handle->env, BENCH_LMDB_TABLES)) != MDB_SUCCESS)
        goto fail;
    if ((rc = mdb_env_set_mapsize(handle->env, (size_t)mapsize)) != MDB_SUCCESS)
        goto fail;
    /* No MDB_NOSYNC/MDB_NOMETASYNC: commits use LMDB's synchronous default. */
    if ((rc = mdb_env_open(handle->env, path, 0, 0664)) != MDB_SUCCESS)
        goto fail;
    if ((rc = mdb_txn_begin(handle->env, NULL, 0, &txn)) != MDB_SUCCESS)
        goto fail;
    for (uint32_t i = 0; i < BENCH_LMDB_TABLES; i++) {
        rc = mdb_dbi_open(txn, bench_lmdb_table_names[i], MDB_CREATE,
                          &handle->tables[i]);
        if (rc != MDB_SUCCESS)
            goto fail;
    }
    if ((rc = mdb_txn_commit(txn)) != MDB_SUCCESS) {
        txn = NULL;
        goto fail;
    }
    if (error_out != NULL)
        *error_out = MDB_SUCCESS;
    return handle;

fail:
    if (txn != NULL)
        mdb_txn_abort(txn);
    if (handle != NULL) {
        if (handle->env != NULL)
            mdb_env_close(handle->env);
        free(handle);
    }
    if (error_out != NULL)
        *error_out = rc;
    return NULL;
}

void *bench_lmdb_begin(void *opaque, int *error_out) {
    struct bench_lmdb *handle = opaque;
    MDB_txn *txn = NULL;
    int rc;
    if (handle == NULL) {
        rc = EINVAL;
        goto fail;
    }
    if ((rc = mdb_txn_begin(handle->env, NULL, 0, &txn)) != MDB_SUCCESS)
        goto fail;
    if (error_out != NULL)
        *error_out = MDB_SUCCESS;
    return txn;

fail:
    if (error_out != NULL)
        *error_out = rc;
    return NULL;
}

int bench_lmdb_put_batch(void *opaque, void *txn_opaque,
                         const struct bench_lmdb_record *records,
                         size_t count) {
    struct bench_lmdb *handle = opaque;
    MDB_txn *txn = txn_opaque;
    int rc;
    if (handle == NULL || txn == NULL || records == NULL)
        return EINVAL;
    for (size_t i = 0; i < count; i++) {
        MDB_val key;
        MDB_val value;
        if (records[i].table >= BENCH_LMDB_TABLES) {
            return EINVAL;
        }
        key.mv_data = (void *)records[i].key;
        key.mv_size = records[i].key_len;
        value.mv_data = (void *)records[i].value;
        value.mv_size = records[i].value_len;
        rc = mdb_put(txn, handle->tables[records[i].table], &key, &value, 0);
        if (rc != MDB_SUCCESS)
            return rc;
    }
    return MDB_SUCCESS;
}

int bench_lmdb_commit(void *txn_opaque) {
    if (txn_opaque == NULL)
        return EINVAL;
    return mdb_txn_commit((MDB_txn *)txn_opaque);
}

void bench_lmdb_abort(void *txn_opaque) {
    if (txn_opaque != NULL)
        mdb_txn_abort((MDB_txn *)txn_opaque);
}

uint64_t bench_lmdb_count(void *opaque, uint32_t table, int *error_out) {
    struct bench_lmdb *handle = opaque;
    MDB_txn *txn = NULL;
    MDB_stat stat;
    int rc;
    if (handle == NULL || table >= BENCH_LMDB_TABLES) {
        rc = EINVAL;
        goto fail;
    }
    if ((rc = mdb_txn_begin(handle->env, NULL, MDB_RDONLY, &txn)) != MDB_SUCCESS)
        goto fail;
    if ((rc = mdb_stat(txn, handle->tables[table], &stat)) != MDB_SUCCESS)
        goto fail;
    mdb_txn_abort(txn);
    if (error_out != NULL)
        *error_out = MDB_SUCCESS;
    return (uint64_t)stat.ms_entries;

fail:
    if (txn != NULL)
        mdb_txn_abort(txn);
    if (error_out != NULL)
        *error_out = rc;
    return UINT64_MAX;
}

int bench_lmdb_has(void *opaque, uint32_t table,
                   const unsigned char *key_bytes, size_t key_len,
                   int *error_out) {
    struct bench_lmdb *handle = opaque;
    MDB_txn *txn = NULL;
    MDB_val key;
    MDB_val value;
    int rc;
    if (handle == NULL || table >= BENCH_LMDB_TABLES || key_bytes == NULL) {
        rc = EINVAL;
        goto fail;
    }
    if ((rc = mdb_txn_begin(handle->env, NULL, MDB_RDONLY, &txn)) != MDB_SUCCESS)
        goto fail;
    key.mv_data = (void *)key_bytes;
    key.mv_size = key_len;
    rc = mdb_get(txn, handle->tables[table], &key, &value);
    mdb_txn_abort(txn);
    txn = NULL;
    if (rc == MDB_NOTFOUND) {
        if (error_out != NULL)
            *error_out = MDB_SUCCESS;
        return 0;
    }
    if (rc != MDB_SUCCESS)
        goto fail;
    if (error_out != NULL)
        *error_out = MDB_SUCCESS;
    return 1;

fail:
    if (txn != NULL)
        mdb_txn_abort(txn);
    if (error_out != NULL)
        *error_out = rc;
    return -1;
}

const char *bench_lmdb_error(int rc) {
    return mdb_strerror(rc);
}

void bench_lmdb_close(void *opaque) {
    struct bench_lmdb *handle = opaque;
    if (handle == NULL)
        return;
    mdb_env_close(handle->env);
    free(handle);
}

void *bench_ndb_open(const char *path, uint64_t mapsize, int ingest_threads) {
    struct bench_ndb *handle = calloc(1, sizeof(*handle));
    struct ndb_config config;
    if (handle == NULL)
        return NULL;

    ndb_default_config(&config);
    ndb_config_set_mapsize(&config, (size_t)mapsize);
    ndb_config_set_ingest_threads(&config, ingest_threads);
    ndb_config_set_flags(&config,
        NDB_FLAG_NO_FULLTEXT | NDB_FLAG_NO_NOTE_BLOCKS | NDB_FLAG_NO_STATS);
    if (!ndb_init(&handle->db, path, &config)) {
        free(handle);
        return NULL;
    }
    return handle;
}

int bench_ndb_ingest(void *opaque, const char *jsonl, uint64_t len) {
    struct bench_ndb *handle = opaque;
    return handle != NULL && ndb_process_events(handle->db, jsonl, (size_t)len);
}

void bench_ndb_close(void *opaque) {
    struct bench_ndb *handle = opaque;
    if (handle == NULL)
        return;
    ndb_destroy(handle->db);
    free(handle);
}

int bench_ndb_query(void *opaque, const char *filter_json, int capacity,
                    unsigned char *ids, uint32_t *created_at, int *count) {
    struct bench_ndb *handle = opaque;
    struct ndb_query_result *results = NULL;
    struct ndb_filter filter;
    struct ndb_txn txn;
    unsigned char parse_buffer[8192];
    int ok = 0;

    if (handle == NULL || capacity <= 0 || count == NULL)
        return 0;
    if (!ndb_filter_init(&filter))
        return 0;
    if (!ndb_filter_from_json(filter_json, (int)strlen(filter_json), &filter,
                              parse_buffer, sizeof(parse_buffer)))
        goto cleanup_filter;
    results = calloc((size_t)capacity, sizeof(*results));
    if (results == NULL)
        goto cleanup_filter;
    if (!ndb_begin_query(handle->db, &txn))
        goto cleanup_results;
    if (!ndb_query(&txn, &filter, 1, results, capacity, count)) {
        ndb_end_query(&txn);
        goto cleanup_results;
    }
    for (int i = 0; i < *count; i++) {
        memcpy(ids + (size_t)i * 32, ndb_note_id(results[i].note), 32);
        created_at[i] = ndb_note_created_at(results[i].note);
    }
    ndb_end_query(&txn);
    ok = 1;

cleanup_results:
    free(results);
cleanup_filter:
    ndb_filter_destroy(&filter);
    return ok;
}

uint64_t bench_ndb_note_count(void *opaque) {
    struct bench_ndb *handle = opaque;
    struct ndb_stat stat;
    if (handle == NULL || !ndb_stat(handle->db, &stat))
        return UINT64_MAX;
    return (uint64_t)stat.dbs[NDB_DB_NOTE].count;
}
