#include "nostrdb.h"

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

struct bench_ndb {
    struct ndb *db;
};

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
