/* Side-by-side test: does a concurrent fork() break SQLite-style INSERTs?
 *
 * Pattern:
 *   - Thread A (writer): in a tight loop, INSERT a row through a persistent
 *     prepared statement. Count any SQLITE_BUSY / SQLITE_LOCKED returns.
 *   - Thread B (forker): in a tight loop, posix_spawn a `/bin/sleep 0.05`
 *     child and reap it. This mimics what
 *     `std::process::Command::new(...).spawn()` in Rust does.
 *   - Run for a fixed wall-clock window, then report the BUSY count.
 *
 * `run.sh` builds this twice from bundled amalgamations:
 *   - STOCK SQLITE     -> sqlite-amalgamation/sqlite3.c
 *   - DOLTLITE v0.11.5 -> doltlite-v0.11.5/doltlite.c
 *
 * Same C source linked against either library. If only one of them
 * hits SQLITE_BUSY, that's the bug.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <spawn.h>
#include <signal.h>
#include <sys/wait.h>
#include <sys/time.h>
#include <stdatomic.h>

#include <sqlite3.h>

extern char **environ;

static volatile sig_atomic_t g_stop = 0;
static sqlite3 *g_db = NULL;
static atomic_int g_inserts_ok = 0;
static atomic_int g_inserts_busy = 0;
static atomic_int g_inserts_other = 0;
static atomic_int g_forks = 0;

static unsigned long long now_us(void) {
    struct timeval tv; gettimeofday(&tv, NULL);
    return (unsigned long long)tv.tv_sec * 1000000ULL + tv.tv_usec;
}

static void *writer_thread(void *arg) {
    (void)arg;
    sqlite3_stmt *stmt = NULL;
    int rc = sqlite3_prepare_v2(
        g_db, "INSERT INTO t (v) VALUES (?)", -1, &stmt, NULL);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "prepare failed: %s\n", sqlite3_errmsg(g_db));
        return NULL;
    }
    while (!g_stop) {
        sqlite3_bind_text(stmt, 1, "x", -1, SQLITE_STATIC);
        rc = sqlite3_step(stmt);
        if (rc == SQLITE_DONE || rc == SQLITE_OK) {
            atomic_fetch_add(&g_inserts_ok, 1);
        } else if (rc == SQLITE_BUSY || rc == SQLITE_LOCKED) {
            atomic_fetch_add(&g_inserts_busy, 1);
        } else {
            atomic_fetch_add(&g_inserts_other, 1);
            fprintf(stderr, "insert rc=%d msg=%s\n", rc, sqlite3_errmsg(g_db));
        }
        sqlite3_reset(stmt);
    }
    sqlite3_finalize(stmt);
    return NULL;
}

static void *forker_thread(void *arg) {
    (void)arg;
    /* posix_spawn matches what Rust's std::process::Command::spawn() does
     * on macOS in the common case (no pre_exec, no chdir). */
    while (!g_stop) {
        pid_t pid;
        char *argv[] = { "sleep", "0.05", NULL };
        int rc = posix_spawn(&pid, "/bin/sleep", NULL, NULL, argv, environ);
        if (rc == 0) {
            atomic_fetch_add(&g_forks, 1);
            int status;
            waitpid(pid, &status, 0);
        } else {
            fprintf(stderr, "posix_spawn failed: %d\n", rc);
        }
        /* Small pause so we're not spawn-bombing. */
        usleep(2000);
    }
    return NULL;
}

int main(int argc, char **argv) {
    const char *db_path = (argc >= 2) ? argv[1] : "/tmp/fvd_test.db";
    double seconds = (argc >= 3) ? atof(argv[2]) : 3.0;

    /* Make sure we start from a fresh file. */
    unlink(db_path);

    int rc = sqlite3_open(db_path, &g_db);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "open: %s\n", sqlite3_errmsg(g_db));
        return 1;
    }

    /* No PRAGMAs and no WAL — match the frankweiler-sync open path
     * (no journal_mode override; doltlite rejects it anyway). */
    rc = sqlite3_exec(g_db,
        "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)",
        NULL, NULL, NULL);
    if (rc != SQLITE_OK) {
        fprintf(stderr, "create: %s\n", sqlite3_errmsg(g_db));
        return 1;
    }

    fprintf(stderr,
        "fork_vs_db: db=%s wall=%.1fs\n", db_path, seconds);

    pthread_t a, b;
    pthread_create(&a, NULL, writer_thread, NULL);
    pthread_create(&b, NULL, forker_thread, NULL);

    unsigned long long t0 = now_us();
    while ((now_us() - t0) < (unsigned long long)(seconds * 1e6)) {
        usleep(50000);
    }
    g_stop = 1;
    pthread_join(a, NULL);
    pthread_join(b, NULL);

    sqlite3_close(g_db);

    fprintf(stderr,
        "results: forks=%d inserts_ok=%d inserts_BUSY=%d inserts_other=%d\n",
        atomic_load(&g_forks),
        atomic_load(&g_inserts_ok),
        atomic_load(&g_inserts_busy),
        atomic_load(&g_inserts_other));
    return atomic_load(&g_inserts_busy) ? 1 : 0;
}
