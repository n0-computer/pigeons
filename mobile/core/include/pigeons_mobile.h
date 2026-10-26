/*
 * pigeons_mobile.h — C ABI of the pigeons iOS core (mobile/core).
 *
 * Hand-written to match mobile/core/src/ffi.rs; keep the two in step.
 *
 * Model:
 *   - one opaque pm_client, created with pm_client_new, released with
 *     pm_client_free;
 *   - a pm_callbacks table through which every asynchronous outcome arrives.
 *     Callbacks run on background threads; hop to the main thread yourself.
 *     Strings handed to callbacks are valid only during the callback;
 *   - request functions return a request id at once and complete through
 *     on_result with a JSON payload (or an error message when ok is false);
 *   - terminal functions are keyed by the terminal id they return.
 *
 * Strings passed in must be NUL-terminated UTF-8 and are only borrowed.
 * char * strings returned to you are yours; free them with pm_string_free.
 * No callback fires after pm_client_free returns.
 */
#ifndef PIGEONS_MOBILE_H
#define PIGEONS_MOBILE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct pm_client pm_client;

/* Values passed to on_state and returned by pm_client_state. */
enum {
    PM_STATE_DISCONNECTED = 0,
    PM_STATE_DIALING = 1,         /* dialing the roost over iroh */
    PM_STATE_AUTHENTICATING = 2,  /* SSH handshake and authentication */
    PM_STATE_CONNECTED = 3,
    PM_STATE_FAILED = 4           /* detail says why */
};

/* Values passed to on_log. */
enum {
    PM_LOG_ERROR = 1,
    PM_LOG_WARN = 2,
    PM_LOG_INFO = 3,
    PM_LOG_DEBUG = 4,
    PM_LOG_TRACE = 5
};

typedef struct pm_callbacks {
    /* Passed back as the first argument of every callback. */
    void *ctx;
    /* detail is nullable. */
    void (*on_state)(void *ctx, int32_t state, const char *detail);
    /* payload is JSON when ok, an error message otherwise. */
    void (*on_result)(void *ctx, uint64_t request_id, bool ok, const char *payload);
    void (*on_terminal_output)(void *ctx, uint64_t terminal_id, const uint8_t *bytes, size_t len);
    /* exit_code is -1 when unknown; error is nullable. */
    void (*on_terminal_exit)(void *ctx, uint64_t terminal_id, int32_t exit_code, const char *error);
    void (*on_log)(void *ctx, int32_t level, const char *message);
} pm_callbacks;

typedef struct pm_settings {
    const char *code_dir;     /* work.sh's $CODE_DIR on the roost; null keeps "~/code" */
    const char *work_script;  /* path of work.sh on the roost; null keeps "~/work.sh" */
    bool tmux_mouse;          /* set -g mouse on when attaching, so drags scroll */
} pm_settings;

/* Create a client. data_dir must be writable; the device key lives there.
 * Binds the iroh endpoint before returning, so call off the main thread.
 * Returns NULL and sets *error_out (free with pm_string_free) on failure. */
pm_client *pm_client_new(const char *data_dir, const pm_callbacks *callbacks, char **error_out);

/* Disconnect, stop the runtime, release. Blocks briefly. */
void pm_client_free(pm_client *client);

/* This device's iroh endpoint ID, hex. Free with pm_string_free. */
char *pm_client_endpoint_id(const pm_client *client);

/* The authorized_keys line for this device. Free with pm_string_free. */
char *pm_client_authorized_key(const pm_client *client);

void pm_client_set_settings(pm_client *client, const pm_settings *settings);

int32_t pm_client_state(const pm_client *client);

/* Connect to a roost (hex or z-base-32 endpoint ID) as user. password is
 * nullable and only tried when the device key is rejected. */
void pm_client_connect(pm_client *client, const char *roost_id, const char *user, const char *password);
void pm_client_disconnect(pm_client *client);

/* Payload: {"sessions":[{"name","project","tree","windows","attached",
 *                        "created","activity","path"}],"projects":["..."]} */
uint64_t pm_client_list_sessions(pm_client *client);

/* Payload: [{"name","path","branch","session"}] */
uint64_t pm_client_list_worktrees(pm_client *client, const char *project);

/* Branches a new worktree can start from, newest commit first, local and
 * origin merged. "default" is origin's HEAD branch (else main/master), null
 * when the project has neither.
 * Payload: {"default":"main"|null,"branches":["..."]} */
uint64_t pm_client_list_branches(pm_client *client, const char *project);

/* Files, over the SSH session's SFTP subsystem. Paths may start with "~".
 * resolve_path payload: {"path":"/abs/canonical"}
 * list_dir payload:     {"path":"/abs","entries":[{"name","kind":"dir|file|other",
 *                        "symlink":false,"size":0,"modified":null|seconds}]}
 * read_file payload:    {"path","size","truncated":bool,"binary":bool,"text"} */
uint64_t pm_client_resolve_path(pm_client *client, const char *path);
uint64_t pm_client_list_dir(pm_client *client, const char *path);
uint64_t pm_client_read_file(pm_client *client, const char *path, uint64_t max_bytes);

/* Payload for these three: {"exit_code":0,"stdout":"","stderr":""}
 * (exit_code is null when unknown). */
uint64_t pm_client_kill_session(pm_client *client, const char *session);
uint64_t pm_client_install_authorized_key(pm_client *client);
uint64_t pm_client_exec(pm_client *client, const char *command);

/* Terminals. Each returns a terminal id, or 0 on bad arguments. Output
 * arrives through on_terminal_output; on_terminal_exit fires once at the end. */
uint64_t pm_client_attach_session(pm_client *client, const char *session, uint16_t cols, uint16_t rows);
/* base (nullable) is the branch a brand-new branch starts from, passed to
 * work.sh as BASE; the script asks in the terminal when it is null and
 * ignores it when the branch already exists. */
uint64_t pm_client_open_worktree(pm_client *client, const char *project, const char *tree, const char *base, uint16_t cols, uint16_t rows);
uint64_t pm_client_open_shell(pm_client *client, uint16_t cols, uint16_t rows);

void pm_terminal_write(pm_client *client, uint64_t terminal_id, const uint8_t *bytes, size_t len);
void pm_terminal_resize(pm_client *client, uint64_t terminal_id, uint16_t cols, uint16_t rows);
void pm_terminal_close(pm_client *client, uint64_t terminal_id);

void pm_string_free(char *s);

/* Static; do not free. */
const char *pm_version(void);

#ifdef __cplusplus
}
#endif

#endif /* PIGEONS_MOBILE_H */
