#import <stdint.h>

/// Start the server from a JSON config. Returns a JSON string the caller owns;
/// release it with `stemd_ios_free_string`.
char * _Nullable stemd_ios_start(const char * _Nullable config_json);

/// Stop the server and the separation worker. Safe when nothing is running.
void stemd_ios_stop(void);

/// Release a string returned by `stemd_ios_start`.
void stemd_ios_free_string(char * _Nullable ptr);

/// Install the log subscriber. Optional; `stemd_ios_start` does it too.
void stemd_ios_init_logging(void);

/// Require (1) or stop requiring (0) the share token. Takes effect
/// immediately, for clients that cannot send one.
void stemd_ios_set_token_required(int required);

/// Save (1) or stop saving (0) the pre-separation mix for new jobs. Takes
/// effect on the next job.
void stemd_ios_set_save_pcm(int save);
